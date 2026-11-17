//! Telegram transport (GATE-006): the plumbing every control-plane task
//! builds on. One long-poll client against the Telegram Bot API, an
//! allowlist filter, signed callback validation, and a boxed-handler
//! router.
//!
//! Design rulings (GATE-006):
//! - **The API base URL lives only here.** `https://api.telegram.org` is a
//!   module-private literal inside [`TelegramClient`]; tests never touch
//!   the real client — they inject [`Arc<dyn TelegramApi>`] doubles (see
//!   `fake.rs`, compiled only under `test` / the `test-util` feature).
//! - **Empty allowlist ⇒ loop still runs but drops everything.** Spawning
//!   whenever `TELEGRAM_BOT_TOKEN` is set keeps behaviour uniform (offsets
//!   keep advancing so stranger updates do not pile up) and fails closed:
//!   with no allowlisted approver, no update is ever delivered or answered.
//! - **dyn-safe trait.** Later tasks inject `Arc<dyn TelegramApi>`; the
//!   router and poll loop are generic over the trait object only.
//! - **Sends are replies.** `SendRequest.reply_to` doubles as the target
//!   chat for `sendMessage` — control-plane traffic always answers inside
//!   the thread the update came from.
//! - **Callbacks are answered both ways.** The transport stops the
//!   client-side spinner (`answerCallbackQuery`) whether the nonce
//!   validates or not; handlers see only validated callbacks.
//!
//! Non-allowlisted updates are dropped with NO reply of any kind: not
//! answered, not edited, not deleted — the gate simply never acknowledges
//! strangers.

pub mod send;

#[cfg(any(test, feature = "test-util"))]
pub mod fake;

pub use self::send::{callback_data, MsgRef, SendRequest};

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tokio::sync::{mpsc, watch};

/// Long-poll window passed to Telegram's `getUpdates` (seconds). The API
/// holds the request open for this long before returning an empty result.
const TELEGRAM_LONG_POLL_SECS: i64 = 50;
/// Hard reqwest timeout per call — comfortably above the long-poll window.
const TELEGRAM_HTTP_TIMEOUT: Duration = Duration::from_secs(60);
/// Bot API base URL. Literal lives ONLY in this module (see module docs).
const TELEGRAM_API_BASE: &str = "https://api.telegram.org";

/// Default first-error delay for [`run_poll_loop`]: 1s, 2s, 4s … capped at
/// [`POLL_BACKOFF_CAP`]. Tests inject a smaller base.
pub const POLL_BACKOFF_BASE: Duration = Duration::from_secs(1);
/// Ceiling for the poll loop's exponential backoff.
pub const POLL_BACKOFF_CAP: Duration = Duration::from_secs(60);

/// Reply pushed under a stranger's or expired flow's button: the action
/// references state that no longer matches the flow's stored nonce.
pub const ACTION_NO_LONGER_VALID: &str = "This action is no longer valid";

/// The Telegram Bot API surface the control plane uses (GATE-006).
/// Dyn-safe (`Arc<dyn TelegramApi>`): the real transport is
/// [`TelegramClient`]; tests inject `fake::FakeTelegram`.
#[async_trait]
pub trait TelegramApi: Send + Sync {
    /// Long poll: request `timeout=50` against `getUpdates`; the HTTP
    /// timeout sits above the poll window.
    async fn get_updates(&self, offset: i64) -> Result<Vec<Update>>;
    /// Send one message (target chat comes from `req.reply_to`).
    async fn send_message(&self, req: SendRequest) -> Result<MsgRef>;
    /// Edit an existing message in place; returns the message ref.
    async fn edit_message(&self, to: MsgRef, req: SendRequest) -> Result<MsgRef>;
    /// Stop the client-side spinner on a pressed button.
    async fn answer_callback(&self, callback_id: &str) -> Result<()>;
    async fn delete_message(&self, to: MsgRef) -> Result<()>;
}

/// One inbound Telegram update, narrowed to what the control plane needs
/// from the Bot API JSON (`message.{message_id,chat.id,from.id,text}`,
/// `callback_query.{id,data,from.id,message}`). Unknown fields are
/// ignored; unknown update kinds parse as an id-only update.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Update {
    pub update_id: i64,
    /// `message.from.id` when the update is a message.
    pub sender_id: Option<i64>,
    pub text: Option<String>,
    /// `callback_query` payload when the update is a button press.
    pub callback: Option<Callback>,
    /// Address of the message (or button message) behind the update.
    pub message_ref: Option<MsgRef>,
}

