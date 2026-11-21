//! ntfy control-channel pure layer (GATE-013): the pieces the transport
//! and its tests build on that need no HTTP. One behavioral contract —
//! everything here mirrors a GATE-006 shape.
//!
//! Mapping:
//! - both topics (control + answers) are token-gated; every message on
//!   them is the operator, so synthesized updates carry the transport's
//!   single sender id (see [`SENDER_ID`]);
//! - approval-ask buttons render as ntfy HTTP actions that POST the
//!   signed `"{flow_id}:{nonce}:{action}"` triple (GATE-006's
//!   `callback_data`) to the answers topic;
//! - an inbound message that IS a bare triple (button POST) or an
//!   `/ok`-family command (written reply) becomes the same validated
//!   [`crate::control::Callback`] a Telegram button press would produce —
//!   callback updates carry NO text, exactly like the Telegram shape.

pub mod client;

use std::sync::atomic::{AtomicI64, Ordering};

use serde_json::{json, Value};

pub use self::client::{connect, NtfyClient};

use crate::control::{Callback, Update};

/// The numeric topic id for the control topic — the transport's `MsgRef`
/// anchors and the poll loop's allowlist both use this single id.
pub const CONTROL_TOPIC_ID: i64 = 1;
/// The transport-assigned operator id: the broker token is the identity,
/// so every message on the gated topics is id 1.
pub const SENDER_ID: i64 = 1;

/// Per-process sequential update ids for the poll loop's offset
/// bookkeeping (values carry no meaning across restarts).
#[derive(Default)]
pub struct SharedState {
    next_update_id: AtomicI64,
}

