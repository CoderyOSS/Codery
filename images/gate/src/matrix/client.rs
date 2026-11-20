//! MatrixClient transport (GATE-012): the Matrix half of the control
//! plane, implemented behind the GATE-006 [`crate::control::ControlApi`]
//! trait. ALL matrix-sdk calls live in this file; error strings carry
//! kind words only — never the access token (mirrors the Telegram
//! redaction discipline).
//!
//! Inbound: decrypted room messages from the pinned room, senders on the
//! mxid allowlist, become the same `Update`s a Telegram poll would emit;
//! `/ok`-family commands carrying a signed triple become `Callback`s
//! (see [`super::command_to_callback`]). Outbound: sends into the room;
//! buttons render as the exact commands to send (see
//! [`super::render_buttons`]); edits target bound event ids, falling
//! back to a fresh send when the binding predates this process (ADR
//! GATE-012 trade-off).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::ruma::events::relation::{InReplyTo, Reply, Replacement};
use matrix_sdk::ruma::events::room::message::{MessageType, Relation, RoomMessageEventContent, SyncRoomMessageEvent};
use matrix_sdk::ruma::{OwnedDeviceId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId};
use matrix_sdk::{Client, Room};
use tokio::sync::mpsc;

use super::{command_to_callback, mxid_map, render_buttons, ROOM_CHAT_ID};
use crate::config::MatrixConfig;
use crate::control::{Callback, MsgRef, SendRequest, ControlApi, Update};

/// Wire-shape check for `MATRIX_ROOM_ID`: `!localpart:server` with both
/// halves non-empty. Fail at connect (boot) — never half-wire a channel.
pub fn validate_room_id(room_id: &str) -> Result<()> {
    let id = room_id.trim();
    if !id.starts_with('!') {
        return Err(anyhow!("MATRIX_ROOM_ID must look like '!local:server' (got a value without '!')"));
    }
    let (local, server) = id
        .strip_prefix('!')
        .and_then(|rest| rest.split_once(':'))
        .ok_or_else(|| anyhow!("MATRIX_ROOM_ID must look like '!local:server'"))?;
    if local.is_empty() || server.is_empty() {
        return Err(anyhow!("MATRIX_ROOM_ID must look like '!local:server' — local and server parts are both required"));
    }
    Ok(())
}

/// Shared mutable identity state: sequential update ids for the poll
/// loop's offset bookkeeping, and the numeric message-id ↔ event-id
/// bindings that make `MsgRef`s work over string-keyed Matrix events.
/// In-memory by design (ADR GATE-012 trade-off): bindings predating this
/// process do not resolve, and `edit_message` falls back to a fresh send.
#[derive(Default)]
pub struct SharedState {
    next_update_id: AtomicI64,
    next_message_id: AtomicI32,
    events: Mutex<HashMap<i32, OwnedEventId>>,
    /// Message ids the GATE ITSELF sent (C2): only bot-owned events may
    /// carry `m.replace` edits — cross-sender replacements are invalid
    /// per the Matrix spec and are ignored by clients.
    owned: Mutex<std::collections::HashSet<i32>>,
}