/// A pressed inline button: its query id (for `answerCallbackQuery`), the
/// signed data triple, and the presser's user id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Callback {
    pub id: String,
    pub data: String,
    pub from_id: i64,
}

impl Update {
    /// Lenient parse of one `result[]` element from `getUpdates`. Returns
    /// `None` only when even `update_id` is missing (the one field the
    /// poll loop cannot do without).
    pub fn parse(value: &Value) -> Option<Update> {
        let update_id = value.get("update_id")?.as_i64()?;
        let mut update = Update {
            update_id,
            ..Default::default()
        };
        if let Some(message) = value.get("message") {
            update.sender_id = message.pointer("/from/id").and_then(Value::as_i64);
            update.text = message.get("text").and_then(Value::as_str).map(String::from);
            let chat_id = message.pointer("/chat/id").and_then(Value::as_i64);
            let message_id = message.get("message_id").and_then(Value::as_i64);
            if let (Some(chat_id), Some(message_id)) = (chat_id, message_id) {
                update.message_ref = Some(MsgRef {
                    chat_id,
                    message_id: message_id as i32,
                });
            }
        }
        if let Some(query) = value.get("callback_query") {
            let id = query.get("id").and_then(Value::as_str);
            let data = query.get("data").and_then(Value::as_str);
            let from_id = query.pointer("/from/id").and_then(Value::as_i64);
            if let (Some(id), Some(data), Some(from_id)) = (id, data, from_id) {
                update.callback = Some(Callback {
                    id: id.to_string(),
                    data: data.to_string(),
                    from_id,
                });
                let chat_id = query.pointer("/message/chat/id").and_then(Value::as_i64);
                let message_id = query.pointer("/message/message_id").and_then(Value::as_i64);
                if let (Some(chat_id), Some(message_id)) = (chat_id, message_id) {
                    update.message_ref = Some(MsgRef {
                        chat_id,
                        message_id: message_id as i32,
                    });
                }
            }
        }
        Some(update)
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 16 random lowercase hex characters — the per-flow nonce baked into
/// signed callback buttons and stored with the flow (Task 2's store).
pub fn nonce() -> String {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("entropy source failed");
    hex_encode(&bytes)
}

/// Validate a signed `callback_data` triple against the flow's stored
/// nonce: split on `:`, constant-time compare the nonce, return
/// `(flow_id, action)` — or `None` on any mismatch (wrong length, wrong
/// nonce, missing parts). `expected_nonce` comes from the caller's flow
/// record; there is no flow store yet (Task 2 adds it).
pub fn validate_callback(data: &str, expected_nonce: &str) -> Option<(String, String)> {
    let mut parts = data.splitn(3, ':');
    let flow_id = parts.next()?.to_string();
    let nonce_part = parts.next()?;
    let action = parts.next()?.to_string();
    if flow_id.is_empty() || action.is_empty() {
        return None;
    }
    // Length is compared in the clear (it leaks nothing the format doesn't
    // already show); the value comparison is constant-time.
    if nonce_part.len() != expected_nonce.len() {
        return None;
    }
    if !bool::from(nonce_part.as_bytes().ct_eq(expected_nonce.as_bytes())) {
        return None;
    }
    Some((flow_id, action))
}

/// The user id the allowlist judges: the message sender, or the button
/// presser for callback updates.
pub fn sender_of(update: &Update) -> Option<i64> {
    update
        .sender_id
        .or_else(|| update.callback.as_ref().map(|c| c.from_id))
}

/// Allowlist filter (GATE-006): true only when the update's sender is on
/// the list. An empty allowlist rejects everything — fail closed.
pub fn is_allowed(allowlist: &[i64], update: &Update) -> bool {
    sender_of(update).is_some_and(|id| allowlist.contains(&id))
}

/// Backoff delay after the `failure_count`-th consecutive error:
/// `base * 2^(n-1)`, capped at [`POLL_BACKOFF_CAP`].
fn backoff_delay(failure_count: u32, base: Duration) -> Duration {
    let base = base.min(POLL_BACKOFF_CAP);
    let shift = failure_count.saturating_sub(1).min(6);
    base.saturating_mul(1u32 << shift).min(POLL_BACKOFF_CAP)
}

/// Long-poll loop (GATE-006): `getUpdates(offset = last_update_id + 1)`,
/// allowlist filter FIRST, then hand every surviving update to the router
/// over `tx`. Never panics: transport errors back off exponentially
/// (1s, 2s, 4s… cap 60s) and a dead router ends the loop cleanly. Exits
/// when `shutdown` flips or its sender drops.
pub async fn run_poll_loop(
    api: Arc<dyn TelegramApi>,
    allowlist: Vec<i64>,
    tx: mpsc::Sender<Update>,
    mut shutdown: watch::Receiver<bool>,
    backoff_base: Duration,
) {
    let mut offset: i64 = 0;
    let mut failures: u32 = 0;
    // Mark the initial value seen so `changed()` only fires on a real flip
    // (a fresh receiver treats the initial value as an unseen change).
    shutdown.borrow_and_update();
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            fetched = api.get_updates(offset) => match fetched {
                Ok(updates) => {
                    failures = 0;
                    for update in updates {
                        // Offset advances for EVERY update, allowed or not:
                        // strangers must not be re-fetched forever.
                        offset = offset.max(update.update_id.saturating_add(1));
                        if is_allowed(&allowlist, &update) {
                            if tx.send(update).await.is_err() {
                                // Router is gone; the loop has no consumer.
                                return;
                            }
                        }
                    }
                    // Park between polls: a fast API (or test double) may
                    // resolve synchronously, and spinning inside one poll
                    // would starve the runtime.
                    tokio::task::yield_now().await;
                }
                Err(e) => {
                    failures = failures.saturating_add(1);
                    let delay = backoff_delay(failures, backoff_base);
                    eprintln!("telegram: get_updates failed ({e}); retrying in {delay:?}");
                    tokio::select! {
                        _ = shutdown.changed() => return,
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
            }
        }
    }
}

/// A registered update handler. Later control-plane tasks (2–5) register
/// these on the [`TelegramRouter`]; a handler sees only allowlisted
/// updates, and only validated callbacks.
#[async_trait]
pub trait UpdateHandler: Send + Sync {
    async fn handle(&self, update: Update, api: Arc<dyn TelegramApi>);
}

/// Handler adapter for plain closures (Tasks 2–5 wiring convenience).
pub struct FnHandler<
    F: Fn(Update, Arc<dyn TelegramApi>) -> Pin<Box<dyn Future<Output = ()> + Send>>
        + Send
        + Sync,
>(pub F);

#[async_trait]
impl<F> UpdateHandler for FnHandler<F>
where
    F: Fn(Update, Arc<dyn TelegramApi>) -> Pin<Box<dyn Future<Output = ()> + Send>>
        + Send
        + Sync,
{
    async fn handle(&self, update: Update, api: Arc<dyn TelegramApi>) {
        (self.0)(update, api).await;
    }
}

/// Closure resolving a flow id to its stored nonce (`None` = unknown
/// flow). Task 2 backs this with the flow store; tests back it with
/// literals. Unknown flows validate as expired — fail closed.
pub type NonceLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Boxed-handler registry (GATE-006): text updates go straight to every
/// handler; callback updates are nonce-validated first — a wrong, missing,
/// or unknown nonce gets `answerCallbackQuery` + a "no longer valid" edit
/// and NEVER reaches a handler.
#[derive(Default)]
pub struct TelegramRouter {
    handlers: Vec<Arc<dyn UpdateHandler>>,
    nonce_of: Option<NonceLookup>,
}

impl TelegramRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a handler; every handler sees every surviving update.
    pub fn register(&mut self, handler: Arc<dyn UpdateHandler>) {
        self.handlers.push(handler);
    }

    /// Install the flow→nonce resolver used to validate callbacks. Absent
    /// (or rejecting) lookups reject the callback — fail closed.
    pub fn set_nonce_lookup(&mut self, nonce_of: NonceLookup) {
        self.nonce_of = Some(nonce_of);
    }

    /// Consume updates from the poll loop until the channel closes.
    pub async fn run(self, mut rx: mpsc::Receiver<Update>, api: Arc<dyn TelegramApi>) {
        while let Some(update) = rx.recv().await {
            self.dispatch(&update, &api).await;
        }
    }

    /// Route one update through the filter-and-validate pipeline.
    pub async fn dispatch(&self, update: &Update, api: &Arc<dyn TelegramApi>) {
        let Some(callback) = &update.callback else {
            // Plain text: no signature to check, the allowlist already ran.
            for handler in &self.handlers {
                handler.handle(update.clone(), api.clone()).await;
            }
            return;
        };
        // Resolve the flow's nonce, then constant-time validate. Unknown
        // flows resolve to None ⇒ rejected — fail closed.
        let expected = self.nonce_of.as_ref().and_then(|lookup| {
            let flow_guess = callback.data.split(':').next().unwrap_or("");
            lookup(flow_guess)
        });
        match expected.and_then(|n| validate_callback(&callback.data, &n)) {
            Some(_) => {
                // Stop the client-side spinner before handlers run; the
                // press itself was valid even if the action later fails.
                if let Err(e) = api.answer_callback(&callback.id).await {
                    eprintln!("telegram: answer_callback failed: {e}");
                }
                for handler in &self.handlers {
                    handler.handle(update.clone(), api.clone()).await;
                }
            }
            None => {
                // Wrong/expired nonce: answer the query and mark the
                // button dead; NO handler ever sees it.
                if let Err(e) = api.answer_callback(&callback.id).await {
                    eprintln!("telegram: answer_callback failed: {e}");
                }
                if let Some(to) = update.message_ref {
                    let reply = SendRequest::text(ACTION_NO_LONGER_VALID).replying_to(to);
                    if let Err(e) = api.edit_message(to, reply).await {
                        eprintln!("telegram: invalid-callback edit failed: {e}");
                    }
                }
            }
        }
    }
}

/// Real transport against the Telegram Bot API. Long-polls with
/// `timeout=50`; every call carries a 60s HTTP timeout. The bot token is
/// embedded in the URL path (Telegram's scheme) and never logged: every
/// error leaving this client is rewritten to a token-free summary (see
/// [`redacted_error`]).
pub struct TelegramClient {
    http: reqwest::Client,
    base: String,
}

/// Classify a reqwest transport error into a token-free kind word. Reads
/// ONLY the error kind probes — never `Display`/source text, which embeds
/// the full request URL (`…/bot<TOKEN>/…`).
fn transport_kind(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timed out"
    } else if e.is_connect() {
        "connection failed"
    } else if e.is_redirect() {
        "redirect loop"
    } else if e.is_body() || e.is_decode() {
        "bad response"
    } else {
        "network error"
    }
}

