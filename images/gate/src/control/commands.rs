//! Telegram control-plane commands (GATE-006): `/pending` lists unresolved
//! holds with resume/cancel buttons, `/block` and `/unblock` flip the
//! repo kill switch, `/status` renders cheap read-only gate state,
//! `/elevate` opens the repo's elevation flow by hand, and
//! `/token mint|revoke|list` drives the agent-token lifecycle (GATE-003).
//! All handlers are allowlist-gated upstream (the transport drops strangers
//! before any handler runs); replies edit the command's message in place.
//!
//! `/status` is deliberately P0-cheap: forwarding state (machine
//! credential decrypts), agent-token count, registry rows with roles, and
//! the open-flow count — all local reads, no upstream calls. Unknown or
//! arg-less commands get a short usage reply.
//!
//! Token secrecy (GATE-003): the mint reply is the SINGLE plaintext
//! exposure of a raw agent token — the operator's own chat, once. No
//! handler ever echoes a raw token a second time: listings carry names and
//! timestamps only, audit entries (written by the token store itself) carry
//! the token NAME, and nothing token-shaped reaches a log line.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::approval::list_unresolved;
use crate::audit::{Audit, AuditEntry};
use crate::state::{self, RegistryRow, State};
use crate::control::grant::{outcome_text, RealGrantFlow};
use crate::control::send::SendRequest;
use crate::control::setup::{GrantFlow, NotifyHandle};
use crate::control::{ControlApi, Update, UpdateHandler};
use crate::tokens::TokenStore;
use crate::types::Role;

/// Reply for commands the gate does not know. `/setup` is NOT dispatched
/// here — the guided walkthrough owns it (its replies edit the command
/// bubble and may fresh-send on a stale resume); the dispatcher yields it.
const UNKNOWN_COMMAND: &str =
    "Unknown command. Available: /pending, /status, /block owner/repo, /unblock owner/repo, /token mint|revoke|list, /grant owner/repo [role], /elevate owner/repo role, /revoke owner/repo, /rotate, /disconnect, /setup";

/// Usage line for /block and /unblock.
const BLOCK_USAGE: &str = "Usage: /block owner/repo — or /unblock owner/repo";

/// Usage line for /grant (GATE-010): repo grammar + optional tier.
const GRANT_USAGE: &str =
    "Usage: /grant owner/repo [role] — role: read|triage|write|maintain|admin (default write)";

/// Usage line for /revoke (GATE-010 §10): the lower-or-remove menu.
const REVOKE_USAGE: &str =
    "Usage: /revoke owner/repo — lowers the machine account's role to Write, or removes it entirely";

/// Usage line for /elevate (GATE-006, GATE-010): the operator-side
/// elevation trigger — repo grammar + a gap tier. `read` is not a gap
/// (baseline access) and `admin` needs the proxy triggers (GATE-010 keeps
/// hand-raised elevations below the settings tier), so triage/write/maintain.
const ELEVATE_USAGE: &str = "Usage: /elevate owner/repo role — role: triage|write|maintain";

/// Usage line for /token and its subcommands; also the rejection reply for
/// malformed or invalid token names.
const TOKEN_USAGE: &str = "Usage: /token mint <name> | /token revoke <name> | /token list — names are letters, digits, '-' and '_', up to 64 characters.";

/// Everything the command handlers read. `machine_token` is the same
/// closure the proxies use: calling it decrypts in memory and decides the
/// forwarding line on /status. No secrets ever render — only the
/// enabled/disabled verdict. `lifecycle` carries the machine-token
/// lifecycle deps (GATE-004): present ⇒ /rotate and /disconnect are live,
/// absent ⇒ both answer the usage reply (no bot, no lifecycle). `grant`
/// carries the grant-flow machinery (GATE-010) plus the chat id it
/// reports into: present ⇒ /grant runs the real flow, absent ⇒ usage.
/// `reduction` carries the reduction/revoke machinery (GATE-010 §10,
/// task 9): present ⇒ /revoke runs the real menu, absent ⇒ usage.
pub struct CommandDeps {
    pub state: Arc<Mutex<State>>,
    pub audit: Arc<Audit>,
    pub machine_token: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    pub lifecycle: Option<Arc<crate::tasks::LifecycleDeps>>,
    pub grant: Option<(Arc<RealGrantFlow>, i64)>,
    pub reduction: Option<Arc<crate::control::grant::ReductionDeps>>,
}

