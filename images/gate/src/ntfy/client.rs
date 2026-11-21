//! NtfyClient transport (GATE-013): the ntfy half of the control plane,
//! implemented behind the [`crate::control::ControlApi`] trait. ALL HTTP
//! lives in this file; the access token reaches no log line (kind words
//! only in errors, mirroring the GATE-006 redaction discipline).
//!
//! - `send_message` → JSON publish to the control topic; ask buttons
//!   ([`SendRequest::inline_buttons`]) become ntfy HTTP actions that POST
//!   their signed triple to the answers topic — decisions from the lock
//!   screen.
//! - `get_updates` → JSON-stream subscription of BOTH gated topics
//!   (operator compositions on the control topic, button posts on the
//!   answers topic), resumed from the last seen event id so a dropped
//!   stream loses nothing the broker still caches.
//! - ntfy has no edit/delete: edits fall back to fresh sends (the ADR
//!   fallback), deletions and callback acknowledgements are no-ops.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use super::{
    actions_buttons, update_from_line, CONTROL_TOPIC_ID, SharedState,
};
use crate::config::NtfyConfig;
use crate::control::{MsgRef, SendRequest, Update};

/// Strip HTML tags from a message authored with `SendRequest::html`
/// (GATE-011 copy is Telegram-HTML): ntfy renders markup literally, so
/// the phone must receive plain text (I3).
fn strip_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Publish-body builder (pure, testable): control topic, message text
/// (HTML-stripped when authored as HTML), and ask buttons as ntfy HTTP
/// actions posting to the answers topic.
fn publish_body(req: &SendRequest, cfg: &NtfyConfig) -> Value {
    let text = if req.parse_mode.as_deref() == Some("HTML") {
        strip_html(&req.text)
    } else {
        req.text.clone()
    };
    let mut body = json!({
        "topic": cfg.topic,
        "message": text,
    });
    let answers_url = format!("{}/{}", cfg.url, cfg.answers_topic);
    if let Some(actions) = actions_buttons(&answers_url, &req.inline_buttons) {
        body["actions"] = Value::Array(actions);
    }
    body
}

/// The event id of a MESSAGE stream line — the only ids that are cache
/// positions (M3). Keepalive/open ids are not resumable anchors and do
/// not advance the cursor.
fn cursor_of(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("event").and_then(Value::as_str) != Some("message") {
        return None;
    }
    v.get("id").and_then(Value::as_str).map(String::from)
}

/// The live transport. One reqwest client, no overall timeout (the
/// stream call is a long-poll by design); the cursor is the last event
/// id seen, so reconnects resume exactly where the stream dropped.
pub struct NtfyClient {
    http: reqwest::Client,
    cfg: NtfyConfig,
    cursor: Mutex<String>,
    state: Arc<SharedState>,
}

/// Health-check the broker and arm the transport. Failure aborts boot
/// (fail closed) — a broker the gate cannot see is no control channel.
pub async fn connect(cfg: &NtfyConfig) -> Result<NtfyClient> {
    let http = reqwest::Client::builder()
        .build()
        .map_err(|_| anyhow!("ntfy: client build failed"))?;
    let health = http
        .get(format!("{}/v1/health", cfg.url))
        .bearer_auth(&cfg.token)
        .send()
        .await
        .map_err(|_| anyhow!("ntfy: broker unreachable at {} — check NTFY_URL", cfg.url))?;
    if !health.status().is_success() {
        return Err(anyhow!(
            "ntfy: broker health returned {} — check NTFY_TOKEN",
            health.status().as_u16()
        ));
    }
    Ok(NtfyClient {
        http,
        cfg: cfg.clone(),
        cursor: Mutex::new(String::new()),
        state: Arc::new(SharedState::new()),
    })
}