/// Token-safe error for ANY transport failure: reqwest error Display
/// embeds the request URL, and the URL path carries the bot token, so raw
/// error text must never reach a log line. Only the Bot API method name
/// and the error kind survive (e.g. `getUpdates: transport timed out`).
fn redacted_error(method: &str, e: &reqwest::Error) -> anyhow::Error {
    anyhow!("{method}: transport {}", transport_kind(e))
}

impl TelegramClient {
    pub fn new(token: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(TELEGRAM_HTTP_TIMEOUT)
            .build()
            .expect("reqwest client with rustls backend");
        Self {
            http,
            base: format!("{TELEGRAM_API_BASE}/bot{token}"),
        }
    }

    fn url(&self, method: &str) -> String {
        format!("{}/{}", self.base, method)
    }

    /// POST one Bot API call and unwrap the `{ok: true, result}` envelope.
    /// Transport failures are rewritten to token-free summaries before
    /// they escape ([`redacted_error`]); the only free text ever included
    /// in an error is Telegram's own response `description` — the token
    /// lives in the request URL, never in a response body.
    async fn call(&self, method: &str, body: Value) -> Result<Value> {
        let response = self
            .http
            .post(self.url(method))
            .json(&body)
            .send()
            .await
            .map_err(|e| redacted_error(method, &e))?;
        let status = response.status();
        let payload: Value = response
            .json()
            .await
            .map_err(|e| redacted_error(method, &e))?;
        if payload.get("ok").and_then(Value::as_bool) != Some(true) {
            let description = payload
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("unknown telegram error");
            return Err(anyhow!("{method}: status {status} ({description})"));
        }
        Ok(payload)
    }