/// Register the command dispatcher on a router.
pub fn register_commands(router: &mut crate::control::ControlRouter, deps: Arc<CommandDeps>) {
    router.register(Arc::new(Commands { deps }));
}

struct Commands {
    deps: Arc<CommandDeps>,
}

#[async_trait]
impl UpdateHandler for Commands {
    async fn handle(&self, update: Update, api: Arc<dyn ControlApi>) {
        // Callback updates belong to the approval/lifecycle handlers; a
        // reply needs a message to edit in place.
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
        let rest: Vec<&str> = words.collect();
        let sender = update.sender_id;
        if cmd == "setup" {
            // The guided walkthrough handler (control::setup) sees this
            // same update and owns the reply — nothing to do here.
            return;
        }
        if cmd == "grant" {
            // Grant flow (GATE-010): validates and spawns the polling
            // conversation in the background — the router is never held
            // for the hours a real invitation wait can take. The reply
            // edits in place like every other command.
            let reply = self.grant_reply(arg, &rest, &api);
            let _ = api.edit_message(to, reply).await;
            return;
        }
        if cmd == "revoke" {
            // Reduction/removal menu (GATE-010 §10, task 9): sync menu
            // build (a flows row + two signed buttons); the conversation
            // starts on a button press, in the background.
            let reply = self.revoke_reply(arg);
            let _ = api.edit_message(to, reply).await;
            return;
        }
        if cmd == "elevate" {
            // Operator-side elevation trigger (GATE-006, GATE-010):
            // creates/adopts the repo's elevation flow — the sweep sends
            // the consent message and the verify loop takes over. Sync:
            // only a local row + reply.
            let reply = self.elevate_reply(arg, &rest);
            let _ = api.edit_message(to, reply).await;
            return;
        }
        let reply: SendRequest = match (cmd, arg) {
            ("pending", _) => self.pending_reply().into(),
            ("block", Some(repo)) => self.block_reply(repo, true, sender).into(),
            ("unblock", Some(repo)) => self.block_reply(repo, false, sender).into(),
            ("block", None) | ("unblock", None) => (BLOCK_USAGE.to_string(), Vec::new()).into(),
            ("status", _) => self.status_reply().into(),
            ("token", _) => self.token_reply(arg, &rest).into(),
            // Machine-token lifecycle (GATE-004): delegate to the
            // lifecycle layer — one reply per command, like every other.
            ("rotate", _) => match &self.deps.lifecycle {
                Some(deps) => crate::control::lifecycle::rotate_reply(deps, sender),
                None => (UNKNOWN_COMMAND.to_string(), Vec::new()).into(),
            },
            ("disconnect", _) => match &self.deps.lifecycle {
                Some(deps) => crate::control::lifecycle::disconnect_reply(deps, sender),
                None => (UNKNOWN_COMMAND.to_string(), Vec::new()).into(),
            },
            _ => (UNKNOWN_COMMAND.to_string(), Vec::new()).into(),
        };
        let _ = api.edit_message(to, reply).await;
    }
}

/// `(text, callback-buttons)` command replies render into a plain
/// `SendRequest` (commands.rs carried this tuple shape before the
/// lifecycle replies needed link buttons too).
impl From<(String, Vec<(String, String)>)> for SendRequest {
    fn from((text, buttons): (String, Vec<(String, String)>)) -> Self {
        SendRequest::text(text).buttons(buttons)
    }
}

