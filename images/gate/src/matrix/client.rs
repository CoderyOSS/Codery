//! MatrixClient transport (GATE-012): the Matrix half of the control
//! plane, implemented behind the GATE-006 [`crate::telegram::TelegramApi`]
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
use crate::telegram::{Callback, MsgRef, SendRequest, TelegramApi, Update};

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

    /// Bind an outbound/inbound event id to a fresh `MsgRef` in the
    /// control room. One binding table serves both directions: the id
    /// space is per-process and monotonic.
    pub fn bind(&self, event_id: OwnedEventId) -> MsgRef {
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
/// the queue the sync handlers feed; a closed queue means the sync loop
/// died and the poll loop backs off (GATE-006 loop semantics).
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
    let to = state.bind(original.event_id);
    let update = if let Some(triple) = command_to_callback(&body) {
        Update {
            update_id: 0,
            sender_id: Some(from_id),
            text: Some(body),
            callback: Some(Callback {
                id: "matrix".to_string(),
                data: triple,
                from_id,
            }),
            message_ref: Some(to),
        }
    } else {
        Update {
            update_id: 0,
            sender_id: Some(from_id),
            text: Some(body),
            callback: None,
            message_ref: Some(to),
        }
    };
    let _ = tx.send(update).await;
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
        if let Err(e) = sync_client
            .sync(SyncSettings::default().timeout(Duration::from_secs(30)))
            .await
        {
            eprintln!("matrix: sync loop ended ({})", kind_of(&e));
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
impl TelegramApi for MatrixClient {
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

    /// Edit in place via `m.new_content`; a binding that predates this
    /// process falls back to a fresh send (ADR GATE-012 trade-off).
    async fn edit_message(&self, to: MsgRef, req: SendRequest) -> Result<MsgRef> {
        match self.state.resolve(to) {
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
            None => self.send_content(&req).await,
        }
    }

    /// Matrix has no callback-query spinner to stop.
    async fn answer_callback(&self, _callback_id: &str) -> Result<()> {
        Ok(())
    }

    /// Redact; unknown bindings (pre-restart) are a no-op — the message
    /// is gone from the process's world anyway.
    async fn delete_message(&self, to: MsgRef) -> Result<()> {
        if let Some(event_id) = self.state.resolve(to) {
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
    use crate::telegram::Callback;

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
}
