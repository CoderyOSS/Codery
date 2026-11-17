//! Telegram approval delivery (GATE-005/006): the real [`ApprovalBackend`]
//! behind the git/API proxies when the bot is configured. `present` sends
//! the ask to the control-plane chat with signed `[Approve]`/`[Deny]`
//! buttons (`callback_data = "{pending_id}:{nonce}:{action}"`, nonce stored
//! on the pending row); `wait` polls the row the core proxies already race;
//! `resolve` and the button handler meet in the same idempotent row write,
//! so a press and a programmatic resolve can never double-apply.
//!
//! Review-focus rule (GATE-006): a decision only lands when the button's
//! nonce matches the row's stored nonce AND the row is still unresolved.
//! Anything else — unknown id, already decided, already timed out — edits
//! a reply stating the current state and changes nothing.
//!
//! Auditing (binding ruling): when a Telegram user resolves a hold, the
//! audit event carries `approver` = their Telegram user id (string) and
//! `basis` = the outcome; the core [`ApprovalBackend`] trait signature is
//! unchanged. Programmatic resolves keep the core audit shape.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;

use crate::approval::{
    ensure_row, now_ms, persist_request_with_nonce, resolve_pending_as, row_nonce, row_state,
    store_ask_msg, sweep_unresolved_pending, ApprovalBackend, ApprovalOutcome, ApprovalRequest,
    POLL,
};
use crate::audit::{Audit, AuditEntry};
use crate::state::{self, State};
use crate::telegram::send::{MsgRef, SendRequest};
use crate::telegram::{
    nonce, validate_callback, ACTION_NO_LONGER_VALID, NonceLookup, TelegramApi, Update,
    UpdateHandler,
};

/// Reply to a press on a hold that already timed out: state, no change.
pub const TIMED_OUT_TEXT: &str = "This request already timed out.";

/// Notice edited into the ask message when its own deadline passes.
const TIMED_OUT_NOTICE: &str =
    "No decision arrived in time — the held operation was rejected.";

/// The approval backend wired behind the core trait when the bot runs:
/// holds land on the operator's phone, decisions come back as signed
/// button presses.
pub struct TelegramApprovalBackend {
    state: Arc<Mutex<State>>,
    audit: Arc<Audit>,
    api: Arc<dyn TelegramApi>,
    /// Chat the asks go to (`TELEGRAM_CHAT_ID`). `None` means asks cannot
    /// be delivered: `present` errors and the proxy denies the hold —
    /// fail closed.
    ask_chat: Option<i64>,
}

impl TelegramApprovalBackend {
    /// Build the backend, sweeping every unresolved hold first (GATE-005,
    /// same as the stub backend: on init the restart resolves rows without
    /// a live waiter as `TimedOut` — no post-restart press may resurrect a
    /// hold whose consumer is gone).
    pub fn new(
        state: Arc<Mutex<State>>,
        audit: Arc<Audit>,
        api: Arc<dyn TelegramApi>,
        ask_chat: Option<i64>,
    ) -> Result<Self> {
        {
            let guard = state.lock().expect("state lock must be poison-free");
            sweep_unresolved_pending(&guard, &audit)?;
        }
        Ok(Self {
            state,
            audit,
            api,
            ask_chat,
        })
    }

    /// The nonce resolver for the router's callback validation: flow id is
    /// the pending row id, nonce lives on the row. Existing-but-decided
    /// rows still resolve, so a late press reaches the handler and gets a
    /// state reply instead of a generic rejection; unknown rows fail
    /// closed at the router.
    pub fn nonce_lookup(state: Arc<Mutex<State>>) -> NonceLookup {
        Arc::new(move |flow| {
            let guard = state.lock().expect("state lock must be poison-free");
            row_nonce(&guard, flow)
        })
    }