impl Commands {
    /// /grant owner/repo [role]: validate, ack in place, and run the
    /// grant conversation in the background (GATE-010). The spawned task
    /// reports the terminal outcome as one message — setup's checklist
    /// rendering does not apply here.
    fn grant_reply(
        &self,
        arg: Option<&str>,
        rest: &[&str],
        api: &Arc<dyn ControlApi>,
    ) -> SendRequest {
        let Some((flow, chat)) = self.deps.grant.as_ref() else {
            return (GRANT_USAGE.to_string(), Vec::new()).into();
        };
        let Some(repo_arg) = arg else {
            return (GRANT_USAGE.to_string(), Vec::new()).into();
        };
        let Some(repo) = valid_repo(repo_arg) else {
            return (
                format!("'{repo_arg}' is not owner/repo. {GRANT_USAGE}"),
                Vec::new(),
            )
                .into();
        };
        let intended = match rest.first() {
            None => Role::Write,
            Some(word) => match role_from_arg(word) {
                Some(role) => role,
                None => {
                    return (format!("Unknown role '{word}'. {GRANT_USAGE}"), Vec::new()).into();
                }
            },
        };
        let notify = NotifyHandle {
            api: api.clone(),
            chat_id: *chat,
        };
        let flow = flow.clone();
        let login = flow.deps.machine_login.clone();
        let ack = format!(
            "Grant flow started for {repo} (intended role: {intended}). Instructions \
             below — I'll confirm here when access is verified."
        );
        tokio::spawn(async move {
            let outcome = flow.grant(&repo, &notify).await;
            let _ = notify
                .send(SendRequest::text(outcome_text(&repo, &login, &outcome)))
                .await;
        });
        SendRequest::text(ack)
    }

    /// /revoke owner/repo (GATE-010 §10, task 9): validate, then build the
    /// lower-or-remove menu. The menu row is durable; the conversation
    /// starts only on a button press ([Lower to Write] / [Remove
    /// completely], handled in `grant::RevokeHandler`). An unregistered
    /// repo has nothing to revoke — the registry is the work list.
    fn revoke_reply(&self, arg: Option<&str>) -> SendRequest {
        let Some(deps) = self.deps.reduction.as_ref() else {
            return (REVOKE_USAGE.to_string(), Vec::new()).into();
        };
        let Some(repo_arg) = arg else {
            return (REVOKE_USAGE.to_string(), Vec::new()).into();
        };
        let Some(repo) = valid_repo(repo_arg) else {
            return (
                format!("'{repo_arg}' is not owner/repo. {REVOKE_USAGE}"),
                Vec::new(),
            )
                .into();
        };
        let registered = {
            let guard = self.deps.state.lock().expect("state lock must be poison-free");
            guard.registry_get(&repo).ok().flatten().is_some()
        };
        if !registered {
            return SendRequest::text(format!(
                "No registry entry for {repo} — nothing to revoke. Only registered \
                 repos are managed; /grant {repo} adds one."
            ));
        }
        crate::control::grant::revoke_menu_reply(deps, &repo)
    }