impl SharedState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The next sequential update id (1, 2, 3, …) — the poll loop only
    /// needs monotonic ids (`offset = last + 1`); it never derives
    /// meaning from the values.
    pub fn next_update_id(&self) -> i64 {
        self.next_update_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Bind an OUTBOUND (bot-sent) event id to a fresh `MsgRef`. One
    /// binding table serves both directions; ownership is tracked
    /// separately so edits only ever target the gate's own messages.
    pub fn bind(&self, event_id: OwnedEventId) -> MsgRef {
        let id = self.next_message_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.events
            .lock()
            .expect("event map lock must be poison-free")
            .insert(id, event_id);
        self.owned
            .lock()
            .expect("owned set lock must be poison-free")
            .insert(id);
        MsgRef {
            chat_id: ROOM_CHAT_ID,
            message_id: id,
        }
    }

    /// Bind an INBOUND (operator-sent) event id. The gate must never
    /// `m.replace` another user's event (Matrix spec: replacement validity)
    /// — `resolve_owned` returns `None` for these, driving the fresh-reply
    /// fallback in `edit_message`.
    pub fn bind_inbound(&self, event_id: OwnedEventId) -> MsgRef {
        let id = self.next_message_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.events
            .lock()
            .expect("event map lock must be poison-free")
            .insert(id, event_id);
        MsgRef {
            chat_id: ROOM_CHAT_ID,
            message_id: id,
        }
    }

    /// Resolve a `MsgRef` to its event id — `None` when the binding is
    /// unknown (predates this process): the caller falls back per the
    /// ADR (edit ⇒ fresh send; delete ⇒ no-op).
    pub fn resolve(&self, to: MsgRef) -> Option<OwnedEventId> {
        self.events
            .lock()
            .expect("event map lock must be poison-free")
            .get(&to.message_id)
            .cloned()
    }

    /// Like [`resolve`], but `None` unless the event was sent BY THE GATE
    /// (C2) — the precondition for a spec-valid `m.replace`.
    pub fn resolve_owned(&self, to: MsgRef) -> Option<OwnedEventId> {
        let owned = self
            .owned
            .lock()
            .expect("owned set lock must be poison-free")
            .contains(&to.message_id);
        if owned {
            self.resolve(to)
        } else {
            None
        }
    }

    /// Queue a parsed inbound update for the poll loop; sequential ids
    /// assigned here. Test-visible.
    pub fn stage(&self, mut update: Update, queue: &Mutex<Vec<Update>>) {
        update.update_id = self.next_update_id();
        queue
            .lock()
            .expect("queue lock must be poison-free")
            .push(update);
    }
}

/// Classify a transport failure into a kind word — never the SDK error's
/// Display (defence in depth: credentials live in the session, and error
/// text is the classic leak path).
fn kind_of(_: &impl std::fmt::Debug) -> &'static str {
    "transport error"
}

/// Backoff delay after the `failure_count`-th consecutive sync error:
/// 1s, 2s, 4s … capped at 60s (mirrors the GATE-006 poll-loop posture).
fn sync_backoff(failure_count: u32) -> Duration {
    let shift = failure_count.saturating_sub(1).min(6);
    Duration::from_secs(1)
        .saturating_mul(1u32 << shift)
        .min(Duration::from_secs(60))
}

/// Where the SDK persists device identity + Megolm sessions: a directory
/// under the gate data dir. Persistence is load-bearing for E2EE: a fresh
/// olm account per boot would churn the bot's device identity and lose
/// room keys (GATE-012).
fn state_store_dir(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("matrix-state")
}

/// File-backed stable device id: same file across boots so the bot's
/// device identity (and thus its room-key membership) survives restarts.
async fn load_or_create_device_id(state_dir: &Path) -> Result<OwnedDeviceId> {
    let path = state_dir.join("device-id");
    if let Ok(existing) = tokio::fs::read_to_string(&path).await {
        let id = existing.trim();
        if !id.is_empty() {
            return Ok(id.into());
        }
    }
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).map_err(|_| anyhow!("entropy source failed"))?;
    let id: OwnedDeviceId = format!(
        "gate-{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
    .into();
    tokio::fs::write(&path, id.as_str())
        .await
        .map_err(|_| anyhow!("cannot persist device id to {}", path.display()))?;
    Ok(id)
}

/// Resolve the bot account's mxid via `/account/whoami` — the one raw
/// call the SDK session needs (`restore_session` requires the user id,
/// which an access token alone does not carry). Bearer-token errors are
/// reduced to kind words.
async fn whoami(homeserver_url: &str, access_token: &str) -> Result<OwnedUserId> {
    let base = homeserver_url.trim_end_matches('/');
    let url = format!("{base}/_matrix/client/v3/account/whoami");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| anyhow!("whoami: client build failed"))?;
    let response = http
        .get(&url)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|_| anyhow!("whoami: transport error"))?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "whoami: status {} — check MATRIX_ACCESS_TOKEN",
            response.status().as_u16()
        ));
    }
    let payload: serde_json::Value = response
        .json()
        .await
        .map_err(|_| anyhow!("whoami: bad response"))?;
    let user_id = payload
        .get("user_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("whoami: no user_id in response"))?;
    user_id
        .parse::<OwnedUserId>()
        .map_err(|_| anyhow!("whoami: unparsable user_id"))
}

