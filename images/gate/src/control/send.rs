//! Outbound Telegram message types and button helpers (GATE-006). Every
//! control-plane reply is a [`SendRequest`]: plain text plus optional
//! inline keyboard rows. Buttons come in two flavours — link buttons (URL)
//! and callback buttons, whose `callback_data` is the signed triple
//! `"{flow_id}:{nonce}:{action}"` validated server-side by
//! [`crate::control::validate_callback`].

use serde_json::{json, Value};

/// Address of one Telegram message: the chat it lives in and its id
/// inside that chat. Doubles as the reply anchor: replying pins the
/// follow-up to the same thread (and thereby the same chat).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsgRef {
    pub chat_id: i64,
    pub message_id: i32,
}

/// Wire format of a signed callback button: `"{flow_id}:{nonce}:{action}"`.
/// The nonce is the per-flow [`crate::control::nonce()`] value; the
/// control plane validates it before honouring any action.
pub fn callback_data(flow_id: &str, nonce: &str, action: &str) -> String {
    format!("{flow_id}:{nonce}:{action}")
}

/// One outbound message: text plus optional buttons and reply anchor.
/// The target chat travels in `reply_to` — control-plane traffic always
/// replies inside the thread the update came from, and the transport
/// takes the `chat_id` for `sendMessage` from that anchor.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SendRequest {
    pub text: String,
    /// Telegram parse mode for `text` — `Some("HTML")` from
    /// [`SendRequest::html`]; `None` sends plain text exactly as before.
    pub parse_mode: Option<String>,
    /// `(label, url)` pairs rendered as URL buttons.
    pub link_buttons: Vec<(String, String)>,
    /// `(label, callback_data)` pairs rendered as signed callback buttons.
    pub inline_buttons: Vec<(String, String)>,
    /// Message to reply to; also supplies the target chat for sends.
    pub reply_to: Option<MsgRef>,
}

impl SendRequest {
    /// Plain-text message, no buttons, no reply anchor.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Default::default()
        }
    }

    /// HTML-formatted message (`<b>bold</b>` etc.) — sent with Telegram's
    /// HTML parse mode so step copy can bold GitHub's labels (GATE-011).
    /// Callers MUST escape user-derived values (`setup`'s `esc` helper).
    pub fn html(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            parse_mode: Some("HTML".to_string()),
            ..Default::default()
        }
    }

    /// Append a URL button.
    pub fn with_link_button(mut self, label: impl Into<String>, url: impl Into<String>) -> Self {
        self.link_buttons.push((label.into(), url.into()));
        self
    }

    /// Append a signed callback button rendering the
    /// `{flow_id}:{nonce}:{action}` triple.
    pub fn with_inline_button(
        mut self,
        label: impl Into<String>,
        flow_id: &str,
        nonce: &str,
        action: &str,
    ) -> Self {
        self.inline_buttons
            .push((label.into(), callback_data(flow_id, nonce, action)));
        self
    }

    /// Append pre-rendered callback buttons — `(label, callback_data)`
    /// pairs already in signed-triple form (command listings reuse the
    /// nonces stored on rows).
    pub fn buttons(mut self, buttons: Vec<(String, String)>) -> Self {
        self.inline_buttons.extend(buttons);
        self
    }

    /// Pin the message as a reply to `to` (and target its chat on sends).
    ///
    /// transports without one (ntfy, GATE-013).
    pub fn replying_to(mut self, to: MsgRef) -> Self {
        self.reply_to = Some(to);
        self
    }

    /// Telegram `reply_markup` payload: all buttons on one inline-keyboard
    /// row, URL buttons first, callback buttons second. `None` when the
    /// message carries no buttons.
    pub fn reply_markup(&self) -> Option<Value> {
        if self.link_buttons.is_empty() && self.inline_buttons.is_empty() {
            return None;
        }
        let mut buttons = Vec::new();
        for (label, url) in &self.link_buttons {
            buttons.push(json!({ "text": label, "url": url }));
        }
        for (label, data) in &self.inline_buttons {
            buttons.push(json!({ "text": label, "callback_data": data }));
        }
        Some(json!({ "inline_keyboard": [buttons] }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_button_renders_callback_data_triple() {
        let req = SendRequest::text("Push to main needs approval")
            .with_inline_button("Approve", "elev1", "abcd0123456789ef", "approve");
        let markup = req.reply_markup().expect("buttons must render a markup");
        let button = &markup["inline_keyboard"][0][0];
        assert_eq!(button["text"], "Approve");
        assert_eq!(button["callback_data"], "elev1:abcd0123456789ef:approve");
        assert_eq!(
            callback_data("elev1", "abcd0123456789ef", "approve"),
            "elev1:abcd0123456789ef:approve"
        );
    }

    #[test]
    fn link_button_renders_url_and_markup_absent_without_buttons() {
        let req = SendRequest::text("open the gate")
            .with_link_button("Docs", "https://example.com/gate");
        let markup = req.reply_markup().expect("link button must render");
        let button = &markup["inline_keyboard"][0][0];
        assert_eq!(button["text"], "Docs");
        assert_eq!(button["url"], "https://example.com/gate");
        assert!(button.get("callback_data").is_none(), "link buttons carry urls");

        let plain = SendRequest::text("no buttons here");
        assert!(plain.reply_markup().is_none(), "no buttons ⇒ no reply_markup");
    }

    #[test]
    fn html_sets_parse_mode_and_plain_text_leaves_it_off() {
        let rich = SendRequest::html("<b>bold</b>");
        assert_eq!(rich.parse_mode.as_deref(), Some("HTML"));
        // Button helpers must preserve the parse mode.
        let with_buttons = rich.with_link_button("Open", "https://example.com");
        assert_eq!(with_buttons.parse_mode.as_deref(), Some("HTML"));

        let plain = SendRequest::text("plain");
        assert_eq!(plain.parse_mode, None, "legacy plain sends carry no parse mode");
    }

    // -- zero reply anchors must serialize WITHOUT reply parameters (F2) --

        }