impl SharedState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn next_update_id(&self) -> i64 {
        self.next_update_id.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// Minimum nonce length for a triple to count as signed (production
/// nonces are 16 hex chars, GATE-006's [`crate::control::nonce`]).
const MIN_NONCE_LEN: usize = 8;

/// Parse a bare signed triple `flow:nonce:action` — the body an ntfy
/// action button POSTs to the answers topic.
pub fn triple_of(text: &str) -> Option<String> {
    let trimmed = text.trim();
    // A bare triple has no whitespace anywhere (button bodies); anything
    // with spaces is either a command (handled by `command_to_callback`)
    // or prose — never a triple.
    if trimmed.split_whitespace().count() != 1 {
        return None;
    }
    let mut parts = trimmed.split(':');
    let flow = parts.next()?;
    let nonce = parts.next()?;
    let action = parts.next()?;
    if parts.next().is_some()
        || flow.is_empty()
        || action.is_empty()
        || nonce.len() < MIN_NONCE_LEN
    {
        return None;
    }
    Some(trimmed.to_string())
}

/// An inbound `/ok`-family command (written reply): the triple it
/// carries, or `None` when the text is not a command (stays plain text
/// for the command router). Prefix matching is case-insensitive.
pub fn command_to_callback(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let (prefix, rest) = trimmed.split_once(' ')?;
    let is_prefix = matches!(
        prefix.to_ascii_lowercase().as_str(),
        "/ok" | "/no" | "/approve" | "/deny"
    );
    if !is_prefix {
        return None;
    }
    triple_of(rest.trim())
}

/// ntfy renders at most THREE user actions per notification — anything
/// beyond is dropped here (transports may degrade rendering; the dropped
/// decisions remain available as written `/ok`-family commands).
pub const MAX_ACTIONS: usize = 3;

/// The ntfy `actions` JSON array for approval-ask buttons: one HTTP
/// action per `(label, data)` — `body` carries the signed triple, `clear`
/// swaps the ask for its outcome. Capped at [`MAX_ACTIONS`]; `None` when
/// there are no buttons. The POST is unauthenticated BY DESIGN: the
/// answers topic carries a write-only anonymous grant and its URL is the
/// secret (GATE-013) — the operator's phone holds no broker token.
pub fn actions_buttons(
    answers_base_url: &str,
    buttons: &[(String, String)],
) -> Option<Vec<Value>> {
    if buttons.is_empty() {
        return None;
    }
    let base = answers_base_url.trim_end_matches('/');
    Some(
        buttons
            .iter()
            .take(MAX_ACTIONS)
            .map(|(label, data)| {
                json!({
                    "action": "http",
                    "label": label,
                    "url": format!("{base}"),
                    "method": "POST",
                    "body": data,
                    "clear": true,
                })
            })
            .collect(),
    )
}

/// One `/json` stream line → zero or one [`Update`] (GATE-013). Message
/// events on the gated topics: a bare triple (button POST) or an
/// `/ok`-family command becomes a callback update with NO text (the
/// Telegram shape — command handlers fire on text presence); any other
/// message becomes a text update. Non-message events (`open`, `keepalive`,
/// `poll_request`), title-only messages, and unparseable lines ⇒ `None`.
pub fn update_from_line(state: &SharedState, line: &str) -> Option<Update> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("event").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let body = v.get("message").and_then(Value::as_str)?;
    if body.is_empty() {
        return None;
    }
    let data = triple_of(body).or_else(|| command_to_callback(body));
    let update_id = state.next_update_id();
    Some(match data {
        Some(data) => Update {
            update_id,
            sender_id: None,
            text: None,
            callback: Some(Callback {
                id: "ntfy".to_string(),
                data,
                from_id: SENDER_ID,
            }),
            message_ref: None,
        },
        None => Update {
            update_id,
            sender_id: Some(SENDER_ID),
            text: Some(body.to_string()),
            callback: None,
            message_ref: None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWERS: &str = "https://ntfy.example.com/gate-answers-91bd";

    #[test]
    fn triple_of_accepts_exact_shape_and_rejects_garbage() {
        assert_eq!(
            triple_of("elev1:abcd0123456789ef:approve").as_deref(),
            Some("elev1:abcd0123456789ef:approve")
        );
        assert_eq!(
            triple_of("  elev1:abcd0123456789ef:approve ").as_deref(),
            Some("elev1:abcd0123456789ef:approve"),
            "tolerant trim: button bodies may carry stray whitespace"
        );
        assert_eq!(triple_of("one:two"), None);
        assert_eq!(triple_of("elev1:short:approve"), None);
        assert_eq!(triple_of("elev1:abcd0123456789ef:"), None);
        assert_eq!(triple_of("a:b:c:d"), None);
    }

    #[test]
    fn command_prefixes_yield_the_triple() {
        for prefix in ["/ok", "/no", "/approve", "/deny", "/OK"] {
            let got = command_to_callback(&format!("{prefix} elev1:abcd0123456789ef:approve"));
            assert_eq!(got.as_deref(), Some("elev1:abcd0123456789ef:approve"), "{prefix}");
        }
        assert_eq!(command_to_callback("/status"), None);
        assert_eq!(command_to_callback("hello"), None);
    }

    #[test]
    fn actions_buttons_match_the_ntfy_json_shape() {
        let buttons = actions_buttons(
            ANSWERS,
            &[
                ("Approve".to_string(), "elev1:abcd0123456789ef:approve".to_string()),
                ("Reject".to_string(), "elev1:abcd0123456789ef:deny".to_string()),
            ],
        )
        .expect("two buttons must render");
        assert_eq!(buttons.len(), 2);
        let approve = &buttons[0];
        assert_eq!(approve["action"], "http");
        assert_eq!(approve["label"], "Approve");
        assert_eq!(approve["url"], ANSWERS);
        assert_eq!(approve["method"], "POST");
        assert_eq!(approve["body"], "elev1:abcd0123456789ef:approve");
        assert_eq!(approve["clear"], true);
        assert_eq!(buttons[1]["body"], "elev1:abcd0123456789ef:deny");
        assert!(actions_buttons(ANSWERS, &[]).is_none(), "no buttons ⇒ no actions");
    }

    #[test]
    fn actions_are_capped_at_three() {
        let buttons: Vec<(String, String)> = (0..5)
            .map(|i| (format!("B{i}"), format!("flow{i}:abcd0123456789ef:approve")))
            .collect();
        let rendered = actions_buttons(ANSWERS, &buttons).expect("buttons present");
        assert_eq!(rendered.len(), MAX_ACTIONS, "ntfy renders three actions at most");
    }

    #[test]
    fn message_line_becomes_a_text_update() {
        let state = SharedState::new();
        let line = r#"{"id":"TTQ1x","time":1795217600,"event":"message","topic":"gate-control","message":"/status"}"#;
        let update = update_from_line(&state, line).expect("plain message ⇒ text update");
        assert_eq!(update.text.as_deref(), Some("/status"));
        assert_eq!(update.sender_id, Some(SENDER_ID));
        assert!(update.callback.is_none());
        assert_eq!(update.update_id, 1);
    }

    #[test]
    fn button_body_becomes_a_callback_update_without_text() {
        let state = SharedState::new();
        let line = r#"{"id":"TTQ1y","time":1795217601,"event":"message","topic":"gate-answers-91bd","message":"elev1:abcd0123456789ef:approve"}"#;
        let update = update_from_line(&state, line).expect("bare triple ⇒ callback");
        let cb = update.callback.as_ref().expect("callback present");
        assert_eq!(cb.data, "elev1:abcd0123456789ef:approve");
        assert_eq!(cb.from_id, SENDER_ID);
        assert_eq!(cb.id, "ntfy");
        assert_eq!(update.text, None, "callback updates carry no text");
        assert_eq!(update.sender_id, None, "callback updates carry no sender");
        assert_eq!(update.update_id, 1);
    }

    #[test]
    fn written_ok_command_also_becomes_a_callback() {
        let state = SharedState::new();
        let line = r#"{"id":"TTQ1z","time":1795217602,"event":"message","topic":"gate-control","message":"/ok elev1:abcd0123456789ef:approve"}"#;
        let update = update_from_line(&state, line).expect("command ⇒ callback");
        assert_eq!(
            update.callback.as_ref().unwrap().data,
            "elev1:abcd0123456789ef:approve"
        );
        assert_eq!(update.text, None);
    }

    #[test]
    fn non_message_events_empty_and_garbage_lines_are_skipped() {
        let state = SharedState::new();
        assert_eq!(
            update_from_line(&state, r#"{"id":"x","event":"open"}"#),
            None,
            "open event"
        );
        assert_eq!(
            update_from_line(&state, r#"{"id":"x","event":"keepalive"}"#),
            None,
            "keepalive"
        );
        assert_eq!(
            update_from_line(&state, r#"{"id":"x","event":"message","topic":"t","title":"only a title"}"#),
            None,
            "no message body"
        );
        assert_eq!(update_from_line(&state, "not json at all"), None, "garbage");
        assert_eq!(update_from_line(&state, ""), None, "empty line");
    }

    #[test]
    fn update_ids_are_sequential() {
        let state = SharedState::new();
        let line = r#"{"id":"a","event":"message","topic":"t","message":"one"}"#;
        let a = update_from_line(&state, line).expect("first");
        let b = update_from_line(&state, line).expect("second");
        assert_eq!((a.update_id, b.update_id), (1, 2));
    }
}