/// Build the SDK client: sqlite state store under `state_dir`, session
/// restored from the staged access token + stable device id.
async fn build_client(cfg: &MatrixConfig, state_dir: &Path) -> Result<Client> {
    let client = Client::builder()
        .homeserver_url(&cfg.homeserver_url)
        .sqlite_store(state_store_dir(state_dir), None::<&str>)
        .build()
        .await
        .map_err(|_| anyhow!("matrix: client build failed"))?;
    let user_id = whoami(&cfg.homeserver_url, &cfg.access_token).await?;
    let device_id = load_or_create_device_id(state_dir).await?;
    client
        .restore_session(MatrixSession {
            meta: matrix_sdk::SessionMeta { user_id, device_id },
            tokens: matrix_sdk::authentication::SessionTokens {
                access_token: cfg.access_token.clone(),
                refresh_token: None,
            },
        })
        .await
        .map_err(|_| anyhow!("matrix: session restore failed — check MATRIX_ACCESS_TOKEN"))?;
    Ok(client)
}

/// Join the pinned control room. Failure is fatal at boot (the room IS
/// the channel — without it there is no control plane).
async fn join_control_room(client: &Client, room_id: &RoomId) -> Result<Room> {
    client
        .join_room_by_id(room_id)
        .await
        .map_err(|_| anyhow!("matrix: could not join the control room — invite the bot and retry"))
}

/// E2EE check (Review Focus 5): the room must be encrypted or the channel
/// is no better than Telegram. Warn loudly, do not refuse — the operator
/// may still be mid-setup.
async fn warn_if_unencrypted(room: &Room, room_id: &RoomId) {
    match room.latest_encryption_state().await {
        Ok(matrix_sdk::EncryptionState::Encrypted) => {}
        Ok(matrix_sdk::EncryptionState::NotEncrypted) => eprintln!(
            "WARNING: matrix control room {room_id} is NOT encrypted — \
             enable encryption for the GATE-012 guarantees"
        ),
        Ok(_) => eprintln!(
            "WARNING: matrix control room {room_id} encryption state unknown \
             (room history may not be decryptable yet)"
        ),
        Err(_) => eprintln!(
            "WARNING: matrix control room {room_id} encryption state could not be fetched"
        ),
    }
}

/// The live transport. `rx` long-poll semantics: `get_updates` blocks on
/// the queue the sync handlers feed. The sync task RETRIES with backoff
/// on transport errors (C3) — a transient homeserver blip must never
/// permanently silence the channel, so the queue in practice stays open
/// for the process's lifetime.
pub struct MatrixClient {
    room: Room,
    rx: tokio::sync::Mutex<mpsc::Receiver<Update>>,
    state: Arc<SharedState>,
}

/// Inbound pipeline: one decrypted room message → zero or one `Update`.
/// Room must match, sender must be allowlisted; `/ok`-family commands
/// become validated-later `Callback`s, other text becomes plain updates.
async fn handle_message(
    event: SyncRoomMessageEvent,
    room: Room,
    room_id: &RoomId,
    ids: &Arc<HashMap<String, i64>>,
    state: &Arc<SharedState>,
    tx: &mpsc::Sender<Update>,
) {
    let original = match event {
        SyncRoomMessageEvent::Original(original) => original,
        SyncRoomMessageEvent::Redacted(_) => return,
    };
    if room.room_id() != room_id {
        return;
    }
    let body = match original.content.msgtype {
        MessageType::Text(text) => text.body,
        MessageType::Notice(notice) => notice.body,
        _ => return,
    };
    // Edits carry a Replacement relation; commands are never edits.
    if matches!(original.content.relates_to, Some(Relation::Replacement(_))) {
        return;
    }
    let mxid = original.sender.as_str();
    let Some(&from_id) = ids.get(mxid) else {
        return; // stranger: dropped with zero outbound (GATE-006 rule)
    };
    let update = synthesize_update(state, from_id, &body, original.event_id);
    let _ = tx.send(update).await;
}