    /// Resolve on behalf of a button presser: persist the decision (with
    /// the approver ruling), then reply on the pressed message — decision
    /// text when the row changed, current-state text when it was already
    /// decided (never mutating it). The stored ask message is updated too
    /// when the press came from a different message (e.g. /pending).
    pub async fn resolve_from_button(
        &self,
        id: &str,
        outcome: ApprovalOutcome,
        approver: i64,
        pressed: MsgRef,
        api: Arc<dyn TelegramApi>,
    ) {
        let (changed, settled, repo, class) = {
            let guard = self.state.lock().expect("state lock must be poison-free");
            let changed =
                resolve_pending_as(&guard, &self.audit, id, outcome, Some(&approver.to_string()), None)
                    .unwrap_or(false);
            let settled = row_state(&guard, id).2;
            let (repo, class) = match crate::approval::payload_of(&guard, id) {
                Some(p) => (p.repo, p.class),
                None => ("-".to_string(), "unknown".to_string()),
            };
            (changed, settled, repo, class)
        };
        let reply = match (changed, settled) {
            (true, Some(o)) => decision_text(o, Some(approver), &repo, &class),
            (false, Some(o)) => state_text(o),
            // Absent rows cannot be resolved: the nonce validation above
            // already failed closed — this is the belt-and-braces reply.
            (_, None) => ACTION_NO_LONGER_VALID.to_string(),
        };
        let _ = api
            .edit_message(pressed, SendRequest::text(reply))
            .await;
        if changed {
            if let Some(o) = settled {
                let ask = {
                    let guard = self.state.lock().expect("state lock must be poison-free");
                    crate::approval::ask_msg_of(&guard, id).map(|(chat_id, message_id)| MsgRef {
                        chat_id,
                        message_id,
                    })
                };
                if let Some(ask_ref) = ask {
                    if ask_ref != pressed {
                        let _ = api
                            .edit_message(
                                ask_ref,
                                SendRequest::text(decision_text(o, Some(approver), &repo, &class)),
                            )
                            .await;
                    }
                }
            }
        }
    }

    /// Re-deliver the ask for an unresolved hold (the /pending Resume
    /// button): fresh message, same pending id + nonce, so the original
    /// signed buttons keep working. Decided holds get a state reply.
    pub async fn resume(&self, id: &str, pressed: MsgRef, api: Arc<dyn TelegramApi>) {
        let payload = {
            let guard = self.state.lock().expect("state lock must be poison-free");
            crate::approval::payload_of(&guard, id)
        };
        let Some(payload) = payload else {
            let _ = api
                .edit_message(pressed, SendRequest::text(ACTION_NO_LONGER_VALID))
                .await;
            return;
        };
        let decided = {
            let guard = self.state.lock().expect("state lock must be poison-free");
            row_state(&guard, id).2
        };
        if let Some(o) = decided {
            let _ = api
                .edit_message(pressed, SendRequest::text(state_text(o)))
                .await;
            return;
        }
        let Some(n) = payload.nonce.clone() else {
            let _ = api
                .edit_message(
                    pressed,
                    SendRequest::text("This hold has no signed buttons to resume."),
                )
                .await;
            return;
        };
        let ask = SendRequest::text(render_ask(&payload.agent, &payload.repo, &payload.class, &payload.detail))
            .with_inline_button("Approve", id, &n, "approve")
            .with_inline_button("Deny", id, &n, "deny")
            .replying_to(pressed);
        match api.send_message(ask).await {
            Ok(sent) => {
                let guard = self.state.lock().expect("state lock must be poison-free");
                let _ = store_ask_msg(&guard, id, sent.chat_id, sent.message_id);
            }
            Err(e) => {
                let _ = api
                    .edit_message(pressed, SendRequest::text(format!("Resume failed: {e}")))
                    .await;
            }
        }
    }

