//! Telegram control-plane commands (GATE-006): `/pending` lists unresolved
//! holds with resume/cancel buttons, `/block` and `/unblock` flip the
//! repo kill switch, `/status` renders cheap read-only gate state. All
//! handlers are allowlist-gated upstream (the transport drops strangers
//! before any handler runs); replies edit the command's message in place.
//!
//! `/status` is deliberately P0-cheap: forwarding state (machine
//! credential decrypts), agent-token count, registry rows with roles, and
//! the open-flow count — all local reads, no upstream calls. Unknown or
//! arg-less commands get a short usage reply.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::approval::list_unresolved;
use crate::audit::{Audit, AuditEntry};
use crate::state::{self, State};
use crate::telegram::send::SendRequest;
use crate::telegram::{TelegramApi, Update, UpdateHandler};

/// Reply for commands the gate does not know.
const UNKNOWN_COMMAND: &str =
    "Unknown command. Available: /pending, /status, /block owner/repo, /unblock owner/repo";

/// Usage line for /block and /unblock.
const BLOCK_USAGE: &str = "Usage: /block owner/repo — or /unblock owner/repo";

/// Everything the command handlers read. `machine_token` is the same
/// closure the proxies use: calling it decrypts in memory and decides the
/// forwarding line on /status. No secrets ever render — only the
/// enabled/disabled verdict.
pub struct CommandDeps {
    pub state: Arc<Mutex<State>>,
    pub audit: Arc<Audit>,
    pub machine_token: Arc<dyn Fn() -> Option<String> + Send + Sync>,
}

/// Register the command dispatcher on a router.
pub fn register_commands(router: &mut crate::telegram::TelegramRouter, deps: Arc<CommandDeps>) {
    router.register(Arc::new(Commands { deps }));
}

struct Commands {
    deps: Arc<CommandDeps>,
}

#[async_trait]
impl UpdateHandler for Commands {
    async fn handle(&self, update: Update, api: Arc<dyn TelegramApi>) {
        // Callback updates belong to the approval handler; a reply needs a
        // message to edit in place.
        let Some(text) = update.text else {
            return;
        };
        let Some(to) = update.message_ref else {
            return;
        };
        let mut words = text.trim().split_whitespace();
        let Some(cmd) = words.next().and_then(|w| w.strip_prefix('/')) else {
            return; // plain chat text: no handler action
        };
        let arg = words.next();
        let sender = update.sender_id;
        let reply = match (cmd, arg) {
            ("pending", _) => self.pending_reply(),
            ("block", Some(repo)) => self.block_reply(repo, true, sender),
            ("unblock", Some(repo)) => self.block_reply(repo, false, sender),
            ("block", None) | ("unblock", None) => (BLOCK_USAGE.to_string(), Vec::new()),
            ("status", _) => self.status_reply(),
            _ => (UNKNOWN_COMMAND.to_string(), Vec::new()),
        };
        let _ = api.edit_message(to, SendRequest::text(reply.0).buttons(reply.1)).await;
    }
}

impl Commands {
    /// /pending: every unresolved hold — id, repo, class, age — with
    /// resume/cancel buttons bound to (id, nonce, action).
    fn pending_reply(&self) -> (String, Vec<(String, String)>) {
        let guard = self.deps.state.lock().expect("state lock must be poison-free");
        let rows = match list_unresolved(&guard) {
            Ok(rows) => rows,
            Err(e) => return (format!("pending lookup failed: {e}"), Vec::new()),
        };
        if rows.is_empty() {
            return ("No pending approvals.".to_string(), Vec::new());
        }
        let mut text = format!("Pending approvals ({}):", rows.len());
        let mut buttons = Vec::new();
        for r in &rows {
            text.push_str(&format!(
                "\n- {} {} {} age {}",
                r.id,
                r.repo,
                r.class,
                age(r.created_ms)
            ));
            let Some(nonce) = &r.nonce else {
                continue; // legacy row without signed buttons
            };
            buttons.push(("Resume".to_string(), format!("{}:{nonce}:resume", r.id)));
            buttons.push(("Cancel".to_string(), format!("{}:{nonce}:cancel", r.id)));
        }
        (text, buttons)
    }