/// Inbound event → the `Update` the control plane consumes (C2/I1 fix
/// pass). One function so the synthesis shapes stay testable without the
/// SDK: `/ok`-family commands become callback updates in the exact shape
/// a Telegram button press produces — NO text, NO sender_id (`commands`
/// fires on text presence; carrying both would double-fire an
/// UNKNOWN_COMMAND reply on every decision) — other text keeps sender and
/// text. The anchor binds as INBOUND: the gate owns it not, so feedback
/// edits resolve to None and answer with a fresh reply instead of the
/// spec-invalid cross-sender `m.replace`.
fn synthesize_update(
    state: &SharedState,
    from_id: i64,
    body: &str,
    event_id: OwnedEventId,
) -> Update {
    let msg_ref = state.bind_inbound(event_id);
    if let Some(data) = command_to_callback(body) {
        return Update {
            update_id: 0,
            sender_id: None,
            text: None,
            callback: Some(Callback {
                id: "matrix".to_string(),
                data,
                from_id,
            }),
            message_ref: Some(msg_ref),
        };
    }
    Update {
        update_id: 0,
        sender_id: Some(from_id),
        text: Some(body.to_string()),
        callback: None,
        message_ref: Some(msg_ref),
    }
}

/// Connect and arm the transport: validate config shape, restore/build
/// the SDK client, join the pinned room, install the sync handlers, and
/// spawn the sync loop. Any failure aborts boot (fail closed).
pub async fn connect(cfg: &MatrixConfig, state_dir: &Path) -> Result<MatrixClient> {
    validate_room_id(&cfg.room_id)?;
    let room_id: OwnedRoomId = RoomId::parse(cfg.room_id.trim())
        .map_err(|_| anyhow!("MATRIX_ROOM_ID does not parse as a room id"))?;
    let ids = Arc::new(mxid_map(&cfg.user_allowlist));
    let state = Arc::new(SharedState::new());
    tokio::fs::create_dir_all(state_dir)
        .await
        .map_err(|_| anyhow!("matrix: cannot create {}", state_dir.display()))?;

    let client = build_client(cfg, state_dir).await?;
    let room = join_control_room(&client, &room_id).await?;
    warn_if_unencrypted(&room, &room_id).await;

    let (tx, rx) = mpsc::channel(64);
    let handler_room_id = room_id.clone();
    let handler_ids = ids.clone();
    let handler_state = state.clone();
    let handler_tx = tx.clone();
    client.add_event_handler(move |event: SyncRoomMessageEvent, room: Room| {
        let room_id = handler_room_id.clone();
        let ids = handler_ids.clone();
        let state = handler_state.clone();
        let tx = handler_tx.clone();
        async move {
            handle_message(event, room, &room_id, &ids, &state, &tx).await;
        }
    });
    let sync_client = client.clone();
    tokio::spawn(async move {
        // C3: the SDK's sync returns on ANY transport error and it is the
        // caller's job to continue — a fire-and-forget spawn would turn
        // one homeserver blip into silent permanent loss of inbound
        // traffic. Retry with the GATE-006 backoff posture instead.
        let mut failures: u32 = 0;
        loop {
            match sync_client
                .sync(SyncSettings::default().timeout(Duration::from_secs(30)))
                .await
            {
                Ok(()) => return, // clean stop: the client was dropped
                Err(e) => {
                    failures = failures.saturating_add(1);
                    let delay = sync_backoff(failures);
                    eprintln!("matrix: sync error ({}); retrying in {delay:?}", kind_of(&e));
                    tokio::time::sleep(delay).await;
                }
            }
        }
    });

    Ok(MatrixClient {
        room,
        rx: tokio::sync::Mutex::new(rx),
        state,
    })
}

/// Append the button lines to an outbound body (GATE-012: buttons become
/// the exact commands to send).
fn with_button_lines(mut body: String, req: &SendRequest) -> String {
    for (label, url) in &req.link_buttons {
        body.push_str(&format!("\n[{label}]({url})"));
    }
    if let Some(lines) = render_buttons(&req.inline_buttons) {
        body.push('\n');
        body.push_str(&lines);
    }
    body
}