    /// Edit the stored ask message in place to show the decision (and the
    /// approver when one exists). Best-effort: an edit failure must never
    /// fail the resolution itself.
    async fn announce(&self, id: &str, outcome: ApprovalOutcome, approver: Option<i64>) {
        if !matches!(outcome, ApprovalOutcome::Approved | ApprovalOutcome::Denied) {
            return;
        }
        let (msg, repo, class) = {
            let guard = self.state.lock().expect("state lock must be poison-free");
            let msg = crate::approval::ask_msg_of(&guard, id);
            let (repo, class) = match crate::approval::payload_of(&guard, id) {
                Some(p) => (p.repo, p.class),
                None => ("-".to_string(), "unknown".to_string()),
            };
            (msg, repo, class)
        };
        if let Some((chat_id, message_id)) = msg {
            let _ = self
                .api
                .edit_message(
                    MsgRef {
                        chat_id,
                        message_id,
                    },
                    SendRequest::text(decision_text(outcome, approver, &repo, &class)),
                )
                .await;
        }
    }
}

#[async_trait]
impl ApprovalBackend for TelegramApprovalBackend {
    /// Deliver the ask: the chat must be configured BEFORE anything is
    /// persisted, and a send failure immediately resolves the row `Denied`
    /// (`delivery-failed`) — an ask that cannot be delivered must never
    /// leave a resolvable row behind, because `/pending`'s Resume would
    /// resurrect it into a phantom-approvable hold with no live waiter.
    async fn present(&self, req: &ApprovalRequest) -> Result<()> {
        // (1) Config check first: no chat ⇒ no row, Err ⇒ the proxy
        // already maps present-Err to deny + audit ("backend unavailable").
        let chat = self.ask_chat.ok_or_else(|| {
            anyhow!(
                "no approval chat configured; ask cannot be delivered — hold is denied fail-closed"
            )
        })?;
        let n = nonce();
        {
            let guard = self.state.lock().expect("state lock must be poison-free");
            persist_request_with_nonce(&guard, req, Some(&n))?;
        }
        self.audit.record(AuditEntry {
            ts: state::now(),
            agent: Some(req.agent.clone()),
            repo: req.repo.clone(),
            action: "approval.request".to_string(),
            decision: "pending".to_string(),
            basis: "hold pending human decision".to_string(),
            approver: None,
        });
        let ask = SendRequest::text(render_ask(
            &req.agent,
            &req.repo,
            crate::approval::class_str(req.class),
            &crate::approval::detail_value(&req.detail),
        ))
        .with_inline_button("Approve", &req.id, &n, "approve")
        .with_inline_button("Deny", &req.id, &n, "deny")
        // message_id 0 anchors a fresh send into the ask chat (no reply).
        .replying_to(MsgRef {
            chat_id: chat,
            message_id: 0,
        });
        let sent = match self.api.send_message(ask).await {
            Ok(sent) => sent,
            // Send failure ⇒ roll the hold back fail-closed: the row
            // resolves Denied (basis `delivery-failed`), the delivery
            // failure itself is audited, and Err denies the hold. The
            // transport's errors are token-free summaries (Task 1
            // redaction) and audit record() scrubs secret-shaped material
            // besides.
            Err(e) => {
                {
                    let guard = self.state.lock().expect("state lock must be poison-free");
                    // Idempotent: if the row somehow settled already, the
                    // first decision stands.
                    let _ = resolve_pending_as(
                        &guard,
                        &self.audit,
                        &req.id,
                        ApprovalOutcome::Denied,
                        None,
                        Some("delivery-failed"),
                    );
                }
                self.audit.record(AuditEntry {
                    ts: state::now(),
                    agent: Some(req.agent.clone()),
                    repo: req.repo.clone(),
                    action: "approval.delivery".to_string(),
                    decision: "deny".to_string(),
                    basis: format!("delivery failed: {e}"),
                    approver: None,
                });
                return Err(anyhow!("approval ask delivery failed: {e}"));
            }
        };
        {
            let guard = self.state.lock().expect("state lock must be poison-free");
            store_ask_msg(&guard, &req.id, sent.chat_id, sent.message_id)?;
        }
        Ok(())
    }