    /// /elevate owner/repo role (GATE-006, GATE-010): validate, then open
    /// (or join) the repo's elevation flow — the SAME row the proxies
    /// create on a role gap — so the sweep batches it, sends the consent
    /// message and the verify loop takes over. The confirmation names the
    /// flow's current state (DETECTED for a fresh flow; the joined flow's
    /// state otherwise), so a re-issue reads as a join, not a duplicate.
    fn elevate_reply(&self, arg: Option<&str>, rest: &[&str]) -> SendRequest {
        let Some(repo_arg) = arg else {
            return (ELEVATE_USAGE.to_string(), Vec::new()).into();
        };
        let Some(repo) = valid_repo(repo_arg) else {
            return (
                format!("'{repo_arg}' is not owner/repo. {ELEVATE_USAGE}"),
                Vec::new(),
            )
                .into();
        };
        let needed = match rest.first() {
            Some(word) => match word.to_ascii_lowercase().as_str() {
                "triage" => Role::Triage,
                "write" => Role::Write,
                "maintain" => Role::Maintain,
                _ => {
                    return (
                        format!("Unknown role '{word}'. {ELEVATE_USAGE}"),
                        Vec::new(),
                    )
                        .into();
                }
            },
            None => return (ELEVATE_USAGE.to_string(), Vec::new()).into(),
        };
        let (flow_id, flow_state) = {
            let guard = self.deps.state.lock().expect("state lock must be poison-free");
            let id = crate::control::elevation::open_or_join_flow(
                &guard,
                &repo,
                "/elevate (operator request)",
                needed,
            );
            let state = guard.flow_state(&id).ok().flatten().unwrap_or_default();
            (id, state)
        };
        SendRequest::text(format!(
            "Elevation flow {flow_id} for {repo} is {flow_state} (needed: {needed}) \
             — the consent message with the grant buttons follows in this chat."
        ))
    }

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
            basis: "control command".to_string(),
            approver: sender.map(|id| id.to_string()),
        });
        let text = if blocked {
            format!("Blocked {repo}. Pushes to it are denied until you run /unblock {repo}.")
        } else {
            format!("Unblocked {repo}. Policy decides pushes to it again.")
        };
        (text, Vec::new())
    }

    /// /token dispatch: strict arity — `mint`/`revoke` take exactly one
    /// name, `list` takes none; anything else answers usage. Audit lives
    /// in the token store itself (one entry per event, token NAME only),
    /// so this layer records nothing extra.
    fn token_reply(
        &self,
        sub: Option<&str>,
        args: &[&str],
    ) -> (String, Vec<(String, String)>) {
        match (sub, args) {
            (Some("list"), []) => self.token_list_reply(),
            (Some("mint"), [name]) => self.token_mint_reply(name),
            (Some("revoke"), [name]) => self.token_revoke_reply(name),
            _ => (TOKEN_USAGE.to_string(), Vec::new()),
        }
    }

    /// /token mint {name}: mint (or rotate) and reply with the raw token
    /// exactly once plus the store-it-now warning — the single plaintext
    /// exposure a token ever gets (GATE-003). Duplicate name ⇒ ROTATION
    /// (core Task 5 upsert invalidates the previous token); the reply says
    /// which happened.
    fn token_mint_reply(&self, name: &str) -> (String, Vec<(String, String)>) {
        if !valid_token_name(name) {
            return (
                format!("Invalid token name '{name}'. {TOKEN_USAGE}"),
                Vec::new(),
            );
        }
        let minted = {
            let guard = self.deps.state.lock().expect("state lock must be poison-free");
            let store = TokenStore::new(&guard, &self.deps.audit);
            let rotating = store.exists(name);
            store.mint(name).map(|raw| (rotating, raw))
        };
        let (rotating, raw) = match minted {
            Ok(pair) => pair,
            Err(e) => return (format!("token mint failed: {e}"), Vec::new()),
        };
        let header = if rotating {
            format!("Token ROTATED for '{name}' — the previous token no longer works.")
        } else {
            format!("Token minted for '{name}'.")
        };
        let text = format!(
            "{header}\n\n{raw}\n\nStore this now — it is shown only once and cannot be recovered."
        );
        (text, Vec::new())
    }

    /// /token revoke {name}: confirm with the affected-agent note, or a
    /// graceful miss for an unknown name.
    fn token_revoke_reply(&self, name: &str) -> (String, Vec<(String, String)>) {
        if !valid_token_name(name) {
            return (
                format!("Invalid token name '{name}'. {TOKEN_USAGE}"),
                Vec::new(),
            );
        }
        let existed = {
            let guard = self.deps.state.lock().expect("state lock must be poison-free");
            TokenStore::new(&guard, &self.deps.audit).revoke(name)
        };
        if existed {
            (
                format!(
                    "Token '{name}' revoked. Any agent still authenticating \
                     with it will fail immediately."
                ),
                Vec::new(),
            )
        } else {
            (
                format!("No such token '{name}'. Nothing was revoked."),
                Vec::new(),
            )
        }
    }

    /// /token list: names + created/last-used timestamps. NEVER hashes or
    /// raw tokens — secret material never renders in a listing.
    fn token_list_reply(&self) -> (String, Vec<(String, String)>) {
        let rows = {
            let guard = self.deps.state.lock().expect("state lock must be poison-free");
            TokenStore::new(&guard, &self.deps.audit).list()
        };
        if rows.is_empty() {
            return (
                "No agent tokens. Mint one with /token mint <name>.".to_string(),
                Vec::new(),
            );
        }
        let mut text = format!("Agent tokens ({}):", rows.len());
        for t in &rows {
            let last_used = match t.last_used_at {
                Some(ts) => ts.to_string(),
                None => "never".to_string(),
            };
            text.push_str(&format!(
                "\n- {} — created {}, last used {}",
                t.name, t.created_at, last_used
            ));
        }
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
        for row in &registry {
            if row.granted_role == "none" {
                text.push_str(&format!(
                    "\n- {}: access removed (intended {}) via {} verified {}",
                    row.repo, row.intended_role, row.via, row.last_verified_at
                ));
            } else {
                text.push_str(&format!(
                    "\n- {}: granted {} (intended {}) via {} verified {}",
                    row.repo, row.granted_role, row.intended_role, row.via, row.last_verified_at
                ));
            }
        }
        text.push_str(&format!("\nopen flows: {flows}"));
        (text, Vec::new())
    }
}