impl MatrixClient {
    /// Room send with GATE-012 shaping: HTML mode renders formatted,
    /// replies anchor via `m.in_reply_to`, buttons become command lines.
    async fn send_content(&self, req: &SendRequest) -> Result<MsgRef> {
        let body = with_button_lines(req.text.clone(), req);
        let mut content = if req.parse_mode.as_deref() == Some("HTML") {
            RoomMessageEventContent::text_html(&body, &body)
        } else {
            RoomMessageEventContent::text_plain(&body)
        };
        if let Some(reply_to) = req.reply_to {
            if reply_to.message_id > 0 {
                if let Some(event_id) = self.state.resolve(reply_to) {
                    content.relates_to = Some(Relation::Reply(Reply::new(InReplyTo::new(event_id))));
                }
            }
        }
        let response = self
            .room
            .send(content)
            .await
            .map_err(|_| anyhow!("matrix send: transport error"))?;
        Ok(self.state.bind(response.response.event_id))
    }
}

#[async_trait]
impl ControlApi for MatrixClient {
    /// Long-poll equivalent: block on the queue (the SDK sync loop feeds
    /// it), then drain everything already queued. The offset argument is
    /// bookkeeping-only — update ids are sequential per-process.
    async fn get_updates(&self, _offset: i64) -> Result<Vec<Update>> {
        let mut rx = self.rx.lock().await;
        let first = rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("matrix sync task stopped"))?;
        let mut updates = vec![first];
        while let Ok(update) = rx.try_recv() {
            updates.push(update);
        }
        Ok(updates)
    }

    async fn send_message(&self, req: SendRequest) -> Result<MsgRef> {
        self.send_content(&req).await
    }

    /// Edit in place via `m.new_content` — bot-owned events only (C2):
    /// a cross-sender `m.replace` is invalid per the Matrix spec, so an
    /// operator's event (or a pre-restart binding) falls back to a fresh
    /// REPLY send, which every handler's request already anchors via
    /// `replying_to`.
    async fn edit_message(&self, to: MsgRef, req: SendRequest) -> Result<MsgRef> {
        match self.state.resolve_owned(to) {
            Some(event_id) => {
                let body = with_button_lines(req.text.clone(), &req);
                let new_content = if req.parse_mode.as_deref() == Some("HTML") {
                    RoomMessageEventContent::text_html(&body, &body)
                } else {
                    RoomMessageEventContent::text_plain(&body)
                };
                let mut content = RoomMessageEventContent::text_plain("* (edit)");
                content.relates_to = Some(Relation::Replacement(Replacement::new(
                    event_id,
                    new_content.into(),
                )));
                let response = self
                    .room
                    .send(content)
                    .await
                    .map_err(|_| anyhow!("matrix edit: transport error"))?;
                Ok(self.state.bind(response.response.event_id))
            }
            None => {
                let req = match req.reply_to {
                    Some(_) => req,
                    None => req.replying_to(to),
                };
                self.send_content(&req).await
            }
        }
    }

    /// Matrix has no callback-query spinner to stop.
    async fn answer_callback(&self, _callback_id: &str) -> Result<()> {
        Ok(())
    }

    /// Redact — bot-owned events only; anything else is a no-op (the
    /// gate cannot redact another user's message anyway).
    async fn delete_message(&self, to: MsgRef) -> Result<()> {
        if let Some(event_id) = self.state.resolve_owned(to) {
            self.room
                .redact(&event_id, None, None)
                .await
                .map_err(|_| anyhow!("matrix redact: transport error"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::Callback;

    const GOOD_ROOM: &str = "!control:example.org";

    #[test]
    fn room_id_shape_validated() {
        assert!(validate_room_id(GOOD_ROOM).is_ok());
        for bad in [
            "control:example.org", // no bang
            "!nocolon",            // no server
            "!:example.org",       // empty local
            "!local:",             // empty server
            "",                    // empty
        ] {
            assert!(validate_room_id(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(validate_room_id("  !control:example.org  ").is_ok(), "trimmed");
    }

    #[test]
    fn event_map_roundtrip() {
        let state = SharedState::new();
        let ev: OwnedEventId = "$abc123:example.org".parse().expect("valid event id");
        let to = state.bind(ev.clone());
        assert_eq!(to.chat_id, ROOM_CHAT_ID);
        assert_eq!(to.message_id, 1, "first binding gets id 1");
        assert_eq!(state.resolve(to).as_ref(), Some(&ev));

        let ev2: OwnedEventId = "$def456:example.org".parse().expect("valid event id");
        let to2 = state.bind(ev2.clone());
        assert_eq!(to2.message_id, 2, "bindings are sequential");
        assert_eq!(state.resolve(to2).as_ref(), Some(&ev2));
        assert!(
            state.resolve(MsgRef { chat_id: ROOM_CHAT_ID, message_id: 999 }).is_none(),
            "unknown bindings resolve to None (pre-restart fallback)"
        );
    }

    #[test]
    fn update_ids_are_sequential() {
        let state = SharedState::new();
        let queue = Mutex::new(Vec::new());
        let mk = |cb: Option<Callback>| Update {
            sender_id: Some(1),
            text: Some("hi".into()),
            callback: cb,
            ..Default::default()
        };
        state.stage(mk(None), &queue);
        state.stage(mk(None), &queue);
        let staged = queue.lock().unwrap().clone();
        assert_eq!(
            staged.iter().map(|u| u.update_id).collect::<Vec<_>>(),
            vec![1, 2],
            "sequential update ids for the poll loop's offset bookkeeping"
        );
    }

    // --- C2/I1: update synthesis + edit-ownership (GATE-012 fix pass) ---

    #[test]
    fn callback_update_shape_matches_telegram() {
        // A synthesized button-press equivalent must look EXACTLY like a
        // Telegram callback update: no text, no sender_id (commands.rs
        // fires on text presence — carrying both would double-fire an
        // UNKNOWN_COMMAND reply on every decision).
        let state = SharedState::new();
        let ev: OwnedEventId = "$cmd1:example.org".parse().unwrap();
        let update = synthesize_update(&state, 2, "/ok elev1:abcd0123456789ef:approve", ev);
        let cb = update.callback.as_ref().expect("command ⇒ callback");
        assert_eq!(cb.data, "elev1:abcd0123456789ef:approve");
        assert_eq!(cb.from_id, 2);
        assert_eq!(cb.id, "matrix");
        assert_eq!(update.text, None, "callback updates carry no text");
        assert_eq!(update.sender_id, None, "callback updates carry no sender");
        assert!(update.message_ref.is_some(), "anchor kept for feedback");
    }

    #[test]
    fn text_update_keeps_sender_and_text() {
        let state = SharedState::new();
        let ev: OwnedEventId = "$cmd2:example.org".parse().unwrap();
        let update = synthesize_update(&state, 3, "hello gate", ev);
        assert_eq!(update.text.as_deref(), Some("hello gate"));
        assert_eq!(update.sender_id, Some(3));
        assert!(update.callback.is_none());
    }

    #[test]
    fn owned_vs_inbound_bindings() {
        // C2: the gate may only m.replace events IT sent. Outbound binds
        // (send_message) mark ownership; inbound binds never do, so
        // edit/redact on an operator's event resolves to None and the
        // transport falls back to a fresh reply.
        let state = SharedState::new();
        let bot_ev: OwnedEventId = "$bot1:example.org".parse().unwrap();
        let bot_ref = state.bind(bot_ev.clone());
        let op_ev: OwnedEventId = "$op1:example.org".parse().unwrap();
        let op_ref = state.bind_inbound(op_ev.clone());

        assert_eq!(state.resolve_owned(bot_ref).as_ref(), Some(&bot_ev));
        assert!(state.resolve_owned(op_ref).is_none(), "inbound ⇒ not owned");
        assert_eq!(state.resolve(op_ref).as_ref(), Some(&op_ev), "reply anchors still resolve");
    }

    #[test]
    fn sync_backoff_matches_the_gate006_posture() {
        // C3: 1s, 2s, 4s … capped at 60s.
        assert_eq!(sync_backoff(1), Duration::from_secs(1));
        assert_eq!(sync_backoff(2), Duration::from_secs(2));
        assert_eq!(sync_backoff(3), Duration::from_secs(4));
        assert_eq!(sync_backoff(7), Duration::from_secs(60), "cap");
        assert_eq!(sync_backoff(50), Duration::from_secs(60), "stays capped");
    }
}