    /// /block + /unblock: flip the repo kill switch and confirm in place.
    fn block_reply(&self, repo_arg: &str, blocked: bool, sender: Option<i64>) -> (String, Vec<(String, String)>) {
        let Some(repo) = valid_repo(repo_arg) else {
            return (BLOCK_USAGE.to_string(), Vec::new());
        };
        {
            let guard = self.deps.state.lock().expect("state lock must be poison-free");
            if guard.set_blocked(&repo, blocked).is_err() {
                return ("kill-switch update failed".to_string(), Vec::new());
            }
        }
        self.deps.audit.record(AuditEntry {
            ts: state::now(),
            agent: None,
            repo: repo.clone(),
            action: "killswitch.set".to_string(),
            decision: if blocked { "blocked" } else { "unblocked" }.to_string(),
            basis: "telegram command".to_string(),
            approver: sender.map(|id| id.to_string()),
        });
        let text = if blocked {
            format!("Blocked {repo}. Pushes to it are denied until you run /unblock {repo}.")
        } else {
            format!("Unblocked {repo}. Policy decides pushes to it again.")
        };
        (text, Vec::new())
    }

    /// /status: cheap, read-only, local-only.
    fn status_reply(&self) -> (String, Vec<(String, String)>) {
        let forwarding = (self.deps.machine_token)().is_some();
        let (tokens, flows, registry) = {
            let guard = self.deps.state.lock().expect("state lock must be poison-free");
            let tokens: i64 = guard
                .conn()
                .query_row("SELECT COUNT(*) FROM agent_tokens", [], |r| r.get(0))
                .unwrap_or(0);
            let flows: i64 = guard
                .conn()
                .query_row("SELECT COUNT(*) FROM flows", [], |r| r.get(0))
                .unwrap_or(0);
            (tokens, flows, registry_rows(&guard))
        };
        let mut text = String::from("Gate status\nforwarding: ");
        text.push_str(if forwarding {
            "enabled (machine credential loaded)"
        } else {
            "disabled (no machine credential)"
        });
        text.push_str(&format!("\nagent tokens: {tokens}"));
        text.push_str("\nregistry:");
        if registry.is_empty() {
            text.push_str(" (none)");
        }
        for (repo, granted, intended, via) in &registry {
            text.push_str(&format!(
                "\n- {repo}: granted {granted} (intended {intended}) via {via}"
            ));
        }
        text.push_str(&format!("\nopen flows: {flows}"));
        (text, Vec::new())
    }
}

/// Registry rows for /status; any DB hiccup renders as "no rows" — status
/// is informational and must never panic.
fn registry_rows(guard: &State) -> Vec<(String, String, String, String)> {
    let Ok(mut stmt) = guard.conn().prepare(
        "SELECT repo, granted_role, intended_role, via FROM registry ORDER BY repo",
    ) else {
        return Vec::new();
    };
    let mapped = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    });
    match mapped {
        Ok(rows) => rows.collect::<Result<Vec<_>, _>>().unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// `owner/repo` shape check mirroring the proxy's path grammar: a single
/// slash, both segments non-empty, no traversal.
fn valid_repo(arg: &str) -> Option<String> {
    let (owner, repo) = arg.split_once('/')?;
    let segment = |s: &str| {
        !s.is_empty()
            && s != "."
            && !s.contains("..")
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    if segment(owner) && segment(repo) {
        Some(arg.to_string())
    } else {
        None
    }
}

/// Human age of a hold: seconds, then minutes, then hours.
fn age(created_ms: u64) -> String {
    let secs = now_unix_ms().saturating_sub(created_ms) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_repo_accepts_owner_repo_and_rejects_traversal() {
        assert_eq!(valid_repo("acme/api").as_deref(), Some("acme/api"));
        assert_eq!(
            valid_repo("octo/hello.world_2-x").as_deref(),
            Some("octo/hello.world_2-x")
        );
        assert_eq!(valid_repo(""), None);
        assert_eq!(valid_repo("acme"), None);
        assert_eq!(valid_repo("acme/"), None);
        assert_eq!(valid_repo("/api"), None);
        assert_eq!(valid_repo("../etc"), None);
        assert_eq!(valid_repo("a..b/api"), None);
        assert_eq!(valid_repo("a/b/c"), None, "extra slash fails the repo segment");
    }

    #[test]
    fn age_renders_seconds_minutes_hours() {
        let now = now_unix_ms();
        assert_eq!(age(now - 45_000), "45s");
        assert_eq!(age(now - 5 * 60_000), "5m");
        assert_eq!(age(now - 3 * 3600_000), "3h");
    }
}