/// Registry rows for /status; any DB hiccup renders as "no rows" — status
/// is informational and must never panic.
fn registry_rows(guard: &State) -> Vec<RegistryRow> {
    guard.registry_all().unwrap_or_default()
}

/// `owner/repo` shape check mirroring the proxy's path grammar: a single
/// slash, both segments non-empty, no traversal. Shared with the setup
/// walkthrough's step-8 repo-line parser.
pub(crate) fn valid_repo(arg: &str) -> Option<String> {
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

/// Token-name grammar (GATE-003): letters, digits, `-` and `_`, at most
/// 64 characters.
fn valid_token_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// /grant's optional role word (GATE-010): case-insensitive registry
/// spellings; `none` is not a grantable target.
fn role_from_arg(word: &str) -> Option<Role> {
    match word.to_ascii_lowercase().as_str() {
        "read" => Some(Role::Read),
        "triage" => Some(Role::Triage),
        "write" => Some(Role::Write),
        "maintain" => Some(Role::Maintain),
        "admin" => Some(Role::Admin),
        _ => None,
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

    #[test]
    fn valid_token_name_grammar() {
        assert!(valid_token_name("claude-code"));
        assert!(valid_token_name("agent_2"));
        assert!(valid_token_name("A1"));
        assert!(!valid_token_name(""), "empty is invalid");
        assert!(!valid_token_name("bad name"), "space rejected");
        assert!(!valid_token_name("bad$name"), "symbols rejected");
        assert!(!valid_token_name("../traversal"), "slash rejected");
        assert!(!valid_token_name(&"a".repeat(65)), "65 chars rejected");
        assert!(valid_token_name(&"a".repeat(64)), "64 chars accepted");
    }

    #[test]
    fn grant_role_arg_maps_registry_spellings_case_insensitively() {
        assert_eq!(role_from_arg("write"), Some(Role::Write));
        assert_eq!(role_from_arg("Write"), Some(Role::Write));
        assert_eq!(role_from_arg("TRIAGE"), Some(Role::Triage));
        assert_eq!(role_from_arg("read"), Some(Role::Read));
        assert_eq!(role_from_arg("maintain"), Some(Role::Maintain));
        assert_eq!(role_from_arg("admin"), Some(Role::Admin));
        assert_eq!(role_from_arg("none"), None, "none is not a grant target");
        assert_eq!(role_from_arg("emperor"), None);
        assert_eq!(role_from_arg(""), None);
    }

    #[test]
    fn elevate_validates_args_and_opens_or_joins_one_flow() {
        let state = Arc::new(Mutex::new(
            State::open(":memory:").expect("open"),
        ));
        let audit_path = std::env::temp_dir().join(format!(
            "gate-elevate-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let deps = Arc::new(CommandDeps {
            state: state.clone(),
            audit: Arc::new(Audit::open(&audit_path)),
            machine_token: Arc::new(|| None),
            lifecycle: None,
            grant: None,
            reduction: None,
        });
        let commands = Commands { deps: deps.clone() };

        // Bad args: usage replies, and NOTHING opens.
        let reply = commands.elevate_reply(Some("acme"), &["write"]);
        assert!(reply.text.contains("not owner/repo"), "{}", reply.text);
        for args in [
            (None, vec![]),                    // no repo at all
            (Some("acme/api"), vec![]),        // role missing
            (Some("acme/api"), vec!["emperor"]), // unknown role
            (Some("acme/api"), vec!["admin"]), // admin is not a hand-raise tier
            (Some("acme/api"), vec!["read"]),  // read is not a gap tier
        ] {
            let reply = commands.elevate_reply(args.0, &args.1);
            assert!(
                reply.text.contains("Usage: /elevate"),
                "args {args:?} ⇒ usage: {}",
                reply.text
            );
        }
        {
            let guard = deps.state.lock().unwrap();
            assert!(
                guard.flows_list("elevation").unwrap().is_empty(),
                "rejections must not open a flow"
            );
        }

        // Valid: opens the DETECTED row and names the state in the reply.
        let reply = commands.elevate_reply(Some("acme/api"), &["Write"]);
        assert!(reply.text.contains("DETECTED"), "{}", reply.text);
        let flow_id = {
            let guard = deps.state.lock().unwrap();
            let rows = guard.flows_list("elevation").unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].flow_state, "DETECTED");
            let p = crate::control::elevation::ElevationPayload::from_json(&rows[0].payload_json);
            assert_eq!(p.repo, "acme/api");
            assert_eq!(p.needed_role, "write");
            assert_eq!(p.actions.len(), 1);
            rows[0].id.clone()
        };
        assert!(reply.text.contains(&flow_id), "reply names the flow: {}", reply.text);

        // Re-issue: joins the SAME flow (role raised, action appended, no
        // duplicate row), and the reply names the joined state.
        let reply = commands.elevate_reply(Some("acme/api"), &["maintain"]);
        assert!(
            reply.text.contains("DETECTED") && reply.text.contains(&flow_id),
            "same open flow: {}",
            reply.text
        );
        {
            let guard = deps.state.lock().unwrap();
            let rows = guard.flows_list("elevation").unwrap();
            assert_eq!(rows.len(), 1, "join, never duplicate");
            let p = crate::control::elevation::ElevationPayload::from_json(&rows[0].payload_json);
            assert_eq!(p.needed_role, "maintain", "raised to the max");
            assert_eq!(p.actions.len(), 2, "the gap joined the action list");
        }
        let _ = std::fs::remove_file(&audit_path);
    }

    #[test]
    fn status_shows_last_verified_per_repo_and_removals() {
        let state = Arc::new(Mutex::new(
            State::open(":memory:").expect("open"),
        ));
        {
            let guard = state.lock().unwrap();
            guard
                .registry_upsert("acme/api", "write", "write", "collaborator")
                .unwrap();
            guard
                .registry_upsert("acme/gone", "none", "write", "collaborator")
                .unwrap();
        }
        let audit_path = std::env::temp_dir().join(format!(
            "gate-status-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let deps = Arc::new(CommandDeps {
            state,
            audit: Arc::new(Audit::open(&audit_path)),
            machine_token: Arc::new(|| None),
            lifecycle: None,
            grant: None,
            reduction: None,
        });
        let commands = Commands { deps };
        let (text, buttons) = commands.status_reply();
        assert!(buttons.is_empty());
        assert!(
            text.contains("acme/api: granted write (intended write) via collaborator verified"),
            "each registry row carries its last_verified_at: {text}"
        );
        assert!(
            text.contains("acme/gone: access removed (intended write) via collaborator verified"),
            "a granted_role=none row renders as access removed: {text}"
        );
        let _ = std::fs::remove_file(&audit_path);
    }
}