    /// `MsgRef` out of a Bot API `result` message object (lenient).
    fn parse_message_ref(result: &Value) -> Option<MsgRef> {
        Some(MsgRef {
            chat_id: result.pointer("/chat/id")?.as_i64()?,
            message_id: result.get("message_id")?.as_i64()? as i32,
        })
    }

    /// `sendMessage` body: target chat from the reply anchor, optional
    /// keyboard, optional reply pin.
    fn send_body(req: &SendRequest, chat_id: i64) -> Value {
        let mut body = json!({ "chat_id": chat_id, "text": req.text });
        if let Some(markup) = req.reply_markup() {
            body["reply_markup"] = markup;
        }
        if let Some(reply) = req.reply_to {
            body["reply_to_message_id"] = json!(reply.message_id);
        }
        body
    }
}

#[async_trait]
impl TelegramApi for TelegramClient {
    async fn get_updates(&self, offset: i64) -> Result<Vec<Update>> {
        let payload = self
            .call(
                "getUpdates",
                json!({ "offset": offset, "timeout": TELEGRAM_LONG_POLL_SECS }),
            )
            .await?;
        let items = payload
            .get("result")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // Lenient: skip unparseable shapes rather than failing the batch.
        Ok(items.iter().filter_map(Update::parse).collect())
    }