    /// Poll the pending row until a decision lands or the row's own
    /// deadline passes (the row — not the waiter — is the truth). At the
    /// deadline the hold resolves `TimedOut` and the ask is marked in
    /// place, best-effort.
    async fn wait(&self, id: &str) -> ApprovalOutcome {
        {
            let guard = self.state.lock().expect("state lock must be poison-free");
            ensure_row(&guard, id);
        }
        loop {
            let (created_ms, timeout_ms, decided) = {
                let guard = self.state.lock().expect("state lock must be poison-free");
                row_state(&guard, id)
            };
            if let Some(outcome) = decided {
                return outcome;
            }
            let deadline_ms = created_ms.saturating_add(timeout_ms);
            if now_ms() >= deadline_ms {
                {
                    let guard = self.state.lock().expect("state lock must be poison-free");
                    // Best-effort, matching the core stub: the row write is
                    // the durable truth; a DB error leaves the row for the
                    // restart sweep.
                    let _ = resolve_pending_as(
                        &guard,
                        &self.audit,
                        id,
                        ApprovalOutcome::TimedOut,
                        None,
                        None,
                    );
                }
                let notice = {
                    let guard = self.state.lock().expect("state lock must be poison-free");
                    crate::approval::ask_msg_of(&guard, id)
                };
                if let Some((chat_id, message_id)) = notice {
                    let _ = self
                        .api
                        .edit_message(
                            MsgRef {
                                chat_id,
                                message_id,
                            },
                            SendRequest::text(TIMED_OUT_NOTICE),
                        )
                        .await;
                }
                return ApprovalOutcome::TimedOut;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Programmatic resolve (no Telegram approver): the idempotent core
    /// row write, plus an in-place decision edit on the ask message.
    async fn resolve(&self, id: &str, outcome: ApprovalOutcome) -> Result<()> {
        let changed = {
            let guard = self.state.lock().expect("state lock must be poison-free");
            resolve_pending_as(&guard, &self.audit, id, outcome, None, None)?
        };
        if changed {
            self.announce(id, outcome, None).await;
        }
        Ok(())
    }
}

/// Short display sha (first 7 hex chars).
fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// Human-readable ask per GATE-006: what's happening (repo, class, agent),
/// the operation's shape (git refs old→new short-sha + commit count, or
/// method + path + body snippet), and exactly what happens without a
/// decision. No secrets: shas are truncated, the snippet is body content
/// only.
fn render_ask(agent: &str, repo: &str, class: &str, detail: &Value) -> String {
    format!(
        "Approval needed: {class} to {repo} (agent {agent}).\n{}\nNo decision ⇒ the hold times out and the operation is rejected.",
        render_detail(detail)
    )
}

fn render_detail(detail: &Value) -> String {
    let mut lines = Vec::new();
    if let Some(updates) = detail.get("updates").and_then(Value::as_array) {
        for u in updates {
            let old = u.get("old").and_then(Value::as_str).unwrap_or("-");
            let new = u.get("new").and_then(Value::as_str).unwrap_or("-");
            let refname = u.get("refname").and_then(Value::as_str).unwrap_or("-");
            lines.push(format!("{refname} {}→{}", short(old), short(new)));
        }
        let commits = detail.get("commits").and_then(Value::as_array);
        let count = commits.map(Vec::len).unwrap_or(0);
        lines.push(format!("{count} new commit(s)"));
        if let Some(commits) = commits {
            for c in commits.iter().take(5) {
                if let Some(subject) = c.as_str() {
                    lines.push(format!("  - {subject}"));
                }
            }
        }
    } else {
        let method = detail.get("method").and_then(Value::as_str).unwrap_or("-");
        let path = detail.get("path").and_then(Value::as_str).unwrap_or("-");
        let snippet = detail.get("body_snippet").and_then(Value::as_str).unwrap_or("");
        lines.push(format!("{method} {path}"));
        if !snippet.is_empty() {
            lines.push(format!("body: {snippet}"));
        }
    }
    lines.join("\n")
}

/// Decision edit text: the outcome plus who made it.
fn decision_text(outcome: ApprovalOutcome, approver: Option<i64>, repo: &str, class: &str) -> String {
    let by = approver
        .map(|id| format!(" by user {id}"))
        .unwrap_or_default();
    match outcome {
        ApprovalOutcome::Approved => format!("Approved{by}: {class} to {repo} will proceed."),
        ApprovalOutcome::Denied => format!("Denied{by}: {class} to {repo} is rejected."),
        ApprovalOutcome::TimedOut => TIMED_OUT_TEXT.to_string(),
    }
}

/// Current-state reply for presses on decided holds — never a mutation.
fn state_text(outcome: ApprovalOutcome) -> String {
    match outcome {
        ApprovalOutcome::TimedOut => TIMED_OUT_TEXT.to_string(),
        o => format!("Already decided: {}", o.as_str()),
    }
}

/// Parse `"id:action"` out of a validated `callback_data` triple (the nonce
/// is validated separately against the row).
fn split_flow(data: &str) -> Option<(String, String)> {
    let mut parts = data.splitn(3, ':');
    let id = parts.next()?.to_string();
    let _nonce = parts.next()?;
    let action = parts.next()?.to_string();
    if id.is_empty() || action.is_empty() {
        return None;
    }
    Some((id, action))
}

/// The router handler for approval buttons: approve/deny/cancel resolve the
/// hold (approver = presser), resume re-delivers the ask. Runs only for
/// nonce-validated callbacks (the router filters first); the row's nonce is
/// re-checked here so a row that vanished between validation and handling
/// still fails closed.
struct ApprovalCallbacks {
    backend: Arc<TelegramApprovalBackend>,
}

#[async_trait]
impl UpdateHandler for ApprovalCallbacks {
    async fn handle(&self, update: Update, api: Arc<dyn TelegramApi>) {
        let Some(cb) = update.callback else {
            return;
        };
        let Some(to) = update.message_ref else {
            return;
        };
        let Some((id, action)) = split_flow(&cb.data) else {
            let _ = api
                .edit_message(to, SendRequest::text(ACTION_NO_LONGER_VALID))
                .await;
            return;
        };
        let expected = {
            let guard = self.backend.state.lock().expect("state lock must be poison-free");
            row_nonce(&guard, &id)
        };
        let Some(expected) = expected else {
            // Row gone (sweep/corruption): no longer valid, state unchanged.
            let _ = api.answer_callback(&cb.id).await;
            let _ = api
                .edit_message(to, SendRequest::text(ACTION_NO_LONGER_VALID))
                .await;
            return;
        };
        if validate_callback(&cb.data, &expected).is_none() {
            let _ = api.answer_callback(&cb.id).await;
            let _ = api
                .edit_message(to, SendRequest::text(ACTION_NO_LONGER_VALID))
                .await;
            return;
        }
        match action.as_str() {
            "approve" => {
                self.backend
                    .resolve_from_button(&id, ApprovalOutcome::Approved, cb.from_id, to, api)
                    .await;
            }
            "deny" | "cancel" => {
                self.backend
                    .resolve_from_button(&id, ApprovalOutcome::Denied, cb.from_id, to, api)
                    .await;
            }
            "resume" => self.backend.resume(&id, to, api).await,
            _ => {
                let _ = api.edit_message(to, SendRequest::text("Unknown action.")).await;
            }
        }
    }
}

/// Wire the approval layer onto a router: the row-backed nonce lookup for
/// callback validation plus the button handler.
pub fn register_approvals(
    router: &mut crate::telegram::TelegramRouter,
    backend: Arc<TelegramApprovalBackend>,
) {
    router.set_nonce_lookup(TelegramApprovalBackend::nonce_lookup(backend.state.clone()));
    router.register(Arc::new(ApprovalCallbacks { backend }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telegram::fake::FakeTelegram;
    use serde_json::json;

    /// Backend construction sweeps unresolved holds fail-closed (GATE-005),
    /// exactly like the stub backend does.
    #[test]
    fn new_sweeps_unresolved_holds_from_before_the_restart() {
        let dir = std::env::temp_dir().join(format!(
            "gate-tg-sweep-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let audit_path = dir.join("audit.jsonl");
        let audit = Arc::new(Audit::open(&audit_path));
        let state = Arc::new(Mutex::new(State::open(":memory:").unwrap()));

        // A hold left unresolved by the "previous run": after restart, no
        // waiter exists, so it must resolve TimedOut before anything else.
        {
            let guard = state.lock().unwrap();
            guard
                .conn()
                .execute(
                    "INSERT INTO pending(id, kind, payload_json, created_at, decided_at, outcome)
                     VALUES ('hold-pre-restart', 'approval.push', '{}', 0, NULL, NULL)",
                    [],
                )
                .unwrap();
        }

        let api: Arc<dyn TelegramApi> = Arc::new(FakeTelegram::new());
        TelegramApprovalBackend::new(state.clone(), audit.clone(), api, Some(1)).unwrap();
        let outcome = {
            let guard = state.lock().unwrap();
            row_state(&guard, "hold-pre-restart").2
        };
        assert_eq!(outcome, Some(ApprovalOutcome::TimedOut), "fail closed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_ask_names_repo_class_refs_and_consequence() {
        let detail = json!({
            "updates": [
                {"old": "1234567890abcdef1234567890abcdef12345678",
                 "new": "fedcba0987654321fedcba0987654321fedcba09",
                 "refname": "refs/heads/main"}
            ],
            "commits": ["add feature", "fix bug"],
        });
        let text = render_ask("claude-code", "octo/hello", "force_push", &detail);
        assert!(text.contains("octo/hello"), "{text}");
        assert!(text.contains("force_push"), "{text}");
        assert!(text.contains("refs/heads/main"), "{text}");
        assert!(text.contains("1234567→"), "{text}");
        assert!(text.contains("fedcba0"), "{text}");
        assert!(text.contains("2 new commit(s)"), "{text}");
        assert!(text.contains("- add feature"), "{text}");
        assert!(text.contains("times out"), "{text}");
    }

    #[test]
    fn render_ask_api_shows_method_path_snippet_but_no_digest() {
        let detail = json!({
            "method": "DELETE",
            "path": "/repos/octo/hello/hooks/1",
            "body_snippet": "{}",
            "body_sha": "ab12cd34",
        });
        let text = render_ask("codex-cli", "octo/hello", "push", &detail);
        assert!(text.contains("DELETE /repos/octo/hello/hooks/1"), "{text}");
        assert!(text.contains("body: {}"), "{text}");
        // The digest is metadata, not something an approver needs.
        assert!(!text.contains("ab12cd34"), "{text}");
    }

    #[test]
    fn decision_and_state_texts_cover_the_review_focus_matrix() {
        assert_eq!(
            decision_text(ApprovalOutcome::Approved, Some(42), "octo/hello", "force_push"),
            "Approved by user 42: force_push to octo/hello will proceed."
        );
        assert_eq!(
            decision_text(ApprovalOutcome::Denied, None, "octo/hello", "push"),
            "Denied: push to octo/hello is rejected."
        );
        assert_eq!(state_text(ApprovalOutcome::TimedOut), TIMED_OUT_TEXT);
        assert_eq!(
            state_text(ApprovalOutcome::Approved),
            "Already decided: approved"
        );
    }

    #[test]
    fn split_flow_extracts_id_and_action_from_validated_triple() {
        assert_eq!(
            split_flow("hold1:abcd0123456789ef:approve"),
            Some(("hold1".to_string(), "approve".to_string()))
        );
        assert_eq!(split_flow("hold1"), None);
        assert_eq!(split_flow(":abcd0123456789ef:approve"), None);
        assert_eq!(split_flow("hold1:abcd0123456789ef:"), None);
    }
}
