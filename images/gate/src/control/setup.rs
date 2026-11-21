//! Guided control-channel setup — the first-run walkthrough (GATE-011): a user
//! with no stored machine token goes from `/setup` to a working agent
//! setup, one step per message, with durable resumable progress.
//!
//! # Durable state
//!
//! Everything lives in the `setup_progress` table: `step` (current step;
//! its `updated_at` doubles as the step message's send time),
//! `username`, `main_email_hint` (the plus-alias suggestion source —
//! deleted when setup finishes, never used beyond the alias), `nonce`
//! (the one-time form nonce this flow minted), `msg` (the walkthrough
//! message anchor, `chat_id:message_id`), `checklist` (step 8's JSON
//! state) and `ts:{n}` completion stamps. `[Cancel setup]` and the finish
//! summary clear the table; a restart resumes at the same step, resending
//! the current step message ONLY when its row is older than 1 hour
//! ([`RESEND_AFTER_SECS`]).
//!
//! # The eight steps (copy deck: GATE-011 §"The eight steps")
//!
//! 0 intro `[Start]` · 1 email (plus-address suggestion from an optional
//! text reply) · 2 sign-up (username via text reply, existence polled via
//! unauthenticated `GET /users/{name}`; `[Done]` (or resending the name)
//! re-runs the check and the flow advances ONLY on a 200 — a taken name gets
//! `{name}-agent-1`/`{name}-bot` alternatives) · 3 profile · 4 private
//! email · 5 2FA (steps 1–5 carry Done-style buttons) · 6 token creation
//! (prefilled link, 14/3/1-day reminder promise) · 7 intake (one-time
//! form `{GATE_PUBLIC_URL}/setup/token?nonce=…`, or the paste fallback —
//! pastes are deleted instantly; intake accepts them in BOTH modes: a
//! token sent into chat must vanish whether or not a form URL exists)
//! · 8 repo checklist (`owner/repo` lines through the grant flow — the
//! grants run OFF the router dispatch in a spawned worker that updates
//! ONE message in place per repo and sends the setup-completion summary
//! when the last outcome lands; a mid-grant `[Cancel setup]` clears the
//! rows and the worker bails on its liveness check) → finish summary +
//! command list + ruleset-backstop reminder.
//!
//! Steps 6 and 7 are a PAIR delivered together (a documented flow
//! ruling): step 6 cannot be API-verified and carries no Done (GATE-011
//! reserves Done for steps 1–5), so its message is followed immediately
//! by step 7's intake ask — the same shape the `/rotate` intake uses.
//! Other steps advance by editing the walkthrough message in place; the
//! intake ask and the step-8 checklist are fresh sends that become the
//! new anchor. A failed intake (wrong login, missing 2FA…) merges the
//! reason into a step-6 re-show and re-sends the intake ask with the SAME
//! nonce — nothing is re-minted, so the original form link stays valid.
//!
//! # THE GRANT-FLOW SEAM (controller ruling, binding)
//!
//! Step 8 consumes [`GrantFlow`] as `Arc<dyn GrantFlow>`. This module
//! ships the trait, [`GrantOutcome`], the [`NotifyHandle`] the flow uses
//! to reach the user and a scripted test double ([`FakeGrantFlow`]). The
//! REAL grant machinery lives in [`crate::control::grant`]
//! ([`crate::control::grant::RealGrantFlow`], GATE-010): it implements
//! [`GrantFlow`] and `main.rs` wires it in — the step-8 handler here
//! consumes the trait and never changed.
//!
//! Intake mechanics (steps 6–7) REUSE task 4's pieces:
//! [`crate::onepage::mint_setup_nonce`] for the single-use form nonce
//! (stale `rotate_in_progress` is cleared — the setup intake supersedes a
//! pending rotation), [`crate::custody::MachineTokenStore`] for storage,
//! [`crate::tasks::apply_stored_token`] for the post-store lifecycle and
//! [`crate::tasks::check_failed_text`] for naming a failed check. The
//! form's success path calls [`form_stored_hook`], which advances a FIRST
//! setup (never a rotation — the caller distinguishes via
//! `rotate_in_progress`) to step 8.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::audit::{Audit, AuditEntry};
use crate::custody::MachineTokenStore;
use crate::github::Platform;
use crate::state::{self, State as DbState};
use crate::control::send::{MsgRef, SendRequest};
use crate::control::{validate_callback, ControlApi, Update, UpdateHandler};

/// Flow id of the setup walkthrough's signed buttons.
pub const SETUP_FLOW: &str = "setup";

/// kv key backing the walkthrough's signed callbacks (persists so old
/// step messages keep working across restarts, like the rotate nonce).
const KV_SETUP_CB_NONCE: &str = "setup_cb_nonce";

/// A step whose message row is older than this on resume is resent.
pub const RESEND_AFTER_SECS: i64 = 3600;

// setup_progress keys.
const KEY_STEP: &str = "step";
const KEY_USERNAME: &str = "username";
const KEY_EMAIL_HINT: &str = "main_email_hint";
const KEY_NONCE: &str = "nonce";
const KEY_MSG: &str = "msg";
const KEY_CHECKLIST: &str = "checklist";

// ---------------------------------------------------------------------------
// The grant-flow seam (the real implementation lives in control::grant)
// ---------------------------------------------------------------------------

/// What a grant flow uses to reach the operator while granting (GATE-010:
/// instruction messages, invitation reminders). The setup handler only
/// constructs it; the flow sends through it.
pub struct NotifyHandle {
    pub api: Arc<dyn ControlApi>,
    pub chat_id: i64,
}

impl NotifyHandle {
    /// Send one message into the operator chat.
    pub async fn send(&self, req: SendRequest) -> anyhow::Result<MsgRef> {
        self.api
            .send_message(req.replying_to(MsgRef {
                chat_id: self.chat_id,
                message_id: 0,
            }))
            .await
    }
}

/// Result of granting one repo, rendered as a checklist mark.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GrantOutcome {
    /// Access verified — ✅ line.
    Granted { role: String },
    /// In progress on the user's side (invitation sent, role change
    /// pending) — ⏳ line with the detail.
    Pending { detail: String },
    /// Cannot grant (role unavailable, upstream refusal) — ❌ line with
    /// the reason.
    Failed { reason: String },
}

impl GrantOutcome {
    /// Checklist symbol for the outcome.
    pub fn symbol(&self) -> &'static str {
        match self {
            GrantOutcome::Granted { .. } => "✅",
            GrantOutcome::Pending { .. } => "⏳",
            GrantOutcome::Failed { .. } => "❌",
        }
    }

    /// The explanatory text after the symbol.
    pub fn detail(&self) -> String {
        match self {
            GrantOutcome::Granted { role } => format!("granted ({role})"),
            GrantOutcome::Pending { detail } => detail.clone(),
            GrantOutcome::Failed { reason } => reason.clone(),
        }
    }
}

/// The step-8 seam (see the module docs): run the full grant conversation
/// for `repo` — send instructions via `notify`, verify on GitHub — and
/// report one outcome for the in-place checklist update.
#[async_trait]
pub trait GrantFlow: Send + Sync {
    async fn grant(&self, repo: &str, notify: &NotifyHandle) -> GrantOutcome;
}

/// Scripted test double: per-repo outcomes with a default fallback, plus
/// a call log. Compiled only under `test` / `test-util` (like
/// [`crate::control::fake::FakeTransport`]).
#[cfg(any(test, feature = "test-util"))]
pub struct FakeGrantFlow {
    scripted: Mutex<Vec<(String, GrantOutcome)>>,
    default: Mutex<Option<GrantOutcome>>,
    calls: Mutex<Vec<String>>,
}