impl NtfyClient {
    /// JSON publish to the control topic. Buttons become actions; the
    /// answers topic never sees a direct publish from the gate.
    async fn publish(&self, req: &SendRequest) -> Result<MsgRef> {
        let response = self
            .http
            .post(&self.cfg.url)
            .bearer_auth(&self.cfg.token)
            .json(&publish_body(req, &self.cfg))
            .send()
            .await
            .map_err(|_| anyhow!("ntfy publish: transport error"))?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "ntfy publish: status {} — check NTFY_TOKEN",
                response.status().as_u16()
            ));
        }
        Ok(MsgRef {
            chat_id: CONTROL_TOPIC_ID,
            message_id: 0,
        })
    }

    /// Held-open JSON stream over both gated topics (comma-separated
    /// topics, one stream): blocks until a message event arrives, then
    /// returns it. NO `poll=1` — a poll returns the cache and closes,
    /// which would turn every quiet moment into an error-driven backoff
    /// (I1). The cursor advances on MESSAGE events only (keepalive/open
    /// ids are not cache positions) and is seeded with a 1-second
    /// duration anchor at boot, so a fresh gate does not replay the
    /// broker's cache; reconnects resume exactly after the last seen
    /// message.
    async fn next_update(&self) -> Result<Update> {
        let cursor = self.cursor.lock().expect("cursor lock").clone();
        let since = if cursor.is_empty() {
            "1s".to_string()
        } else {
            cursor
        };
        let url = format!(
            "{}/{},{}?since={since}",
            self.cfg.url, self.cfg.topic, self.cfg.answers_topic
        );
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.cfg.token)
            .send()
            .await
            .map_err(|_| anyhow!("ntfy stream: transport error"))?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "ntfy stream: status {} — check NTFY_TOKEN",
                response.status().as_u16()
            ));
        }
        let mut buffer: Vec<u8> = Vec::new();
        let mut response = response;
        loop {
            let chunk = response
                .chunk()
                .await
                .map_err(|_| anyhow!("ntfy stream: transport error"))?
                .ok_or_else(|| anyhow!("ntfy stream: closed"))?;
            buffer.extend_from_slice(&chunk);
            while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = buffer.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]);
                if let Some(id) = cursor_of(&line) {
                    *self.cursor.lock().expect("cursor lock") = id;
                }
                if let Some(update) = update_from_line(&self.state, &line) {
                    return Ok(update);
                }
            }
        }
    }
}

#[async_trait]
impl crate::control::ControlApi for NtfyClient {
    async fn get_updates(&self, _offset: i64) -> Result<Vec<Update>> {
        Ok(vec![self.next_update().await?])
    }

    /// ntfy has no edits: every send is a fresh publish (GATE-013 ADR
    /// fallback).
    async fn send_message(&self, req: SendRequest) -> Result<MsgRef> {
        self.publish(&req).await
    }

    async fn edit_message(&self, _to: MsgRef, req: SendRequest) -> Result<MsgRef> {
        self.publish(&req).await
    }

    /// ntfy has no callback-query acknowledgement.
    async fn answer_callback(&self, _callback_id: &str) -> Result<()> {
        Ok(())
    }

    /// ntfy has no message deletion.
    async fn delete_message(&self, _to: MsgRef) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::SendRequest;

    fn cfg() -> NtfyConfig {
        NtfyConfig {
            url: "https://ntfy.example.com".to_string(),
            topic: "gate-control-7f3a".to_string(),
            answers_topic: "gate-answers-91bd".to_string(),
            token: "tk_x".to_string(),
        }
    }

    #[test]
    fn publish_body_carries_topic_text_and_actions() {
        let req = SendRequest::text("Tests pass. Ship it?")
            .with_inline_button("Approve", "elev1", "abcd0123456789ef", "approve")
            .with_inline_button("Reject", "elev1", "abcd0123456789ef", "deny");
        let body = publish_body(&req, &cfg());
        assert_eq!(body["topic"], "gate-control-7f3a");
        assert_eq!(body["message"], "Tests pass. Ship it?");
        let actions = body["actions"].as_array().expect("actions present");
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0]["label"], "Approve");
        assert_eq!(
            actions[0]["url"],
            "https://ntfy.example.com/gate-answers-91bd",
            "buttons POST to the answers topic"
        );
        assert_eq!(actions[0]["body"], "elev1:abcd0123456789ef:approve");
        assert_eq!(actions[1]["body"], "elev1:abcd0123456789ef:deny");
    }

    #[test]
    fn publish_body_without_buttons_has_no_actions_key() {
        let body = publish_body(&SendRequest::text("all quiet"), &cfg());
        assert!(body.get("actions").is_none(), "no buttons ⇒ no actions key");
        assert_eq!(body["topic"], "gate-control-7f3a");
    }

    #[test]
    fn html_authored_messages_publish_tag_free() {
        let req = SendRequest::html("<b>Email</b> — use a <code>private window</code>");
        let body = publish_body(&req, &cfg());
        let message = body["message"].as_str().expect("message present");
        assert!(!message.contains('<'), "no markup reaches ntfy: {message}");
        assert!(message.contains("Email"), "text survives the strip: {message}");
        assert!(message.contains("private window"), "text survives the strip: {message}");
    }

    #[test]
    fn cursor_of_reads_any_event_kind() {
        assert_eq!(
            cursor_of(r#"{"id":"TTQ1x","event":"message","message":"hi"}"#).as_deref(),
            Some("TTQ1x")
        );
        assert_eq!(
            cursor_of(r#"{"id":"TTQ1y","event":"keepalive"}"#),
            None,
            "keepalive ids are not cache positions (M3)"
        );
        assert_eq!(cursor_of("garbage"), None);
    }
}