    async fn send_message(&self, req: SendRequest) -> Result<MsgRef> {
        let Some(reply) = req.reply_to else {
            return Err(anyhow!(
                "send_message needs a target chat: set SendRequest.reply_to to the message being replied to"
            ));
        };
        let body = Self::send_body(&req, reply.chat_id);
        let payload = self.call("sendMessage", body).await?;
        Self::parse_message_ref(payload.get("result").unwrap_or(&Value::Null))
            .ok_or_else(|| anyhow!("telegram sendMessage returned no message ref"))
    }

    async fn edit_message(&self, to: MsgRef, req: SendRequest) -> Result<MsgRef> {
        let mut body = json!({
            "chat_id": to.chat_id,
            "message_id": to.message_id,
            "text": req.text,
        });
        if let Some(markup) = req.reply_markup() {
            body["reply_markup"] = markup;
        }
        let payload = self.call("editMessageText", body).await?;
        Ok(Self::parse_message_ref(payload.get("result").unwrap_or(&Value::Null))
            .unwrap_or(to))
    }

    async fn answer_callback(&self, callback_id: &str) -> Result<()> {
        self.call(
            "answerCallbackQuery",
            json!({ "callback_query_id": callback_id }),
        )
        .await
        .map(|_| ())
    }

    async fn delete_message(&self, to: MsgRef) -> Result<()> {
        self.call(
            "deleteMessage",
            json!({ "chat_id": to.chat_id, "message_id": to.message_id }),
        )
        .await
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use crate::telegram::fake::FakeTelegram;

    #[derive(Default, Clone)]
    struct CallLog(Arc<Mutex<Vec<Update>>>);

    impl CallLog {
        fn len(&self) -> usize {
            self.0.lock().unwrap().len()
        }
        fn snapshot(&self) -> Vec<Update> {
            self.0.lock().unwrap().clone()
        }
    }

    struct LogHandler(CallLog);

    #[async_trait]
    impl UpdateHandler for LogHandler {
        async fn handle(&self, update: Update, _api: Arc<dyn TelegramApi>) {
            self.0 .0.lock().unwrap().push(update);
        }
    }

    /// Poll a condition for up to ~6s (10ms steps) — deterministic enough
    /// for loop-driven tests without fixed sleeps.
    async fn wait_for(mut cond: impl FnMut() -> bool) {
        for _ in 0..600 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not met within 6s");
    }

    fn text_update(sender: i64, text: &str) -> Update {
        Update {
            sender_id: Some(sender),
            text: Some(text.to_string()),
            ..Default::default()
        }
    }

    fn callback_update(from: i64, data: &str) -> Update {
        Update {
            callback: Some(Callback {
                id: "cbq-1".to_string(),
                data: data.to_string(),
                from_id: from,
            }),
            message_ref: Some(MsgRef {
                chat_id: 555,
                message_id: 77,
            }),
            ..Default::default()
        }
    }

    fn noop_nonce_lookup() -> NonceLookup {
        Arc::new(|flow| (flow == "elev1").then(|| NONCE.to_string()))
    }

    const NONCE: &str = "abcd0123456789ef";

    // --- allowlist filter ---

    #[tokio::test]
    async fn allowlisted_update_reaches_handler() {
        let fake = Arc::new(FakeTelegram::new());
        fake.push_update(text_update(42, "/start"));

        let (tx, rx) = mpsc::channel(8);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut router = TelegramRouter::new();
        let log = CallLog::default();
        router.register(Arc::new(LogHandler(log.clone())));
        let api: Arc<dyn TelegramApi> = fake.clone();
        tokio::spawn(run_poll_loop(
            api.clone(),
            vec![42],
            tx,
            shutdown_rx,
            Duration::from_millis(10),
        ));
        tokio::spawn(router.run(rx, api));

        wait_for(|| log.len() == 1).await;
        let seen = log.snapshot();
        assert_eq!(seen[0].sender_id, Some(42));
        assert_eq!(seen[0].text.as_deref(), Some("/start"));
        shutdown_tx.send_replace(true);
    }

    #[tokio::test]
    async fn non_allowlisted_update_dropped_with_zero_outbound() {
        let fake = Arc::new(FakeTelegram::new());
        fake.push_update(text_update(99, "/start"));

        let (tx, rx) = mpsc::channel(8);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut router = TelegramRouter::new();
        let log = CallLog::default();
        router.register(Arc::new(LogHandler(log.clone())));
        let api: Arc<dyn TelegramApi> = fake.clone();
        tokio::spawn(run_poll_loop(
            api.clone(),
            vec![42],
            tx,
            shutdown_rx,
            Duration::from_millis(10),
        ));
        tokio::spawn(router.run(rx, api));

        wait_for(|| fake.offsets_seen().len() >= 2).await;
        assert_eq!(log.len(), 0, "stranger updates are never dispatched");
        assert!(fake.sent().is_empty(), "strangers get NO reply");
        assert!(fake.edits().is_empty());
        assert!(fake.answered().is_empty());
        assert!(fake.deletes().is_empty());
        shutdown_tx.send_replace(true);
    }

    #[tokio::test]
    async fn callback_from_non_allowlisted_user_dropped_silently() {
        let fake = Arc::new(FakeTelegram::new());
        fake.push_update(callback_update(99, &format!("elev1:{NONCE}:approve")));

        let (tx, rx) = mpsc::channel(8);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut router = TelegramRouter::new();
        let log = CallLog::default();
        router.register(Arc::new(LogHandler(log.clone())));
        router.set_nonce_lookup(noop_nonce_lookup());
        let api: Arc<dyn TelegramApi> = fake.clone();
        tokio::spawn(run_poll_loop(
            api.clone(),
            vec![42],
            tx,
            shutdown_rx,
            Duration::from_millis(10),
        ));
        tokio::spawn(router.run(rx, api));

        wait_for(|| fake.offsets_seen().len() >= 2).await;
        assert_eq!(log.len(), 0, "stranger button presses are never dispatched");
        assert!(fake.answered().is_empty(), "strangers get no callback answer");
        assert!(fake.edits().is_empty(), "strangers get no edit");
        shutdown_tx.send_replace(true);
    }

    #[tokio::test]
    async fn empty_allowlist_drops_everything_fail_closed() {
        let fake = Arc::new(FakeTelegram::new());
        fake.push_update(text_update(42, "/start"));

        let (tx, rx) = mpsc::channel(8);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut router = TelegramRouter::new();
        let log = CallLog::default();
        router.register(Arc::new(LogHandler(log.clone())));
        let api: Arc<dyn TelegramApi> = fake.clone();
        tokio::spawn(run_poll_loop(
            api.clone(),
            vec![],
            tx,
            shutdown_rx,
            Duration::from_millis(10),
        ));
        tokio::spawn(router.run(rx, api));

        wait_for(|| fake.offsets_seen().len() >= 2).await;
        assert_eq!(log.len(), 0, "empty allowlist ⇒ even known senders dropped");
        assert!(fake.sent().is_empty());
        shutdown_tx.send_replace(true);
    }

    // --- signed callback validation ---

    #[tokio::test]
    async fn callback_with_wrong_nonce_answers_and_edits_without_dispatch() {
        let fake = Arc::new(FakeTelegram::new());
        let mut router = TelegramRouter::new();
        let log = CallLog::default();
        router.register(Arc::new(LogHandler(log.clone())));
        router.set_nonce_lookup(noop_nonce_lookup());

        // Same shape and length as a real triple, wrong nonce value.
        let update = callback_update(42, "elev1:0000000000000000:approve");
        let api: Arc<dyn TelegramApi> = fake.clone();
        router.dispatch(&update, &api).await;

        assert_eq!(log.len(), 0, "invalid nonce ⇒ handler NOT dispatched");
        assert_eq!(fake.answered(), vec!["cbq-1".to_string()]);
        let edits = fake.edits();
        assert_eq!(edits.len(), 1, "exactly the no-longer-valid edit");
        assert_eq!(
            edits[0].0,
            MsgRef {
                chat_id: 555,
                message_id: 77
            }
        );
        assert_eq!(edits[0].1.text, ACTION_NO_LONGER_VALID);
        assert!(fake.sent().is_empty());
    }

    #[tokio::test]
    async fn callback_with_unknown_flow_rejected_fail_closed() {
        let fake = Arc::new(FakeTelegram::new());
        let mut router = TelegramRouter::new();
        let log = CallLog::default();
        router.register(Arc::new(LogHandler(log.clone())));
        router.set_nonce_lookup(noop_nonce_lookup());

        let update = callback_update(42, "unknownflow:whatever:approve");
        let api: Arc<dyn TelegramApi> = fake.clone();
        router.dispatch(&update, &api).await;

        assert_eq!(log.len(), 0, "unknown flow ⇒ no dispatch");
        assert_eq!(fake.answered(), vec!["cbq-1".to_string()]);
        assert_eq!(fake.edits().len(), 1);
        assert_eq!(fake.edits()[0].1.text, ACTION_NO_LONGER_VALID);
    }

    #[tokio::test]
    async fn callback_with_valid_nonce_dispatches_handler() {
        let fake = Arc::new(FakeTelegram::new());
        let mut router = TelegramRouter::new();
        let log = CallLog::default();
        router.register(Arc::new(LogHandler(log.clone())));
        router.set_nonce_lookup(noop_nonce_lookup());

        let update = callback_update(42, &format!("elev1:{NONCE}:approve"));
        let api: Arc<dyn TelegramApi> = fake.clone();
        router.dispatch(&update, &api).await;

        assert_eq!(log.len(), 1, "valid nonce reaches the handler");
        assert_eq!(log.snapshot()[0].callback.as_ref().unwrap().data, format!("elev1:{NONCE}:approve"));
        assert_eq!(fake.answered(), vec!["cbq-1".to_string()], "spinner stopped");
        assert!(fake.edits().is_empty(), "no error edit on the happy path");
    }

    #[test]
    fn validate_callback_accepts_exact_triple() {
        let parsed = validate_callback(&format!("elev1:{NONCE}:approve"), NONCE);
        assert_eq!(parsed, Some(("elev1".to_string(), "approve".to_string())));
    }

    #[test]
    fn validate_callback_rejects_wrong_nonce_same_length() {
        assert_eq!(validate_callback("elev1:0000000000000000:approve", NONCE), None);
    }

    #[test]
    fn validate_callback_rejects_wrong_length_nonce() {
        assert_eq!(validate_callback("elev1:short:approve", NONCE), None);
        assert_eq!(
            validate_callback(
                &format!("elev1:{}x:approve", NONCE),
                NONCE
            ),
            None,
            "longer nonce never matches"
        );
    }

    #[test]
    fn validate_callback_rejects_malformed_triples() {
        assert_eq!(validate_callback("elev1", NONCE), None, "no nonce part");
        assert_eq!(validate_callback("elev1:nonce", NONCE), None, "no action part");
        assert_eq!(validate_callback(&format!("elev1:{NONCE}:"), NONCE), None, "empty action");
        assert_eq!(validate_callback(&format!(":{NONCE}:approve"), NONCE), None, "empty flow");
        assert_eq!(validate_callback("", NONCE), None);
    }

    #[test]
    fn nonce_is_16_lowercase_hex_chars_and_random() {
        let a = nonce();
        let b = nonce();
        assert_eq!(a.len(), 16);
        assert!(
            a.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "lowercase hex only: {a}"
        );
        assert_ne!(a, b, "nonces must be random per call");
    }

    // --- error redaction (F1): the bot token must never reach a log line ---

    #[tokio::test]
    async fn transport_errors_are_redacted_and_never_carry_the_bot_token() {
        // Synthetic error-shaped payload: a real reqwest error from a
        // refused loopback connect, whose Display embeds the full URL —
        // including a token-shaped secret in the path — exactly the shape
        // that would leak `bot<TOKEN>` if an error were logged raw.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback bind");
        let port = listener.local_addr().unwrap().port();
        drop(listener); // no listener ⇒ the connect below refuses instantly

        let err = reqwest::get(format!(
            "http://127.0.0.1:{port}/botS3cr3tT0ken123/getUpdates"
        ))
        .await
        .expect_err("connect to a dead port must fail");
        let raw = err.to_string();
        assert!(
            raw.contains("botS3cr3tT0ken123"),
            "sanity: raw reqwest errors embed the url: {raw}"
        );

        let summary = redacted_error("getUpdates", &err).to_string();
        assert_eq!(summary, "getUpdates: transport connection failed");
        assert!(!summary.contains("bot"), "no bot substring survives: {summary}");
        assert!(
            !summary.contains("S3cr3tT0ken123"),
            "no token substring survives: {summary}"
        );
        assert!(
            !summary.contains("127.0.0.1"),
            "no url fragment survives: {summary}"
        );
    }

    // --- poll loop ---

    #[tokio::test]
    async fn poll_loop_offset_advances_across_deliveries() {
        let fake = Arc::new(FakeTelegram::new());
        fake.push_update(Update {
            update_id: 5,
            sender_id: Some(42),
            text: Some("first".to_string()),
            ..Default::default()
        });
        fake.push_update(Update {
            update_id: 7,
            sender_id: Some(42),
            text: Some("second".to_string()),
            ..Default::default()
        });

        let (tx, rx) = mpsc::channel(8);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut router = TelegramRouter::new();
        let log = CallLog::default();
        router.register(Arc::new(LogHandler(log.clone())));
        let api: Arc<dyn TelegramApi> = fake.clone();
        tokio::spawn(run_poll_loop(
            api.clone(),
            vec![42],
            tx,
            shutdown_rx,
            Duration::from_millis(10),
        ));
        tokio::spawn(router.run(rx, api));

        wait_for(|| log.len() == 2).await;
        let offsets = fake.offsets_seen();
        assert_eq!(offsets[0], 0, "first poll starts from zero");
        assert_eq!(offsets[1], 8, "offset = last_update_id + 1");
        let texts: Vec<_> = log.snapshot().iter().filter_map(|u| u.text.clone()).collect();
        assert_eq!(texts, vec!["first".to_string(), "second".to_string()]);
        shutdown_tx.send_replace(true);
    }

    #[tokio::test]
    async fn poll_loop_survives_get_updates_errors_with_backoff() {
        let fake = Arc::new(FakeTelegram::new());
        fake.fail_next_get_updates(2);
        fake.push_update(text_update(42, "after the outage"));

        let (tx, rx) = mpsc::channel(8);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut router = TelegramRouter::new();
        let log = CallLog::default();
        router.register(Arc::new(LogHandler(log.clone())));
        let api: Arc<dyn TelegramApi> = fake.clone();
        tokio::spawn(run_poll_loop(
            api.clone(),
            vec![42],
            tx,
            shutdown_rx,
            Duration::from_millis(10),
        ));
        tokio::spawn(router.run(rx, api));

        wait_for(|| log.len() == 1).await;
        let offsets = fake.offsets_seen();
        assert!(
            offsets.len() >= 3,
            "two failed attempts plus the successful one: {offsets:?}"
        );
        assert_eq!(offsets[0], 0);
        assert_eq!(offsets[1], 0, "failed attempt does not advance offset");
        assert_eq!(offsets[2], 0);
        assert_eq!(log.snapshot()[0].text.as_deref(), Some("after the outage"));
        shutdown_tx.send_replace(true);
    }

    // --- lenient Bot API JSON parsing ---

    #[test]
    fn parses_message_update() {
        let v = json!({
            "update_id": 10,
            "message": {
                "message_id": 20,
                "text": "hello gate",
                "date": 1_700_000_000,
                "from": {"id": 42, "is_bot": false, "first_name": "Rob"},
                "chat": {"id": -100_200, "title": "ops", "type": "group"},
            },
        });
        let u = Update::parse(&v).expect("message update parses");
        assert_eq!(u.update_id, 10);
        assert_eq!(u.sender_id, Some(42));
        assert_eq!(u.text.as_deref(), Some("hello gate"));
        assert!(u.callback.is_none());
        assert_eq!(
            u.message_ref,
            Some(MsgRef {
                chat_id: -100_200,
                message_id: 20
            })
        );
    }

    #[test]
    fn parses_callback_query_update() {
        let v = json!({
            "update_id": 11,
            "callback_query": {
                "id": "cbq-9",
                "data": "elev1:abcd0123456789ef:approve",
                "from": {"id": 42, "is_bot": false},
                "message": {"message_id": 30, "chat": {"id": 555}},
            },
        });
        let u = Update::parse(&v).expect("callback update parses");
        assert_eq!(u.update_id, 11);
        let cb = u.callback.expect("callback present");
        assert_eq!(cb.id, "cbq-9");
        assert_eq!(cb.data, "elev1:abcd0123456789ef:approve");
        assert_eq!(cb.from_id, 42);
        assert_eq!(
            u.message_ref,
            Some(MsgRef {
                chat_id: 555,
                message_id: 30
            })
        );
    }

    #[test]
    fn parse_is_lenient_about_unknown_kinds_and_missing_update_id() {
        let unknown_kind = json!({"update_id": 3, "my_chat_member": {"x": 1}});
        let u = Update::parse(&unknown_kind).expect("id-only update survives");
        assert_eq!(u.update_id, 3);
        assert_eq!(u.sender_id, None);
        assert_eq!(u.text, None);
        assert_eq!(u.callback, None);
        assert_eq!(u.message_ref, None);

        assert_eq!(Update::parse(&json!({"message": {"text": "no id"}})), None);
        assert_eq!(Update::parse(&json!({})), None, "no update_id ⇒ skipped");
    }
}