#[cfg(any(test, feature = "test-util"))]
impl FakeGrantFlow {
    pub fn new() -> Self {
        Self {
            scripted: Mutex::new(Vec::new()),
            default: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Script the outcome for one exact repo.
    pub fn with_outcome(self, repo: &str, outcome: GrantOutcome) -> Self {
        self.scripted.lock().unwrap().push((repo.to_string(), outcome));
        self
    }

    /// Outcome for repos without an explicit script.
    pub fn with_default_outcome(self, outcome: GrantOutcome) -> Self {
        *self.default.lock().unwrap() = Some(outcome);
        self
    }

    /// Repos granted so far, in call order.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[cfg(any(test, feature = "test-util"))]
impl Default for FakeGrantFlow {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(test, feature = "test-util"))]
#[async_trait]
impl GrantFlow for FakeGrantFlow {
    async fn grant(&self, repo: &str, _notify: &NotifyHandle) -> GrantOutcome {
        self.calls.lock().unwrap().push(repo.to_string());
        let scripted = self.scripted.lock().unwrap();
        if let Some((_, outcome)) = scripted.iter().find(|(r, _)| r == repo) {
            return outcome.clone();
        }
        drop(scripted);
        self.default
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(GrantOutcome::Pending {
                detail: "fake grant in flight".to_string(),
            })
    }
}

// ---------------------------------------------------------------------------
// Dependencies + registration
// ---------------------------------------------------------------------------

/// Everything the walkthrough reads and flips. `gate_public_url` empty ⇒
/// step 7 uses the paste fallback instead of the one-time form.
pub struct SetupDeps {
    pub state: Arc<Mutex<DbState>>,
    pub platform: Arc<dyn Platform>,
    /// The machine account login the gate verifies tokens against
    /// (`GATE_MACHINE_LOGIN`) — also the username step 2 steers towards.
    pub machine_login: String,
    pub enc_key: String,
    pub gate_public_url: String,
    /// The operator chat for gate-initiated sends (resume-after-restart,
    /// the form-success hook). `0` disables those sends.
    pub chat_id: i64,
    pub forwarding: Arc<std::sync::atomic::AtomicBool>,
    pub grant: Arc<dyn GrantFlow>,
    pub audit: Arc<Audit>,
}

/// Register the walkthrough handler on a router. `/setup` is handled HERE
/// (not in the command dispatcher) because it edits into the command
/// bubble and may fresh-send on a stale resume; `commands.rs` yields it.
pub fn register_setup(router: &mut crate::control::ControlRouter, deps: Arc<SetupDeps>) {
    router.register(Arc::new(SetupHandler { deps }));
}

/// Nonce resolver for the router's callback validation: exactly one flow
/// (`setup`), keyed by the persistent kv nonce.
pub fn nonce_lookup(state: Arc<Mutex<DbState>>) -> crate::control::NonceLookup {
    Arc::new(move |flow| {
        if flow != SETUP_FLOW {
            return None;
        }
        state
            .lock()
            .expect("state lock must be poison-free")
            .kv_get(KV_SETUP_CB_NONCE)
            .ok()
            .flatten()
    })
}

// ---------------------------------------------------------------------------
// Progress plumbing (setup_progress rows; short-lived locks, no await
// while a guard is held)
// ---------------------------------------------------------------------------

fn pget(state: &Mutex<DbState>, key: &str) -> Option<String> {
    state
        .lock()
        .expect("state lock must be poison-free")
        .setup_get(key)
        .ok()
        .flatten()
}

fn pts(state: &Mutex<DbState>, key: &str) -> Option<i64> {
    state
        .lock()
        .expect("state lock must be poison-free")
        .setup_updated_at(key)
        .ok()
        .flatten()
}

fn pset(state: &Mutex<DbState>, key: &str, value: &str) {
    let _ = state
        .lock()
        .expect("state lock must be poison-free")
        .setup_set(key, value);
}

fn pset_with_ts(state: &Mutex<DbState>, key: &str, value: &str, ts: i64) {
    let _ = state
        .lock()
        .expect("state lock must be poison-free")
        .setup_set_with_ts(key, value, ts);
}

fn pdelete(state: &Mutex<DbState>, key: &str) {
    let _ = state
        .lock()
        .expect("state lock must be poison-free")
        .setup_delete(key);
}

fn step_now(state: &Mutex<DbState>) -> Option<u8> {
    pget(state, KEY_STEP).and_then(|v| v.parse().ok())
}

fn parse_anchor(s: &str) -> Option<MsgRef> {
    let (chat, msg) = s.split_once(':')?;
    Some(MsgRef {
        chat_id: chat.parse().ok()?,
        message_id: msg.parse().ok()?,
    })
}

fn anchor_str(m: MsgRef) -> String {
    format!("{}:{}", m.chat_id, m.message_id)
}

fn stored_anchor(state: &Mutex<DbState>) -> Option<MsgRef> {
    pget(state, KEY_MSG).and_then(|s| parse_anchor(&s))
}

// ---------------------------------------------------------------------------
// Copy deck (GATE-011 §"The eight steps")
// ---------------------------------------------------------------------------

/// Minimal HTML escaping for interpolated values (usernames, emails,
/// logins — Telegram HTML parse mode).
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// GitHub username grammar: letters/digits/single hyphens, no
/// leading/trailing hyphen, no double hyphen, max 39 characters.
fn valid_username(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 39
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !name.contains("--")
}

/// The plus-address alias suggested at step 1: `local+agent@domain`.
fn plus_alias(main: &str) -> Option<String> {
    let main = main.trim();
    let (local, domain) = main.split_once('@')?;
    if local.is_empty() || domain.is_empty() || !domain.contains('.') {
        return None;
    }
    Some(format!("{local}+agent@{domain}"))
}

/// Buttons every step carries: the step's own actions, then `[I'm stuck]`
/// + `[Cancel setup]` (GATE-011).
fn base_buttons(nonce: &str, extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut buttons: Vec<(String, String)> = extra
        .iter()
        .map(|(label, action)| (label.to_string(), format!("{SETUP_FLOW}:{nonce}:{action}")))
        .collect();
    buttons.push(("I'm stuck".to_string(), format!("{SETUP_FLOW}:{nonce}:stuck")));
    buttons.push(("Cancel setup".to_string(), format!("{SETUP_FLOW}:{nonce}:cancel")));
    buttons
}

fn step_buttons(step: u8, nonce: &str) -> Vec<(String, String)> {
    let extra: &[(&str, &str)] = match step {
        0 => &[("Start", "start")],
        1 => &[("Done, I have an address", "done")],
        2 => &[("Done, account created", "done")],
        3 | 5 => &[("Done", "done")],
        4 => &[("Done, both checked", "done")],
        8 => &[("Finish setup", "finish")],
        _ => &[],
    };
    base_buttons(nonce, extra)
}

/// The values a step's copy is rendered from (owned — built fresh from
/// the durable rows before each render, never held across an await).
///
/// Two DIFFERENT nonces live here (review round 1, F1): `cb_nonce` is
/// the short 16-hex flow nonce every inline button is signed with
/// (`setup:{cb_nonce}:{action}` ≤ 64 bytes — Telegram's callback_data
/// cap) and the only one callback validation knows; `form_nonce` is the
/// 64-hex one-time FORM nonce and is used exclusively in the
/// `/setup/token?nonce=` URL, never in button data.
struct StepCtx {
    machine_login: String,
    username: Option<String>,
    email_hint: Option<String>,
    cb_nonce: String,
    form_nonce: Option<String>,
    checklist: Option<String>,
    gate_public_url: String,
}

impl StepCtx {
    fn load(state: &Mutex<DbState>, machine_login: &str, gate_public_url: &str) -> Self {
        // The checklist row stores JSON entries; the copy renders the
        // decoded, marked-up lines.
        let checklist = {
            let entries = load_checklist(state);
            (!entries.is_empty()).then(|| render_checklist(&entries))
        };
        Self {
            machine_login: machine_login.to_string(),
            username: pget(state, KEY_USERNAME).filter(|v| !v.is_empty()),
            email_hint: pget(state, KEY_EMAIL_HINT).filter(|v| !v.is_empty()),
            cb_nonce: ensure_cb_nonce(state),
            form_nonce: pget(state, KEY_NONCE).filter(|v| !v.is_empty()),
            checklist,
            gate_public_url: gate_public_url.to_string(),
        }
    }

    /// One step's message: what, why, exactly what to click (GitHub's
    /// labels, bold), one link button where GitHub has a page.
    fn req(&self, step: u8) -> SendRequest {
        let buttons = step_buttons(step, &self.cb_nonce);
        let req = match step {
            0 => SendRequest::html(concat!(
                "Let's set up a <b>machine account</b> — a separate GitHub account your ",
                "agents work through, so your own credentials never touch them. ",
                "Everything happens on GitHub under your control; I verify each step ",
                "and never take over your browser.\n\nThis takes about <b>10 minutes</b>. ",
                "You can pause any time — progress is saved, and <b>/setup</b> brings ",
                "you back.\n\nPress Start when you're ready.",
            )),
            1 => {
                let mut text = String::from(concat!(
                    "Step 1 of 8 — <b>Email</b>\n\nThe new account needs one email. ",
                    "Best option: a <b>plus address</b> — an alias of your normal inbox, ",
                    "e.g. <code>you+agent@example.com</code> still reaches ",
                    "<code>you@example.com</code>.\n\nReply with your main email and ",
                    "I'll suggest the alias — or just use any address you can receive.",
                ));
                if let Some(alias) = self.email_hint.as_deref().and_then(plus_alias) {
                    text.push_str(&format!(
                        "\n\nSuggested address: <code>{}</code> — use it when GitHub \
                         asks for the account email.",
                        esc(&alias)
                    ));
                }
                SendRequest::html(text)
            }
            2 => {
                let suggested = self
                    .username
                    .as_deref()
                    .unwrap_or(&self.machine_login);
                let mut text = format!(
                    concat!(
                        "Step 2 of 8 — <b>Sign-up</b>\n\nOpen the signup page in a ",
                        "<b>private/incognito window</b> — make sure you are NOT signed ",
                        "in as your main account. Pick the username <b>{}</b> — that's ",
                        "the name this gate verifies tokens against. Save the password ",
                        "in your password manager; I never see it.\n\nReply with the ",
                        "username once you've picked it — I'll watch GitHub for the ",
                        "account to appear.",
                    ),
                    esc(suggested)
                );
                if let Some(name) = &self.username {
                    text.push_str(&format!(
                        "\n\nChecking <code>{}</code> — not on GitHub yet. Finish the \
                         signup, then resend the name to re-check, and press \
                         <b>Done, account created</b>.",
                        esc(name)
                    ));
                }
                SendRequest::html(text)
                    .with_link_button("Open GitHub signup", crate::github::SIGNUP_URL)
            }
            3 => SendRequest::html(concat!(
                "Step 3 of 8 — <b>Profile</b>\n\nOn the machine account, open ",
                "<b>Settings → Profile</b> and set the name and a short bio marking ",
                "it automated, e.g. \"Automated agent account — operated by my main ",
                "account\". Humans should see at a glance this isn't a person.",
            ))
            .with_link_button("Open profile settings", crate::github::PROFILE_SETTINGS_URL),
            4 => SendRequest::html(concat!(
                "Step 4 of 8 — <b>Private email</b>\n\nOpen <b>Settings → Emails</b> ",
                "and check BOTH boxes:\n• <b>Keep my email addresses private</b>\n• ",
                "<b>Block command line pushes that expose my email</b>\n\nThis keeps ",
                "the machine account's address out of every commit it makes.",
            ))
            .with_link_button("Open email settings", crate::github::EMAIL_SETTINGS_URL),
            5 => SendRequest::html(concat!(
                "Step 5 of 8 — <b>Two-factor authentication</b>\n\nOpen <b>Settings → ",
                "Password and authentication</b> and run <b>Enable two-factor ",
                "authentication</b>. Save the recovery codes next to the account ",
                "password in your password manager — I never hold either, so losing ",
                "both means losing the account.",
            ))
            .with_link_button("Open security settings", crate::github::SECURITY_SETTINGS_URL),
            6 => SendRequest::html(concat!(
                "Step 6 of 8 — <b>Token</b>\n\nStill in the private window, signed in ",
                "as the machine account: open the prefilled token page below (scopes ",
                "<b>repo</b>, <b>workflow</b>, <b>read:org</b> and the description are ",
                "already set). Set <b>expiration</b> to a custom <b>~6 months</b>, ",
                "generate, and copy the key.\n\nI'll remind you at <b>14, 3 and 1</b> ",
                "day(s) before it expires.",
            ))
            .with_link_button("Open prefilled token page", crate::github::TOKEN_NEW_URL),
            7 => {
                if self.gate_public_url.is_empty() {
                    SendRequest::html(concat!(
                        "Step 7 of 8 — <b>Hand me the token</b>\n\nThis gate has no ",
                        "public URL, so use the paste fallback.\n\nPaste the key here ",
                        "as a message. I'll delete your message right away.",
                    ))
                } else {
                    let base = self.gate_public_url.trim_end_matches('/');
                    // The FORM nonce (64-hex, single-use) belongs in the
                    // URL only — never in callback_data.
                    let link = match &self.form_nonce {
                        Some(n) => format!("{base}/setup/token?nonce={n}"),
                        None => format!("{base}/setup/token"),
                    };
                    SendRequest::html(concat!(
                        "Step 7 of 8 — <b>Hand me the token</b>\n\nUse the one-time ",
                        "form: single use, expires in <b>15 minutes</b>, and the key ",
                        "goes straight into encrypted storage — it never touches this ",
                        "chat.\n\nIf the form expired, resume with <b>/setup</b> for a ",
                        "fresh link.",
                    ))
                    .with_link_button("Open secure form", link)
                }
            }
            8 => {
                let mut text = String::from(concat!(
                    "Step 8 of 8 — <b>Repo access</b>\n\nSend me the repositories as ",
                    "<code>owner/repo</code> — one or several per message. I'll walk ",
                    "the grant flow for each and keep the checklist below up to date.",
                ));
                if let Some(list) = &self.checklist {
                    text.push_str("\n\n");
                    text.push_str(list);
                }
                SendRequest::html(text)
            }
            _ => SendRequest::text("Unknown setup step"),
        };
        req.buttons(buttons)
    }

    /// `[I'm stuck]`: the same step message extended with common problems
    /// (GATE-011).
    fn stuck(&self, step: u8) -> SendRequest {
        let mut req = self.req(step);
        req.text.push_str(&format!(
            "\n\n<b>Common problems</b>\n{}",
            self.stuck_detail(step)
        ));
        req
    }

    fn stuck_detail(&self, step: u8) -> String {
        match step {
            0 => String::from(
                "• Nothing answers? Check you are on the broker allowlist.\n• Want \
                 out? Cancel setup — progress is only cleared when YOU cancel.",
            ),
            1 => String::from(
                "• Plus addresses work with most providers (Gmail, Fastmail, iCloud); \
                 Outlook sometimes rejects them — any address you can receive is \
                 fine.\n• The address only feeds the alias suggestion; nothing is \
                 stored after setup.",
            ),
            2 => format!(
                "• Already signed in as your main account? The signup would attach to \
                 the WRONG account — use a private window.\n• Username rules: letters, \
                 digits, single hyphens, max 39 characters.\n• I probe github.com \
                 unauthenticated — a fresh account can take a minute to appear; resend \
                 the name to re-check.\n• This gate expects the machine account to be named \
                 <b>{}</b>.",
                esc(&self.machine_login)
            ),
            3 => String::from(
                "• The bio is optional but strongly recommended — it's how humans \
                 tell the agent apart from you.\n• You can set it later; the gate \
                 does not verify this step.",
            ),
            4 => String::from(
                "• The noreply address looks like \
                 12345+login@users.noreply.github.com — that's expected.\n• Without \
                 these boxes, agent pushes could leak the account email into commits.",
            ),
            5 => String::from(
                "• Authenticator app vs SMS: both work; the app is faster.\n• \
                 Recovery codes: download them now — GitHub shows them once.\n• The \
                 gate verifies 2FA is ON before accepting the token; this step \
                 cannot be skipped.",
            ),
            6 => String::from(
                "• Wrong account? The page's avatar must be the MACHINE account — use \
                 the private window.\n• Expiration \"No expiration\" works but is \
                 discouraged; reminders only fire with a known expiry.\n• Missing \
                 scopes are rejected at step 7 — keep repo, workflow and read:org \
                 ticked.",
            ),
            7 => {
                if self.gate_public_url.is_empty() {
                    String::from(
                        "• Paste the WHOLE key (ghp_…), nothing else — I delete the \
                         message instantly and verify it.\n• Wrong-account rejection \
                         names the login it found: that token was created on another \
                         account; redo step 6 in the private window.\n• Sent it by \
                         accident? It's already deleted — nothing is stored without \
                         a passing verification.",
                    )
                } else {
                    String::from(
                        "• \"410 Gone\" on the form → the link expired (15 min) or was \
                         already used: run /setup to resume and get a fresh link.\n• \
                         Wrong-account rejection names the login it found: that token \
                         was created on your main account; redo step 6 in the private \
                         window.\n• The form never loads? Check the gate is reachable \
                         at its public URL.",
                    )
                }
            }
            8 => String::from(
                "• Format: owner/repo, one or more per message, e.g. acme/widget.\n• \
                 Pending (⏳) usually means the collaborator invite is waiting for \
                 your click on GitHub.\n• Failed (❌) names the reason — fix it on \
                 GitHub and send the repo again.",
            ),
            _ => String::new(),
        }
    }
}

/// Step 8's checklist rendering from stored entries. Repo names and
/// outcome details are escaped — details come from the grant flow
/// (upstream-derived text) and the message renders in Telegram HTML mode.
fn render_checklist(entries: &[(String, GrantOutcome)]) -> String {
    entries
        .iter()
        .map(|(repo, outcome)| {
            format!(
                "{} {} {}",
                esc(repo),
                outcome.symbol(),
                esc(&outcome.detail())
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn load_checklist(state: &Mutex<DbState>) -> Vec<(String, GrantOutcome)> {
    pget(state, KEY_CHECKLIST)
        .and_then(|v| serde_json::from_str::<Vec<(String, GrantOutcome)>>(&v).ok())
        .unwrap_or_default()
}

fn save_checklist(state: &Mutex<DbState>, entries: &[(String, GrantOutcome)]) {
    if let Ok(json) = serde_json::to_string(entries) {
        pset(state, KEY_CHECKLIST, &json);
    }
}

/// The checklist's in-flight mark: a fresh step-8 repo line before its
/// grant settles. The spawned grant worker treats any entry still carrying
/// it as "another batch is mid-grant" and leaves the completion summary to
/// whichever worker writes the last outcome.
const INFLIGHT_DETAIL: &str = "granting…";

/// Checklist entries against an already-held lock — the spawned grant
/// worker reads AND writes inside its liveness scope so a concurrent
/// cancel can never interleave.
fn load_checklist_guarded(guard: &DbState) -> Vec<(String, GrantOutcome)> {
    guard
        .setup_get(KEY_CHECKLIST)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str::<Vec<(String, GrantOutcome)>>(&v).ok())
        .unwrap_or_default()
}

/// Setup is parked on step 8 — the spawned grant worker's liveness check
/// against an already-held lock.
fn step8_live_guarded(guard: &DbState) -> bool {
    guard
        .setup_get(KEY_STEP)
        .ok()
        .flatten()
        .and_then(|v| v.parse::<u8>().ok())
        == Some(8)
}

fn step8_live(state: &Mutex<DbState>) -> bool {
    step_now(state) == Some(8)
}

/// The finish summary: reachable repos, the command list, and the
/// ruleset-backstop reminder with a link to the setup guide (GATE-011
/// step 8).
fn finish_summary(username: &str, entries: &[(String, GrantOutcome)]) -> SendRequest {
    let mut text = format!(
        "Setup complete ✅\n\nThe machine account <b>{}</b> is connected; forwarding \
         is on.\n\nRepo access:\n{}\n\nCommands you'll use:\n\
         /pending — review held pushes\n/status — gate state at a glance\n\
         /block owner/repo · /unblock owner/repo — kill switch\n\
         /token mint|revoke|list — agent tokens\n/rotate — replace the machine token\n\
         /disconnect — take the gate offline",
        esc(username),
        if entries.is_empty() {
            "(none granted — run /setup any time to send owner/repo lines)".to_string()
        } else {
            render_checklist(entries)
        },
    );
    text.push_str(
        "\n\nLast thing — the <b>ruleset backstop</b>: on each repo, protect the \
         important branches (block force pushes and deletions) and make sure the \
         machine account is NOT on the bypass list. That's what makes approvals \
         actually binding. Full walkthrough in the setup guide.",
    );
    SendRequest::html(text)
        .with_link_button("Open the setup guide", crate::github::SETUP_GUIDE_URL)
}

// ---------------------------------------------------------------------------
// Intake nonce (reuses onepage's single-use form nonce)
// ---------------------------------------------------------------------------

/// Reuse the kv form nonce while it stays valid (>60 s left); otherwise
/// mint a fresh one — clearing any stale `rotate_in_progress` first (the
/// setup intake supersedes a pending rotation, task 4's note).
fn ensure_intake_nonce(state: &Mutex<DbState>) -> String {
    let guard = state.lock().expect("state lock must be poison-free");
    if let (Some(nonce), Some(expires)) = (
        guard.kv_get(crate::onepage::KV_NONCE).ok().flatten(),
        guard
            .kv_get(crate::onepage::KV_NONCE_EXPIRES)
            .ok()
            .flatten()
            .and_then(|v| v.parse::<i64>().ok()),
    ) {
        if state::now() + 60 < expires {
            return nonce;
        }
    }
    let _ = guard.kv_delete(crate::tasks::KV_ROTATE_IN_PROGRESS);
    crate::onepage::mint_setup_nonce(&guard).unwrap_or_else(|_| crate::control::nonce())
}

fn ensure_cb_nonce(state: &Mutex<DbState>) -> String {
    let guard = state.lock().expect("state lock must be poison-free");
    match guard.kv_get(KV_SETUP_CB_NONCE) {
        Ok(Some(n)) => n,
        _ => {
            let n = crate::control::nonce();
            let _ = guard.kv_set(KV_SETUP_CB_NONCE, &n);
            n
        }
    }
}

fn cb_nonce(state: &Mutex<DbState>) -> Option<String> {
    state
        .lock()
        .expect("state lock must be poison-free")
        .kv_get(KV_SETUP_CB_NONCE)
        .ok()
        .flatten()
}

// ---------------------------------------------------------------------------
// Entry points: /setup (start or resume) + restart resume
// ---------------------------------------------------------------------------

/// Restart resume (called from `main` after the startup check): resend the
/// current step message ONLY when its row is older than 1 h — a fresh
/// flow stays perfectly quiet (no duplicate after restart).
pub async fn resume(deps: &SetupDeps, api: Arc<dyn ControlApi>) {
    let Some(step) = step_now(&deps.state) else {
        return;
    };
    if deps.chat_id == 0 {
        return;
    }
    let age = state::now() - pts(&deps.state, KEY_STEP).unwrap_or(0);
    if age < RESEND_AFTER_SECS {
        return;
    }
    if step == 7 {
        // Refresh an expired intake link before re-showing it.
        let nonce = ensure_intake_nonce(&deps.state);
        pset(&deps.state, KEY_NONCE, &nonce);
    }
    let ctx = StepCtx::load(&deps.state, &deps.machine_login, &deps.gate_public_url);
    let req = ctx.req(step).replying_to(MsgRef {
        chat_id: deps.chat_id,
        message_id: 0,
    });
    match api.send_message(req).await {
        Ok(anchor) => {
            pset(&deps.state, KEY_MSG, &anchor_str(anchor));
            pset(&deps.state, KEY_STEP, &step.to_string());
        }
        Err(e) => eprintln!("setup: resume resend failed: {e}"),
    }
}

// ---------------------------------------------------------------------------
// The handler
// ---------------------------------------------------------------------------

struct SetupHandler {
    deps: Arc<SetupDeps>,
}

#[async_trait]
impl UpdateHandler for SetupHandler {
    async fn handle(&self, update: Update, api: Arc<dyn ControlApi>) {
        if update.callback.is_some() {
            self.on_callback(update, api).await;
            return;
        }
        let Some(text) = update.text else {
            return;
        };
        let Some(to) = update.message_ref else {
            return;
        };
        let trimmed = text.trim();
        if trimmed.starts_with('/') {
            let cmd = trimmed.split_whitespace().next().unwrap_or("");
            let cmd = cmd.split('@').next().unwrap_or(cmd);
            if cmd == "/setup" {
                self.start_or_resume(Some(to), api).await;
            }
            return; // other commands belong to the dispatcher
        }
        self.on_text(trimmed, to, api).await;
    }
}

impl SetupHandler {
    /// `/setup` with no active flow: mint the callback nonce and edit the
    /// step-0 intro into the command bubble. With an active flow: a fresh
    /// step row gets a short "you're on step N" ack (no duplicate step
    /// message); a step row older than 1 h is resent (fresh send, same
    /// rule as a restart resume).
    async fn start_or_resume(&self, anchor: Option<MsgRef>, api: Arc<dyn ControlApi>) {
        let deps = &self.deps;
        match step_now(&deps.state) {
            None => {
                ensure_cb_nonce(&deps.state);
                let ctx = StepCtx::load(&deps.state, &deps.machine_login, &deps.gate_public_url);
                let req = ctx.req(0);
                let anchor = match anchor {
                    Some(to) => api.edit_message(to, req).await,
                    None => {
                        api.send_message(req.replying_to(MsgRef {
                            chat_id: deps.chat_id,
                            message_id: 0,
                        }))
                        .await
                    }
                };
                if let Ok(anchor) = anchor {
                    pset(&deps.state, KEY_STEP, "0");
                    pset(&deps.state, KEY_MSG, &anchor_str(anchor));
                }
            }
            Some(step) => {
                let age = state::now() - pts(&deps.state, KEY_STEP).unwrap_or(0);
                if age >= RESEND_AFTER_SECS {
                    let chat_id = anchor.map(|m| m.chat_id).unwrap_or(deps.chat_id);
                    if step == 7 {
                        let nonce = ensure_intake_nonce(&deps.state);
                        pset(&deps.state, KEY_NONCE, &nonce);
                    }
                    let ctx = StepCtx::load(&deps.state, &deps.machine_login, &deps.gate_public_url);
                    let req = ctx.req(step).replying_to(MsgRef {
                        chat_id,
                        message_id: 0,
                    });
                    if let Ok(new_anchor) = api.send_message(req).await {
                        pset(&deps.state, KEY_MSG, &anchor_str(new_anchor));
                        pset(&deps.state, KEY_STEP, &step.to_string());
                    }
                } else if let Some(to) = anchor {
                    let ack = SendRequest::html(format!(
                        "Setup is already in progress — you're on step <b>{step}</b> of 8. \
                         Continue in the walkthrough message; press <b>I'm stuck</b> there \
                         to see this step again.",
                    ));
                    let _ = api.edit_message(to, ack).await;
                }
            }
        }
    }

    /// Plain text while a flow is active, routed by step.
    async fn on_text(&self, text: &str, to: MsgRef, api: Arc<dyn ControlApi>) {
        let deps = &self.deps;
        let Some(step) = step_now(&deps.state) else {
            return; // no active flow: plain chat is none of our business
        };
        match step {
            1 => self.step1_email(text, api).await,
            2 => self.step2_username(text, api).await,
            7 => self.step7_paste(text, to, api).await,
            8 => self.step8_repos(text, to.chat_id, api).await,
            _ => {}
        }
    }

    /// Edit the current step's message (re-rendered) with a note appended
    /// — retries stay in place (message-volume discipline).
    async fn edit_step_note(&self, step: u8, note: String, api: &Arc<dyn ControlApi>) {
        let deps = &self.deps;
        let Some(anchor) = stored_anchor(&deps.state) else {
            return;
        };
        let ctx = StepCtx::load(&deps.state, &deps.machine_login, &deps.gate_public_url);
        let mut req = ctx.req(step);
        req.text.push_str("\n\n");
        req.text.push_str(&note);
        let _ = api.edit_message(anchor, req).await;
    }

    /// Step 1: an optional main-email text reply, used only to render the
    /// plus-address alias (stored in setup_progress, deleted at finish —
    /// GATE-011: never stored beyond the alias suggestion).
    async fn step1_email(&self, text: &str, api: Arc<dyn ControlApi>) {
        let deps = &self.deps;
        let hint = text.trim();
        if plus_alias(hint).is_none() {
            self.edit_step_note(
                1,
                "That doesn't look like an email — send it as name@example.com, or \
                 press <b>Done, I have an address</b>."
                    .to_string(),
                &api,
            )
            .await;
            return;
        }
        pset(&deps.state, KEY_EMAIL_HINT, hint);
        // Re-render step 1 in place: the alias suggestion now rides along.
        if let Some(anchor) = stored_anchor(&deps.state) {
            let ctx = StepCtx::load(&deps.state, &deps.machine_login, &deps.gate_public_url);
            let _ = api.edit_message(anchor, ctx.req(1)).await;
        }
    }

    /// Step 2: username reply. Existence (unauthenticated
    /// `GET /users/{name}`) decides:
    /// - the stored attempt now exists (or the reply IS this gate's
    ///   machine login and exists) ⇒ the account was created — advance;
    /// - a different name that exists ⇒ taken — suggest
    ///   `{name}-agent-1`/`{name}-bot` (never stored);
    /// - a name that doesn't exist ⇒ stored as the attempt, repeat-ask,
    ///   no advance.
    async fn step2_username(&self, text: &str, api: Arc<dyn ControlApi>) {
        let deps = &self.deps;
        let name = text.trim().trim_start_matches('@').to_string();
        if !valid_username(&name) {
            self.edit_step_note(
                2,
                "Usernames are letters, digits and single hyphens (no \
                 leading/trailing hyphen), up to 39 characters. Reply with the \
                 username."
                    .to_string(),
                &api,
            )
            .await;
            return;
        }
        let stored = pget(&deps.state, KEY_USERNAME);
        let exists = deps.platform.user_exists(&name).await;
        if stored.as_deref() == Some(name.as_str()) || (exists && name == deps.machine_login) {
            if exists {
                pset(&deps.state, KEY_USERNAME, &name);
                pset_with_ts(&deps.state, "ts:2", "1", state::now());
                self.advance_from(2, 3, &api).await;
            } else {
                self.edit_step_note(
                    2,
                    format!(
                        "I can't see <b>{}</b> on GitHub yet — finish the signup in \
                         the private window, then resend the name.",
                        esc(&name)
                    ),
                    &api,
                )
                .await;
            }
        } else if exists {
            // Taken: suggest alternatives tied to the user (GATE-011).
            let s1 = format!("{name}-agent-1");
            let s2 = format!("{name}-bot");
            let s1_free = !deps.platform.user_exists(&s1).await;
            let s2_free = !deps.platform.user_exists(&s2).await;
            let mut note = format!(
                "<b>{}</b> already exists — maybe that's your main account. Try:",
                esc(&name)
            );
            if s1_free {
                note.push_str(&format!("\n• <code>{}</code>", esc(&s1)));
            }
            if s2_free {
                note.push_str(&format!("\n• <code>{}</code>", esc(&s2)));
            }
            if !s1_free && !s2_free {
                note.push_str("\nBoth alternatives are taken too — pick another name.");
            }
            note.push_str(&format!(
                "\nOr use <b>{}</b> — the name this gate verifies against.",
                esc(&deps.machine_login)
            ));
            self.edit_step_note(2, note, &api).await;
        } else {
            pset(&deps.state, KEY_USERNAME, &name);
            self.edit_step_note(
                2,
                format!(
                    "Got it — I'll watch for <b>{}</b>. It's not on GitHub yet; \
                     create the account in the private window, then press \
                     <b>Check again</b> or resend the name.",
                    esc(&name)
                ),
                &api,
            )
            .await;
        }
    }

    /// Step 7 paste intake: ANY text here is treated as the token. The
    /// message is deleted immediately (failure told to the user), then
    /// full verification per GATE-004 — wrong login names the login found
    /// and returns to step 6 with the form nonce untouched.
    async fn step7_paste(&self, text: &str, to: MsgRef, api: Arc<dyn ControlApi>) {
        let deps = &self.deps;
        let token = text.trim();
        let mut warning = String::new();
        if api.delete_message(to).await.is_err() {
            warning = String::from(
                "⚠️ I could not delete your message — please delete it yourself.\n\n",
            );
        }

        match deps.platform.verify_token(token, &deps.machine_login).await {
            Ok(vt) => {
                let stored = {
                    let guard = deps.state.lock().expect("state lock must be poison-free");
                    MachineTokenStore::new(&guard, &deps.enc_key)
                        .store(token)
                        .and_then(|()| crate::tasks::apply_stored_token(&guard, &vt))
                };
                match stored {
                    Ok(()) => {
                        deps.forwarding
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        deps.audit.record(AuditEntry {
                            ts: state::now(),
                            agent: None,
                            repo: "-".to_string(),
                            action: "custody.setup".to_string(),
                            decision: "stored".to_string(),
                            basis: "control-channel paste intake".to_string(),
                            approver: None,
                        });
                        let confirmed = SendRequest::html(format!(
                            "{warning}Token verified ✅ — stored encrypted (login \
                             <b>{}</b>{}). Forwarding is on.",
                            esc(&vt.login),
                            match &vt.expires {
                                Some(e) => format!(", expires {}", esc(e)),
                                None => String::new(),
                            }
                        ));
                        if let Some(anchor) = stored_anchor(&deps.state) {
                            let _ = api.edit_message(anchor, confirmed).await;
                        }
                        pdelete(&deps.state, KEY_NONCE);
                        self.enter_step8(&api, to.chat_id).await;
                    }
                    Err(e) => {
                        eprintln!("setup: custody store failed: {e}");
                        self.edit_step_note(
                            7,
                            "The key verified but could not be stored — nothing was \
                             saved. Try pasting it again."
                                .to_string(),
                            &api,
                        )
                        .await;
                    }
                }
            }
            Err(e) => {
                let reason = match &e {
                    crate::github::VerifyError::WrongLogin(found) => format!(
                        "That key belongs to <b>{}</b> — your main account, not the \
                         machine account. Create the token signed in as <b>{}</b> in \
                         the private window, then we redo step 6.",
                        esc(found),
                        esc(&deps.machine_login)
                    ),
                    other => format!(
                        "The token failed verification: {}. Redo step 6 (create or \
                         adjust the token) and hand me the new key.",
                        esc(&crate::tasks::check_failed_text(other, &deps.machine_login))
                    ),
                };
                self.return_to_intake(reason, api).await;
            }
        }
    }

    /// Step 8: `owner/repo` lines through the grant flow, ONE checklist
    /// message updated in place per repo outcome. The grants run in a
    /// spawned worker (final-review fix 2): the router dispatch returns
    /// after the ack render, so commands and buttons stay live while a
    /// real grant polls GitHub for as long as it takes.
    async fn step8_repos(&self, text: &str, chat: i64, api: Arc<dyn ControlApi>) {
        let deps = &self.deps;
        let mut entries = load_checklist(&deps.state);
        let fresh: Vec<String> = text
            .split(|c: char| c.is_whitespace() || c == ',')
            .map(str::trim)
            .filter(|t| crate::control::commands::valid_repo(t).is_some())
            .map(String::from)
            .filter(|repo| !entries.iter().any(|(r, _)| r == repo))
            .collect();
        if fresh.is_empty() {
            self.edit_step_note(
                8,
                "I couldn't find a new owner/repo in that — send lines like \
                 <code>acme/widget</code>."
                    .to_string(),
                &api,
            )
            .await;
            return;
        }
        for repo in &fresh {
            entries.push((
                repo.clone(),
                GrantOutcome::Pending {
                    detail: INFLIGHT_DETAIL.to_string(),
                },
            ));
        }
        save_checklist(&deps.state, &entries);
        self.render_step8(&api).await;

        let notify = NotifyHandle {
            api: api.clone(),
            chat_id: chat,
        };
        tokio::spawn(run_step8_grants(deps.clone(), api, notify, fresh));
    }

    // -- shared plumbing ----------------------------------------------------

    /// Advance from `from` to `to`: stamp the completed step, then
    /// re-show the walkthrough message as the next step (edit in place).
    /// Leaving step 5 enters the 6+7 intake pair instead.
    async fn advance_from(&self, from: u8, to: u8, api: &Arc<dyn ControlApi>) {
        let deps = &self.deps;
        pset_with_ts(&deps.state, &format!("ts:{from}"), "1", state::now());
        if from == 5 {
            self.enter_intake_pair(api).await;
            return;
        }
        pset(&deps.state, KEY_STEP, &to.to_string());
        if let Some(anchor) = stored_anchor(&deps.state) {
            let ctx = StepCtx::load(&deps.state, &deps.machine_login, &deps.gate_public_url);
            let _ = api.edit_message(anchor, ctx.req(to)).await;
        }
    }

    /// Enter the 6+7 pair (from step 5's Done): ensure the form nonce
    /// (clearing a stale rotate marker), re-show step 6 on the anchor,
    /// then fresh-send the step-7 intake ask which becomes the new anchor.
    async fn enter_intake_pair(&self, api: &Arc<dyn ControlApi>) {
        let deps = &self.deps;
        let nonce = ensure_intake_nonce(&deps.state);
        pset(&deps.state, KEY_NONCE, &nonce);
        pset(&deps.state, KEY_STEP, "6");
        if let Some(anchor) = stored_anchor(&deps.state) {
            let ctx = StepCtx::load(&deps.state, &deps.machine_login, &deps.gate_public_url);
            let _ = api.edit_message(anchor, ctx.req(6)).await;
        }
        pset(&deps.state, KEY_STEP, "7");
        let ctx = StepCtx::load(&deps.state, &deps.machine_login, &deps.gate_public_url);
        let req = ctx.req(7).replying_to(MsgRef {
            chat_id: deps.chat_id,
            message_id: 0,
        });
        match api.send_message(req).await {
            Ok(anchor) => pset(&deps.state, KEY_MSG, &anchor_str(anchor)),
            Err(e) => eprintln!("setup: intake ask failed: {e}"),
        }
    }

    /// Return to step 6 after a failed intake verification: the reason is
    /// merged into the step-6 re-show (edit in place), then the intake ask
    /// is re-sent with the SAME nonce (still valid — nothing re-minted).
    async fn return_to_intake(&self, reason: String, api: Arc<dyn ControlApi>) {
        let deps = &self.deps;
        pset(&deps.state, KEY_STEP, "6");
        if let Some(anchor) = stored_anchor(&deps.state) {
            let ctx = StepCtx::load(&deps.state, &deps.machine_login, &deps.gate_public_url);
            let mut req = ctx.req(6);
            req.text = format!("<b>Token check failed.</b> {reason}\n\n{}", req.text);
            let _ = api.edit_message(anchor, req).await;
        }
        pset(&deps.state, KEY_STEP, "7");
        let ctx = StepCtx::load(&deps.state, &deps.machine_login, &deps.gate_public_url);
        let req = ctx.req(7).replying_to(MsgRef {
            chat_id: deps.chat_id,
            message_id: 0,
        });
        match api.send_message(req).await {
            Ok(anchor) => pset(&deps.state, KEY_MSG, &anchor_str(anchor)),
            Err(e) => eprintln!("setup: intake re-ask failed: {e}"),
        }
    }

    /// Enter step 8 (after a successful intake): fresh-send the repo ask,
    /// which becomes the checklist anchor.
    async fn enter_step8(&self, api: &Arc<dyn ControlApi>, chat: i64) {
        let deps = &self.deps;
        pset_with_ts(&deps.state, "ts:7", "1", state::now());
        pset(&deps.state, KEY_STEP, "8");
        pdelete(&deps.state, KEY_CHECKLIST);
        let ctx = StepCtx::load(&deps.state, &deps.machine_login, &deps.gate_public_url);
        let req = ctx.req(8).replying_to(MsgRef {
            chat_id: chat,
            message_id: 0,
        });
        match api.send_message(req).await {
            Ok(anchor) => pset(&deps.state, KEY_MSG, &anchor_str(anchor)),
            Err(e) => eprintln!("setup: step-8 ask failed: {e}"),
        }
    }

    /// Re-render step 8 (ask + checklist) onto the stored anchor.
    async fn render_step8(&self, api: &Arc<dyn ControlApi>) {
        render_step8_for(&self.deps, api).await;
    }

    /// Finish (step 8's `[Finish setup]`): summary + command list +
    /// ruleset reminder as a fresh send, then clear the durable rows
    /// (including the email hint — never persisted beyond setup).
    async fn finish(&self, api: Arc<dyn ControlApi>, chat: i64) {
        let deps = &self.deps;
        let username = pget(&deps.state, KEY_USERNAME).unwrap_or_else(|| deps.machine_login.clone());
        let entries = load_checklist(&deps.state);
        let req = finish_summary(&username, &entries).replying_to(MsgRef {
            chat_id: chat,
            message_id: 0,
        });
        if api.send_message(req).await.is_err() {
            eprintln!("setup: finish summary delivery failed");
        }
        let _ = deps
            .state
            .lock()
            .expect("state lock must be poison-free")
            .setup_clear();
    }

    /// Callback dispatch: signed `setup:{nonce}:{action}` triples only.
    async fn on_callback(&self, update: Update, api: Arc<dyn ControlApi>) {
        let deps = &self.deps;
        let Some(cb) = update.callback else {
            return;
        };
        let Some(expected) = cb_nonce(&deps.state) else {
            return; // fail closed: no nonce, no action
        };
        let Some((flow, action)) = validate_callback(&cb.data, &expected) else {
            return;
        };
        if flow != SETUP_FLOW {
            return;
        }
        let anchor = stored_anchor(&deps.state).or(update.message_ref);
        let Some(step) = step_now(&deps.state) else {
            // Flow finished/cancelled: stale buttons on old messages.
            if let Some(to) = anchor {
                let _ = api
                    .edit_message(
                        to,
                        SendRequest::text(crate::control::ACTION_NO_LONGER_VALID),
                    )
                    .await;
            }
            return;
        };
        match (action.as_str(), step) {
            ("start", 0) => self.advance_from(0, 1, &api).await,
            ("done", 1) => self.advance_from(1, 2, &api).await,
            ("done", 3) => self.advance_from(3, 4, &api).await,
            ("done", 4) => self.advance_from(4, 5, &api).await,
            ("done", 5) => self.advance_from(5, 6, &api).await,
            ("done", 2) | ("check", 2) => {
                // Done-style on the auto-verified step re-runs the check;
                // the flow advances ONLY when GitHub says the account exists.
                match pget(&deps.state, KEY_USERNAME) {
                    Some(name) => {
                        let exists = deps.platform.user_exists(&name).await;
                        if exists {
                            pset_with_ts(&deps.state, "ts:2", "1", state::now());
                            self.advance_from(2, 3, &api).await;
                        } else {
                            self.edit_step_note(
                                2,
                                format!(
                                    "Still can't see <b>{}</b> on GitHub — the account \
                                     isn't created (or GitHub hasn't caught up). Finish \
                                     the signup, then try again.",
                                    esc(&name)
                                ),
                                &api,
                            )
                            .await;
                        }
                    }
                    None => {
                        self.edit_step_note(
                            2,
                            "Reply with the username first — I'll watch for it."
                                .to_string(),
                            &api,
                        )
                        .await;
                    }
                }
            }
            ("stuck", _) => {
                if let Some(to) = anchor {
                    let ctx = StepCtx::load(&deps.state, &deps.machine_login, &deps.gate_public_url);
                    let _ = api.edit_message(to, ctx.stuck(step)).await;
                }
            }
            ("cancel", _) => {
                let _ = deps
                    .state
                    .lock()
                    .expect("state lock must be poison-free")
                    .setup_clear();
                if let Some(to) = anchor {
                    let _ = api
                        .edit_message(
                            to,
                            SendRequest::html(
                                "Setup cancelled — progress cleared. Run <b>/setup</b> \
                                 to start over any time.",
                            ),
                        )
                        .await;
                }
            }
            ("finish", 8) => {
                let chat = anchor.map(|m| m.chat_id).unwrap_or(deps.chat_id);
                self.finish(api, chat).await;
            }
            _ => {
                if let Some(to) = anchor {
                    let _ = api
                        .edit_message(
                            to,
                            SendRequest::text(crate::control::ACTION_NO_LONGER_VALID),
                        )
                        .await;
                }
            }
        }
    }
}

/// Re-render step 8 (ask + checklist) onto the stored anchor — shared by
/// the handler and the spawned grant worker (the anchor row is durable, so
/// the worker renders from the same truth).
async fn render_step8_for(deps: &SetupDeps, api: &Arc<dyn ControlApi>) {
    let Some(anchor) = stored_anchor(&deps.state) else {
        return;
    };
    let ctx = StepCtx::load(&deps.state, &deps.machine_login, &deps.gate_public_url);
    let _ = api.edit_message(anchor, ctx.req(8)).await;
}

/// The spawned step-8 grant worker (final-review fix 2): grants run OFF
/// the router dispatch — a real grant polls GitHub for up to days, and an
/// inline await parked every command and button for that whole wait. Per
/// repo outcome the checklist updates in place via the durable anchor.
/// Liveness (`step == 8`) is checked BEFORE starting each grant and again
/// before each write — in one lock scope with the write, so a mid-grant
/// `[Cancel setup]` stays cleared and a late outcome never resurrects
/// rows. When this batch's last outcome lands and no entry is still
/// granting, the setup-completion summary sends and the durable rows
/// clear (the `[Finish setup]` ending, reached on its own).
async fn run_step8_grants(
    deps: Arc<SetupDeps>,
    api: Arc<dyn ControlApi>,
    notify: NotifyHandle,
    repos: Vec<String>,
) {
    for repo in &repos {
        if !step8_live(&deps.state) {
            return; // cancelled/finished before this grant could start
        }
        let outcome = deps.grant.grant(repo, &notify).await;
        {
            let guard = deps.state.lock().expect("state lock must be poison-free");
            if !step8_live_guarded(&guard) {
                return; // cancelled/finished mid-grant: the rows stay cleared
            }
            let mut entries = load_checklist_guarded(&guard);
            let Some(slot) = entries.iter_mut().find(|(r, _)| r == repo) else {
                return; // checklist replaced under us: nothing to update
            };
            slot.1 = outcome;
            if let Ok(json) = serde_json::to_string(&entries) {
                let _ = guard.setup_set(KEY_CHECKLIST, &json);
            }
        }
        render_step8_for(&deps, &api).await; // in-place update per repo
    }
    // Completion: decide AND clear under one lock, so a concurrent batch
    // can never double-send — its liveness check fails after the clear.
    let summary = {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        if !step8_live_guarded(&guard) {
            None
        } else {
            let entries = load_checklist_guarded(&guard);
            if entries.iter().any(|(_, o)| o.detail() == INFLIGHT_DETAIL) {
                None // another batch is still granting; it completes setup
            } else {
                let username = guard
                    .setup_get(KEY_USERNAME)
                    .ok()
                    .flatten()
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| deps.machine_login.clone());
                let _ = guard.setup_clear();
                Some(finish_summary(&username, &entries).replying_to(MsgRef {
                    chat_id: notify.chat_id,
                    message_id: 0,
                }))
            }
        }
    };
    if let Some(req) = summary {
        let _ = notify.send(req).await;
    }
}

/// The one-time form's success hook (FIRST setup only — `onepage.rs`
/// calls this only when the submitted nonce did NOT match
/// `rotate_in_progress`). Advances an ACTIVE setup flow to step 8 and
/// sends the repo-access ask into the operator chat. With no notify
/// channel the state still advances; `/setup` resumes there.
pub async fn form_stored_hook(
    state: &Arc<Mutex<DbState>>,
    notify: &Option<(Arc<dyn ControlApi>, i64)>,
) {
    if pget(state, KEY_STEP).is_none() {
        return; // no active walkthrough
    }
    pset_with_ts(state, "ts:7", "1", state::now());
    pset(state, KEY_STEP, "8");
    pdelete(state, KEY_NONCE);
    let Some((api, chat)) = notify else {
        return;
    };
    let ctx = StepCtx {
        machine_login: String::new(),
        username: None,
        email_hint: None,
        // The ask carries [Finish setup]/[I'm stuck]/[Cancel setup] —
        // signed with the CB nonce so the buttons actually work.
        cb_nonce: ensure_cb_nonce(state),
        form_nonce: None,
        checklist: None,
        gate_public_url: String::new(),
    };
    let req = ctx.req(8).replying_to(MsgRef {
        chat_id: *chat,
        message_id: 0,
    });
    match api.send_message(req).await {
        Ok(anchor) => pset(state, KEY_MSG, &anchor_str(anchor)),
        Err(e) => eprintln!("setup: step-8 ask after form success failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> StepCtx {
        StepCtx {
            machine_login: "rob-agent".to_string(),
            username: None,
            email_hint: None,
            cb_nonce: "abcd0123456789ef".to_string(),
            form_nonce: Some("ab".repeat(32)),
            checklist: None,
            gate_public_url: "https://gag.example.com".to_string(),
        }
    }

    #[test]
    fn username_grammar_mirrors_github() {
        assert!(valid_username("rob-agent"));
        assert!(valid_username("a"));
        assert!(valid_username("A-bx1"));
        assert!(valid_username("rob123"));
        assert!(!valid_username(""));
        assert!(!valid_username("-lead"));
        assert!(!valid_username("trail-"));
        assert!(!valid_username("double--hyphen"));
        assert!(!valid_username("has space"));
        assert!(!valid_username("under_score"));
        assert!(!valid_username(&"a".repeat(40)));
        assert!(valid_username(&"a".repeat(39)));
    }

    #[test]
    fn plus_alias_renders_and_rejects_junk() {
        assert_eq!(
            plus_alias("rob@example.com").as_deref(),
            Some("rob+agent@example.com")
        );
        assert_eq!(plus_alias("  rob@example.com "), plus_alias("rob@example.com"));
        assert_eq!(plus_alias("nope"), None);
        assert_eq!(plus_alias("@example.com"), None);
        assert_eq!(plus_alias("rob@"), None);
        assert_eq!(plus_alias("rob@localhost"), None, "domain needs a dot");
    }

    #[test]
    fn every_step_carries_stuck_and_cancel_buttons() {
        let c = ctx();
        for step in 0..=8u8 {
            let req = c.req(step);
            assert!(
                req.inline_buttons.iter().any(|(l, _)| l == "I'm stuck"),
                "step {step} lacks [I'm stuck]: {:?}",
                req.inline_buttons
            );
            assert!(
                req.inline_buttons.iter().any(|(l, _)| l == "Cancel setup"),
                "step {step} lacks [Cancel setup]: {:?}",
                req.inline_buttons
            );
            // Review round 1 (F1): buttons are signed with the SHORT
            // callback nonce — never the 64-hex form nonce — and every
            // callback_data stays within Telegram's 64-byte cap.
            assert_eq!(
                c.cb_nonce.len(),
                16,
                "the cb nonce is 16 hex chars"
            );
            for (label, data) in &req.inline_buttons {
                let action = match label.as_str() {
                    "Start" => "start",
                    "Check again" => "check",
                    "Finish setup" => "finish",
                    "I'm stuck" => "stuck",
                    "Cancel setup" => "cancel",
                    _ => "done",
                };
                assert_eq!(
                    data,
                    &format!("{SETUP_FLOW}:{}:{action}", c.cb_nonce),
                    "step {step} button '{label}' is a cb-nonce-signed triple"
                );
                assert!(
                    data.len() <= 64,
                    "step {step} callback_data exceeds Telegram's 64-byte limit: {data}"
                );
            }
        }
        // The form nonce never leaks into button data.
        for step in 0..=8u8 {
            for (_, data) in c.req(step).inline_buttons {
                assert!(
                    !data.contains(&"ab".repeat(32)),
                    "form nonce must never sign a button: {data}"
                );
            }
        }
    }

    #[test]
    fn done_buttons_live_only_on_steps_one_to_five() {
        let c = ctx();
        for step in 0..=8u8 {
            let has_done = c.req(step).inline_buttons.iter().any(|(l, _)| l.starts_with("Done"));
            assert_eq!(
                has_done,
                (1..=5).contains(&step),
                "step {step} Done-button presence wrong"
            );
        }
        // Step 0 has Start; step 2 carries exactly Done (ntfy caps actions
        // at three: base I'm-stuck/Cancel + one more); step 8 Finish.
        let b = |step: u8, label: &str| {
            c.req(step)
                .inline_buttons
                .iter()
                .any(|(l, _)| l == label)
        };
        assert!(b(0, "Start"));
        assert!(b(2, "Done, account created"));
        assert!(!b(2, "Check again"), "dropped to respect ntfy's 3-action cap");
        assert!(b(8, "Finish setup"));
        assert!(!b(6, "Finish setup"));
    }

    #[test]
    fn step_seven_renders_form_link_or_paste_fallback() {
        let mut c = ctx();
        let form = c.req(7);
        let (_, form_url) = form
            .link_buttons
            .iter()
            .find(|(label, _)| label.contains("form"))
            .expect("form mode carries the form link");
        assert!(
            form_url.starts_with("https://gag.example.com/setup/token?nonce="),
            "form mode: {:?}",
            form.link_buttons
        );
        // The URL carries the 64-hex FORM nonce — and only that one.
        assert_eq!(
            form_url.split_once("nonce=").map(|(_, n)| n),
            Some(c.form_nonce.as_deref().unwrap()),
            "the one-time form nonce rides in the url"
        );
        assert!(
            !form_url.contains(&c.cb_nonce),
            "the cb nonce never appears in the form url: {form_url}"
        );

        c.gate_public_url = String::new();
        let fallback = c.req(7);
        assert!(
            fallback.link_buttons.is_empty(),
            "fallback carries no link: {:?}",
            fallback.link_buttons
        );
        assert!(
            fallback
                .text
                .contains("Paste the key here as a message. I'll delete your message right away."),
            "fallback copy verbatim: {}",
            fallback.text
        );
    }

    #[test]
    fn step_six_links_prefilled_token_page_and_promises_reminders() {
        let c = ctx();
        let req = c.req(6);
        assert!(
            req.link_buttons
                .iter()
                .any(|(_, url)| url == crate::github::TOKEN_NEW_URL),
            "prefilled link: {:?}",
            req.link_buttons
        );
        assert!(req.text.contains("14, 3 and 1"), "reminder promise: {}", req.text);
        assert!(
            !req.inline_buttons.iter().any(|(l, _)| l.starts_with("Done")),
            "step 6 has no Done button"
        );
    }

    #[test]
    fn stuck_extends_the_step_without_changing_buttons() {
        let c = ctx();
        for step in [0u8, 2, 7] {
            let base = c.req(step);
            let stuck = c.stuck(step);
            assert!(stuck.text.len() > base.text.len(), "step {step}");
            assert!(stuck.text.contains("Common problems"), "step {step}");
            assert_eq!(stuck.inline_buttons, base.inline_buttons, "step {step}");
            assert_eq!(stuck.link_buttons, base.link_buttons, "step {step}");
        }
    }

    #[test]
    fn checklist_renders_marks_with_details() {
        let entries = vec![
            (
                "acme/one".to_string(),
                GrantOutcome::Granted {
                    role: "write".to_string(),
                },
            ),
            (
                "acme/two".to_string(),
                GrantOutcome::Pending {
                    detail: "invitation sent".to_string(),
                },
            ),
            (
                "acme/bad".to_string(),
                GrantOutcome::Failed {
                    reason: "role_unavailable".to_string(),
                },
            ),
        ];
        let text = render_checklist(&entries);
        assert!(text.contains("acme/one ✅ granted (write)"), "{text}");
        assert!(text.contains("acme/two ⏳ invitation sent"), "{text}");
        assert!(text.contains("acme/bad ❌ role_unavailable"), "{text}");
    }

    #[test]
    fn finish_summary_lists_commands_ruleset_and_guide() {
        let summary = finish_summary(
            "rob-agent",
            &[(
                "acme/one".to_string(),
                GrantOutcome::Granted {
                    role: "write".to_string(),
                },
            )],
        );
        assert!(summary.text.contains("rob-agent"));
        assert!(summary.text.contains("/pending") && summary.text.contains("/rotate"));
        assert!(summary.text.contains("ruleset backstop"));
        assert!(summary.text.contains("bypass"));
        assert!(
            summary
                .link_buttons
                .iter()
                .any(|(_, url)| url.ends_with("gitauthgate-setup.md")),
            "{:?}",
            summary.link_buttons
        );
    }

    #[test]
    fn anchor_roundtrip() {
        let m = MsgRef {
            chat_id: -100_200,
            message_id: 42,
        };
        assert_eq!(parse_anchor(&anchor_str(m)), Some(m));
        assert_eq!(parse_anchor("junk"), None);
    }
}
