//! Matrix control-channel adapter core (GATE-012) — the pure pieces both
//! the transport (client.rs) and its tests build on. One behavioral
//! contract, two transports: everything here mirrors a GATE-006 shape.
//!
//! Transport mapping:
//! - the pinned control room is chat id [`ROOM_CHAT_ID`];
//! - each allowlisted mxid maps to its positional id (1..=n of
//!   `MATRIX_USER_ALLOWLIST`, stable across boots) — the numeric id space
//!   the rest of the control plane already speaks;
//! - inline buttons become text commands embedding the same signed
//!   `"{flow_id}:{nonce}:{action}"` triple (GATE-006's `callback_data`);
//! - an inbound `/ok`/`/no`/`/approve`/`/deny` command carrying a
//!   well-formed triple becomes the same validated [`crate::telegram::Callback`]
//!   update a Telegram button press would produce. The prefix is
//!   ergonomic only — the triple's action word stays authoritative.

use std::collections::HashMap;

pub mod client;

pub use self::client::{connect, validate_room_id, MatrixClient, SharedState};

/// The numeric chat id of the single pinned control room. The room's
/// real identity (`MATRIX_ROOM_ID`) lives only in the transport; the
/// rest of the control plane sees this constant.
pub const ROOM_CHAT_ID: i64 = 1;

/// Positional identity map (GATE-012): allowlisted mxids get ids
/// `1..=n` in allowlist order. Unlisted mxids are absent from the map —
/// the transport drops them before any handler runs (fail closed).
pub fn mxid_map(allowlist: &[String]) -> HashMap<String, i64> {
    allowlist
        .iter()
        .enumerate()
        .map(|(i, mxid)| (mxid.clone(), i as i64 + 1))
        .collect()
}

/// Minimum nonce length for a text command to count as a signed triple —
/// production nonces are 16 hex chars (GATE-006's [`crate::telegram::nonce`]);
/// the floor just keeps garbage like `one:two:three` from looking signed.
const MIN_NONCE_LEN: usize = 8;

/// An inbound `/ok`-family command: the triple it carries, or `None` when
/// the text is not a command (stays plain text for the command router).
/// Prefix matching is case-insensitive (`/OK` works); the remainder must
/// parse as `flow:nonce:action` with all three parts non-empty and a
/// nonce of at least [`MIN_NONCE_LEN`] chars.
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
    let triple = rest.trim();
    let mut parts = triple.split(':');
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
    Some(triple.to_string())
}

/// The command lines rendered for outbound [`crate::telegram::SendRequest`]
/// buttons: one line per button — `` `/ok {data}` `` for `approve`
/// actions, `` `/no {data}` `` for anything else — suffixed ` — {label}`.
/// `None` when there are no inline buttons (link buttons render as
/// Markdown links in the transport, not here).
pub fn render_buttons(buttons: &[(String, String)]) -> Option<String> {
    if buttons.is_empty() {
        return None;
    }
    let lines: Vec<String> = buttons
        .iter()
        .map(|(label, data)| {
            let action = data.split(':').nth(2).unwrap_or("");
            let verb = if action == "approve" { "/ok" } else { "/no" };
            format!("`{verb} {data}` — {label}")
        })
        .collect();
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mxid_map_assigns_positional_ids() {
        let map = mxid_map(&["@a:hs".to_string(), "@b:hs".to_string()]);
        assert_eq!(map.get("@a:hs"), Some(&1));
        assert_eq!(map.get("@b:hs"), Some(&2));
        assert_eq!(map.get("@stranger:hs"), None);
        assert!(mxid_map(&[]).is_empty());
    }

    #[test]
    fn ok_no_approve_deny_all_yield_triple() {
        let triple = "elev1:abcd0123456789ef:approve";
        for prefix in ["/ok", "/no", "/approve", "/deny", "/OK"] {
            let got = command_to_callback(&format!("{prefix} {triple}"))
                .unwrap_or_else(|| panic!("{prefix} must yield the triple"));
            assert_eq!(got, triple);
        }
    }

    #[test]
    fn command_with_malformed_triple_is_plain_text() {
        assert_eq!(command_to_callback("/ok one:two"), None, "two parts");
        assert_eq!(
            command_to_callback("/ok elev1:short:approve"),
            None,
            "nonce below the floor"
        );
        assert_eq!(
            command_to_callback("/ok elev1:abcd0123456789ef:"),
            None,
            "empty action"
        );
        assert_eq!(
            command_to_callback("/ok elev1:abcd0123456789ef:approve:extra"),
            None,
            "four parts"
        );
        assert_eq!(command_to_callback("hello"), None, "no prefix");
        assert_eq!(command_to_callback("/ok"), None, "no argument");
        assert_eq!(
            command_to_callback("/restart elev1:abcd0123456789ef:approve"),
            None,
            "unknown prefix stays a command"
        );
    }

    #[test]
    fn render_buttons_labels_approve_and_deny() {
        let rendered = render_buttons(&[
            ("Approve".to_string(), "elev1:N:approve".to_string()),
            ("Deny".to_string(), "elev1:N:deny".to_string()),
        ])
        .expect("two buttons must render");
        let mut lines = rendered.lines();
        let approve = lines.next().expect("approve line");
        let deny = lines.next().expect("deny line");
        assert!(approve.starts_with("`/ok elev1:N:approve`"), "{approve}");
        assert!(approve.ends_with("— Approve"), "{approve}");
        assert!(deny.starts_with("`/no elev1:N:deny`"), "{deny}");
        assert!(deny.ends_with("— Deny"), "{deny}");
        assert!(lines.next().is_none(), "exactly two lines");

        assert!(render_buttons(&[]).is_none(), "no buttons ⇒ nothing");
    }
}
