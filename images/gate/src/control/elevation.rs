//! Elevation flows (GATE-010 §8–9, task 8): when an action needs a role the
//! machine account does not hold, the proxies fail FAST
//! (`permission_pending` / a git ERR) and this module walks the operator
//! through the GitHub role change — the only human-in-the-loop that is a
//! role gap (approval holds are the other kind, and they are bounded).
//!
//! # Durable state
//!
//! Everything lives in the `flows` table (`kind='elevation'`), one row per
//! repo: `DETECTED → AWAITING_USER_CONSENT → AWAITING_ROLE_CHANGE →
//! VERIFYING → DONE | CANCELLED | EXPIRED (24 h without progress)`. The
//! proxies open rows via [`open_or_join_flow`] — at most ONE active flow
//! per repo, so concurrent role gaps land in the same row (extra actions
//! are appended, the needed role is raised to the max) — and the sweep
//! ([`run_elevation_sweep`]) takes over every DETECTED row, no matter which
//! proxy created it. The 403-admin-rights error mapping (core) produces the
//! same row shape, so `/proxy/elevations/{id}` and this state machine see
//! one system.
//!
//! # Batching window
//!
//! A DETECTED row is claimed only once it has been quiet for
//! [`ElevationDeps::batch_window`] (GATE-010: ~10 s): the consent message
//! then presents EVERY gap collected in the window as ONE combined message
//! instead of spamming one message per request. Joining a gap
//! ([`open_or_join_flow`]) does not stamp `updated_at` (see
//! `State::flow_patch_quiet`), so the window measures time since the first
//! detection, not since the last join.
//!
//! # The conversation
//!
//! 1. **Consent** — why (the actions), needs (max role), has (current
//!    role), one plain-words line on what the new role adds
//!    ([`role_description`]), and buttons `[Grant it] [Grant until done,
//!    then remove] [Not now]`.
//! 2. **Role change** — exact-click instructions + `[Open collaborator
//!    settings]` (URL from the adapter helper per B7's ruling).
//! 3. **Verify** — poll `repo_permissions` every [`ElevationDeps::poll`]
//!    (GATE-010: 5 s). A wrong-but-changed role gets a message naming the
//!    difference (once per distinct wrong role) and polling continues; 10
//!    minutes of silence get ONE reminder. On target: registry refresh +
//!    success message ("The agent will retry its request"); a temporary
//!    grant additionally auto-starts the reduction to the pre-elevation
//!    role through the [`ReductionFlow`] seam.
//!
//! `[Not now]` cancels; the sweep expires any flow without progress for
//! [`ElevationDeps::expire_after`] (GATE-010: 24 h).
//!
//! # THE REDUCTION SEAM (task 9 — filled in `grant.rs`)
//!
//! Temporary grants ("grant until done, then remove") end with an
//! automatic role REDUCTION back to `intended_role`. The seam fires at
//! DONE (the binding ruling — the agent's retry is already unblocked and
//! the reducer owns its outcome), fire-and-forget:
//!
//! * the [`ReductionFlow`] trait (async, dyn-safe, called with the repo and
//!   the role to reduce to);
//! * [`NullReductionFlow`], the no-op placeholder;
//! * the call site in [`run_elevation_flow`]: fired after DONE is written.
//!
//! `main.rs` wires [`crate::control::grant::RealReductionFlow`] as the
//! reducer (GATE-010 §10: instructions + poll-verify + registry update).
//! When a forwarded request on a temporarily-elevated repo answers 2xx,
//! the apiproxy records that through [`mark_work_done`] — bookkeeping
//! only, the reduction still proceeds at DONE.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::audit::{Audit, AuditEntry};
use crate::github::Platform;
use crate::registry::RegistryDeps;
use crate::rolemapping::{role_from_name, role_name};
use crate::state::{self, State as DbState};
use crate::control::send::SendRequest;
use crate::control::{nonce, NonceLookup, ControlApi, Update, UpdateHandler};
use crate::types::Role;

/// `flows.kind` for elevations — the same value the core error mapping
/// writes, so both planes share one flow system.
pub const FLOW_KIND: &str = "elevation";

// -- flow states -------------------------------------------------------------

pub const STATE_DETECTED: &str = "DETECTED";
pub const STATE_AWAITING_USER_CONSENT: &str = "AWAITING_USER_CONSENT";
pub const STATE_AWAITING_ROLE_CHANGE: &str = "AWAITING_ROLE_CHANGE";
pub const STATE_VERIFYING: &str = "VERIFYING";
pub const STATE_DONE: &str = "DONE";
pub const STATE_CANCELLED: &str = "CANCELLED";
pub const STATE_EXPIRED: &str = "EXPIRED";

/// States a live flow can be in; anything else is terminal.
pub fn is_active_state(state: &str) -> bool {
    matches!(
        state,
        STATE_DETECTED | STATE_AWAITING_USER_CONSENT | STATE_AWAITING_ROLE_CHANGE | STATE_VERIFYING
    )
}

// -- payload ------------------------------------------------------------------

/// One recorded role gap (an action the account could not perform).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElevationAction {
    pub path: String,
    pub role: String,
}

/// The JSON payload of an elevation flow row. Every field defaults so the
/// core placeholder shape (`{repo, path, reason}`) deserializes cleanly and
/// unknown future fields survive round trips.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ElevationPayload {
    /// The repo the gap is on (`owner/repo`) — the flow key.
    #[serde(default)]
    pub repo: String,
    /// Highest needed role across all recorded actions (registry spelling).
    #[serde(default)]
    pub needed_role: String,
    /// Every action collected for this flow (the "why" list).
    #[serde(default)]
    pub actions: Vec<ElevationAction>,
    /// Legacy/core placeholder fields, preserved verbatim.
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    /// Signed-button nonce (set when consent goes out).
    #[serde(default)]
    pub nonce: Option<String>,
    /// `[Grant until done, then remove]` ⇒ `Some(true)`.
    #[serde(default)]
    pub temporary: Option<bool>,
    /// Reduction target for a temporary grant (the pre-elevation role).
    #[serde(default)]
    pub intended_role: Option<String>,
    /// Consent message anchor (`chat_id`, `message_id`) for in-place edits.
    #[serde(default)]
    pub msg_chat: Option<i64>,
    #[serde(default)]
    pub msg_id: Option<i32>,
    /// The sweep claims DETECTED rows by setting this (crash-safe: a
    /// claimed row whose task died still expires with the 24 h sweep).
    #[serde(default)]
    pub claimed: Option<bool>,
    /// The one 10-minute reminder has been sent.
    #[serde(default)]
    pub reminder_sent: Option<bool>,
    /// Distinct wrong roles already reported (one message each).
    #[serde(default)]
    pub wrong_roles: Vec<String>,
    /// §9.2 bookkeeping: set to 1 when the apiproxy observed a 2xx on a
    /// retried/forwarded request for this repo while the temporary grant
    /// held ("the triggering request succeeded"). A RECORD ONLY — the
    /// reduction always proceeds at DONE (B9 ruling: no second trigger).
    #[serde(default)]
    pub work_done: Option<i64>,
}

impl ElevationPayload {
    fn to_json(&self) -> String {
        serde_json::to_string(self).expect("elevation payload must serialize")
    }

    pub fn from_json(raw: &str) -> Self {
        serde_json::from_str(raw).unwrap_or_default()
    }

    fn msg_ref(&self) -> Option<crate::control::send::MsgRef> {
        Some(crate::control::send::MsgRef {
            chat_id: self.msg_chat?,
            message_id: self.msg_id?,
        })
    }
}

/// Plain-words line on what a role adds (GATE-010: consent message).
pub fn role_description(role: Role) -> &'static str {
    match role {
        Role::None => "no access at all",
        Role::Read => "browse the code and read issues",
        Role::Triage => "manage issues — labels, closing and assigning",
        Role::Write => "push commits and open or merge pull requests",
        Role::Maintain => "manage repository metadata — topics and Pages",
        Role::Admin => "change repository settings, collaborators and rules",
    }
}

// -- flow creation (the proxies' entry point) ---------------------------------

/// Open (or join) the elevation flow for `repo`: an ACTIVE flow for the
/// repo gains this gap (action appended, needed role raised), otherwise a
/// fresh DETECTED row is minted. Returns the flow id the caller reports
/// back to the agent (`elevation_id`). Synchronous — the proxies call it
/// under the state lock on the request path (fail-fast: this is all the
/// request path does; the conversation happens in the sweep's tasks).
pub fn open_or_join_flow(
    state: &DbState,
    repo: &str,
    path: &str,
    needed_role: Role,
) -> String {
    let needed = role_name(needed_role).to_string();
    let active = match state.flows_list(FLOW_KIND) {
        Ok(rows) => rows.into_iter().find(|row| {
            is_active_state(&row.flow_state)
                && ElevationPayload::from_json(&row.payload_json).repo == repo
        }),
        Err(_) => None,
    };
    if let Some(row) = active {
        let mut payload = ElevationPayload::from_json(&row.payload_json);
        if role_from_name(&needed).map_or(true, |new| {
            role_from_name(&payload.needed_role).map_or(true, |cur| new > cur)
        }) {
            payload.needed_role = needed.clone();
        }
        if !payload
            .actions
            .iter()
            .any(|a| a.path == path && a.role == needed)
        {
            payload.actions.push(ElevationAction {
                path: path.to_string(),
                role: needed.clone(),
            });
        }
        // A quiet patch: joining must not reset the batching clock.
        let _ = state.flow_patch_quiet(&row.id, &row.flow_state, &payload.to_json());
        return row.id;
    }
    let id = state::uuid_v4();
    let payload = ElevationPayload {
        repo: repo.to_string(),
        needed_role: needed,
        actions: vec![ElevationAction {
            path: path.to_string(),
            role: role_name(needed_role).to_string(),
        }],
        ..Default::default()
    };
    state
        .flow_insert(&id, FLOW_KIND, STATE_DETECTED, &payload.to_json())
        .expect("elevation flow row must persist");
    id
}

// -- deps ---------------------------------------------------------------------

/// Everything the elevation sweep and the button handler read. Cloneable
/// (arcs + strings); the durations shrink in tests.
#[derive(Clone)]
pub struct ElevationDeps {
    pub state: Arc<Mutex<DbState>>,
    pub platform: Arc<dyn Platform>,
    /// The machine account login named in every message.
    pub machine_login: String,
    /// `(api, chat_id)` for gate-initiated sends; `None` (no bot) ⇒ flows
    /// stay DETECTED and expire with the sweep — the proxies' fail-fast
    /// answers do not depend on the bot.
    pub notify: Option<(Arc<dyn ControlApi>, i64)>,
    /// Consent-batch collection window (GATE-010: ~10 s).
    pub batch_window: Duration,
    /// Verify-poll cadence (GATE-010: 5 s).
    pub poll: Duration,
    /// Silence before the ONE reminder (GATE-010: 10 min).
    pub reminder_after: Duration,
    /// No-progress expiry (GATE-010: 24 h).
    pub expire_after: Duration,
    /// The Task-9 reduction seam (temporary grants call it on DONE).
    pub reduction: Arc<dyn ReductionFlow>,
    pub audit: Arc<Audit>,
}

// -- the reduction seam (task 9 — filled in `grant.rs`, see module docs) -------

/// Temporary-grant reduction seam: after a "grant until done, then remove"
/// elevation completes, reduce `repo` back to `to`. The real reducer is
/// [`crate::control::grant::RealReductionFlow`] (wired in `main.rs`);
/// [`NullReductionFlow`] remains the no-op placeholder.
#[async_trait]
pub trait ReductionFlow: Send + Sync {
    async fn reduce(&self, repo: &str, to: Role);
}

/// Placeholder for deployments without the reducer: logs the intent and
/// does nothing.
pub struct NullReductionFlow;

#[async_trait]
impl ReductionFlow for NullReductionFlow {
    async fn reduce(&self, repo: &str, to: Role) {
        eprintln!(
            "elevation: reduction deferred (no reducer wired): {repo} back to {to}"
        );
    }
}

/// Mark `work_done = 1` on every temporary elevation flow of `repo` that is
/// still open or DONE — the apiproxy calls this when a forwarded request on
/// a temporarily-elevated repo answered 2xx (§9.2 "after the triggering
/// request succeeds"). Best-effort bookkeeping: it never errors, never
/// touches non-temporary flows, and never starts anything — the reduction
/// proceeds at DONE regardless.
pub fn mark_work_done(state: &DbState, repo: &str) {
    let rows = match state.flows_list(FLOW_KIND) {
        Ok(rows) => rows,
        Err(_) => return,
    };
    for row in rows {
        if row.flow_state != STATE_DONE && !is_active_state(&row.flow_state) {
            continue;
        }
        let mut payload = ElevationPayload::from_json(&row.payload_json);
        if payload.repo != repo
            || payload.temporary != Some(true)
            || payload.work_done == Some(1)
        {
            continue;
        }
        payload.work_done = Some(1);
        // Quiet patch: bookkeeping must not move the flow's clock.
        let _ = state.flow_patch_quiet(&row.id, &row.flow_state, &payload.to_json());
    }
}

// -- sweep --------------------------------------------------------------------

/// The elevation sweep: expires stale flows and takes over DETECTED rows,
/// spawning one conversation task per claim. Runs whether or not the bot
/// is configured — expiry must always make progress; claiming is skipped
/// without a channel (a consent ask nobody can see would be noise).
pub async fn run_elevation_sweep(
    deps: ElevationDeps,
    mut shutdown: watch::Receiver<bool>,
    interval: Duration,
) {
    shutdown.borrow_and_update();
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep(interval) => {
                sweep_tick(&deps).await;
            }
        }
    }
}

/// One sweep pass: (1) expire non-terminal flows without progress for
/// `expire_after`; (2) claim quiet DETECTED rows and start their
/// conversations. Returns how many flows were claimed.
pub async fn sweep_tick(deps: &ElevationDeps) -> usize {
    let rows = {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        guard.flows_list(FLOW_KIND).unwrap_or_default()
    };
    let now = state::now();
    let cutoff = now.saturating_sub(deps.expire_after.as_secs() as i64);
    for row in &rows {
        if is_active_state(&row.flow_state) && row.updated_at < cutoff {
            let guard = deps.state.lock().expect("state lock must be poison-free");
            let _ = guard.flow_insert(&row.id, FLOW_KIND, STATE_EXPIRED, &row.payload_json);
        }
    }
    if deps.notify.is_none() {
        return 0;
    }
    let mut claimed = 0;
    for row in &rows {
        if row.flow_state != STATE_DETECTED {
            continue;
        }
        let mut payload = ElevationPayload::from_json(&row.payload_json);
        if payload.claimed == Some(true) {
            continue;
        }
        // Batching window: only claim rows quiet for the whole window, so
        // concurrent gaps land in the same combined message.
        if now.saturating_sub(row.updated_at)
            < deps.batch_window.as_secs() as i64
        {
            continue;
        }
        payload.claimed = Some(true);
        {
            let guard = deps.state.lock().expect("state lock must be poison-free");
            let _ = guard.flow_insert(&row.id, FLOW_KIND, STATE_DETECTED, &payload.to_json());
        }
        claimed += 1;
        let task_deps = deps.clone();
        let id = row.id.clone();
        tokio::spawn(run_elevation_flow(task_deps, id));
    }
    claimed
}

// -- the conversation ----------------------------------------------------------

/// The full elevation conversation for one flow id: consent (after the
/// batching window) → role-change instructions → verify loop → DONE /
/// CANCELLED / EXPIRED. The DB is the coordination point — the button
/// handler writes the decision, this task polls for it — so the flow is
/// durable in `flows` even though the task itself dies with the process
/// (an orphaned flow then simply expires with the sweep).
pub async fn run_elevation_flow(deps: ElevationDeps, flow_id: String) {
    // Consent. The row was quiet for the whole window when claimed; read it
    // fresh and present everything recorded so far as ONE message.
    let payload = {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        let row = match guard.flow_get(&flow_id) {
            Ok(Some(row)) => row,
            _ => return,
        };
        if row.flow_state != STATE_DETECTED {
            return; // expired/cancelled between claim and now
        }
        ElevationPayload::from_json(&row.payload_json)
    };
    let has_role = {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        guard
            .registry_get(&payload.repo)
            .ok()
            .flatten()
            .and_then(|r| role_from_name(&r.granted_role))
            .unwrap_or(Role::None)
    };
    let Some(needed) = role_from_name(&payload.needed_role) else {
        return;
    };
    let cb_nonce = nonce();
    let consent = consent_text(&deps.machine_login, &payload.repo, needed, has_role, &payload.actions);
    let req = SendRequest::text(consent)
        .with_inline_button("Grant it", &flow_id, &cb_nonce, ACTION_GRANT)
        .with_inline_button("Grant until done, then remove", &flow_id, &cb_nonce, ACTION_GRANT_TEMP)
        .with_inline_button("Not now", &flow_id, &cb_nonce, ACTION_NOT_NOW);
    let mut payload = payload;
    payload.nonce = Some(cb_nonce);
    match send_to_chat(&deps, req).await {
        Some((chat_id, message_id)) => {
            payload.msg_chat = Some(chat_id);
            payload.msg_id = Some(message_id as i32);
            let guard = deps.state.lock().expect("state lock must be poison-free");
            let _ = guard.flow_insert(&flow_id, FLOW_KIND, STATE_AWAITING_USER_CONSENT, &payload.to_json());
        }
        None => {
            // Delivery failed: unclaim so the next sweep tick retries.
            payload.claimed = Some(false);
            let guard = deps.state.lock().expect("state lock must be poison-free");
            let _ = guard.flow_insert(&flow_id, FLOW_KIND, STATE_DETECTED, &payload.to_json());
            return;
        }
    }

    // Wait for the operator's decision (the button handler writes it).
    let mut temporary = false;
    loop {
        tokio::time::sleep(deps.poll).await;
        let state_str = {
            let guard = deps.state.lock().expect("state lock must be poison-free");
            guard.flow_state(&flow_id).ok().flatten()
        };
        match state_str.as_deref() {
            Some(STATE_AWAITING_ROLE_CHANGE) => {
                let guard = deps.state.lock().expect("state lock must be poison-free");
                if let Some(row) = guard.flow_get(&flow_id).ok().flatten() {
                    // Full payload refresh: the press may have set the
                    // temporary flag + reduction target.
                    payload = ElevationPayload::from_json(&row.payload_json);
                    temporary = payload.temporary == Some(true);
                }
                break;
            }
            Some(STATE_CANCELLED) | Some(STATE_EXPIRED) => return,
            _ => {} // still awaiting consent (or a momentary read hiccup)
        }
    }

    // Role-change instructions: exact clicks + the settings link. The
    // 10-minute silence clock starts HERE (a consent that took hours must
    // not earn an instant reminder).
    let Some((owner, name)) = payload.repo.split_once('/') else {
        return;
    };
    let settings_url = crate::github::collaborator_settings_url(owner, name);
    let _ = send_to_chat(
        &deps,
        SendRequest::text(instructions_text(
            &deps.machine_login,
            &payload.repo,
            needed,
        ))
        .with_link_button("Open collaborator settings", settings_url.clone()),
    )
    .await;
    let instructions_at = std::time::Instant::now();
    {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        let _ = guard.flow_insert(&flow_id, FLOW_KIND, STATE_VERIFYING, &payload.to_json());
    }

    // Verify loop: poll repo_permissions until the role reaches the target.
    let mut wrong_roles: Vec<Role> = payload
        .wrong_roles
        .iter()
        .filter_map(|r| role_from_name(r))
        .collect();
    loop {
        tokio::time::sleep(deps.poll).await;
        // Re-read the CURRENT needed role every check: a gap that joined the
        // flow after consent may have raised it.
        let (repo, needed_now, payload_now) = {
            let guard = deps.state.lock().expect("state lock must be poison-free");
            let Some(row) = guard.flow_get(&flow_id).ok().flatten() else {
                return;
            };
            let p = ElevationPayload::from_json(&row.payload_json);
            let needed = role_from_name(&p.needed_role).unwrap_or(needed);
            (p.repo.clone(), needed, p)
        };
        match deps.platform.repo_permissions(owner, name).await {
            Ok(access) if access.role >= needed_now => {
                finish_done(&deps, &flow_id, &repo, needed_now, temporary, &payload_now).await;
                return;
            }
            Ok(access)
                if access.role != Role::None
                    && !wrong_roles.contains(&access.role) =>
            {
                wrong_roles.push(access.role);
                let _ = send_to_chat(
                    &deps,
                    SendRequest::text(wrong_role_text(
                        &deps.machine_login,
                        &repo,
                        access.role,
                        needed_now,
                    ))
                    .with_link_button("Open collaborator settings", settings_url.clone()),
                )
                .await;
                let mut p = payload_now.clone();
                p.wrong_roles = wrong_roles.iter().map(|r| role_name(*r).to_string()).collect();
                let guard = deps.state.lock().expect("state lock must be poison-free");
                let _ = guard.flow_insert(&flow_id, FLOW_KIND, STATE_VERIFYING, &p.to_json());
            }
            _ => {} // unchanged / invisible / upstream hiccup: keep polling
        }
        // ONE reminder after the silence window.
        if payload_now.reminder_sent != Some(true)
            && instructions_at.elapsed() >= deps.reminder_after
        {
            let mut p = payload_now.clone();
            p.reminder_sent = Some(true);
            let _ = send_to_chat(
                &deps,
                SendRequest::text(reminder_text(&deps.machine_login, &repo, needed_now))
                    .with_link_button("Open collaborator settings", settings_url.clone()),
            )
            .await;
            let guard = deps.state.lock().expect("state lock must be poison-free");
            let _ = guard.flow_insert(&flow_id, FLOW_KIND, STATE_VERIFYING, &p.to_json());
        }
        // Bail out when the sweep expired or the operator cancelled.
        let state_str = {
            let guard = deps.state.lock().expect("state lock must be poison-free");
            guard.flow_state(&flow_id).ok().flatten()
        };
        if matches!(state_str.as_deref(), Some(STATE_CANCELLED) | Some(STATE_EXPIRED)) {
            return;
        }
    }
}

/// Write DONE: reconcile the registry (`refresh`, then the elevation's own
/// metadata), send the success message, and auto-start the reduction for a
/// temporary grant through the seam.
async fn finish_done(
    deps: &ElevationDeps,
    flow_id: &str,
    repo: &str,
    needed: Role,
    temporary: bool,
    payload: &ElevationPayload,
) {
    let registry = RegistryDeps {
        state: deps.state.clone(),
        platform: deps.platform.clone(),
        machine_login: deps.machine_login.clone(),
        notify: deps.notify.clone(),
    };
    crate::registry::refresh(&registry, repo).await;
    // The intended role: the elevated tier for a permanent grant; the
    // pre-elevation role (the reduction target) for a temporary one.
    let intended = if temporary {
        payload
            .intended_role
            .clone()
            .unwrap_or_else(|| role_name(Role::None).to_string())
    } else {
        role_name(needed).to_string()
    };
    {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        let granted = guard
            .registry_get(repo)
            .ok()
            .flatten()
            .map(|r| r.granted_role)
            .unwrap_or_else(|| role_name(needed).to_string());
        let _ = guard.registry_upsert(repo, &granted, &intended, "elevation");
        let _ = guard.flow_insert(flow_id, FLOW_KIND, STATE_DONE, &payload.to_json());
    }
    let _ = send_to_chat(
        deps,
        SendRequest::text(success_text(
            &deps.machine_login,
            repo,
            needed,
            temporary,
            role_from_name(&intended),
        )),
    )
    .await;
    if temporary {
        // Task-9 seam: fire-and-forget — DONE is already durable, the
        // agent's retry is unblocked, and the reducer owns its outcome.
        deps.reduction
            .reduce(repo, role_from_name(&intended).unwrap_or(Role::None))
            .await;
    }
}

async fn send_to_chat(deps: &ElevationDeps, req: SendRequest) -> Option<(i64, i32)> {
    let (api, chat_id) = deps.notify.as_ref()?;
    let anchored = req.replying_to(crate::control::send::MsgRef {
        chat_id: *chat_id,
        message_id: 0,
    });
    let msg = api.send_message(anchored).await.ok()?;
    Some((msg.chat_id, msg.message_id))
}

// -- copy -----------------------------------------------------------------------

/// Why / needs / has / one plain-words line + what the buttons mean.
fn consent_text(
    login: &str,
    repo: &str,
    needed: Role,
    has: Role,
    actions: &[ElevationAction],
) -> String {
    let mut text = format!(
        "🔑 Role elevation needed for {repo}\n\nWhy: the agent tried:\n"
    );
    for a in actions {
        text.push_str(&format!("- {} — needs {}\n", a.path, a.role));
    }
    text.push_str(&format!(
        "\nNeeds: {needed} — as {needed}, the account can {}.\nHas: {has}\n\n\
         Grant it and {login} gets the role on GitHub; I verify the change \
         and confirm here. \"Grant until done\" removes the role again \
         afterwards; \"Not now\" leaves the action blocked.",
        role_description(needed)
    ));
    text
}

/// Exact-click instructions for the role change.
fn instructions_text(login: &str, repo: &str, needed: Role) -> String {
    format!(
        "Granting {needed} on {repo}:\n\
         1. Open the collaborator settings (button below).\n\
         2. Find {login} and open the role selector.\n\
         3. Pick {needed} — GitHub labels each tier.\n\
         I check every few seconds and confirm here when it lands."
    )
}

/// Wrong-but-changed role: name the exact difference, re-ask.
fn wrong_role_text(login: &str, repo: &str, found: Role, needed: Role) -> String {
    format!(
        "{login} now has {found} on {repo}, but this needs {needed} — open the \
         collaborator settings and change {login}'s role to {needed}. I keep \
         checking until it matches."
    )
}

/// The single 10-minute-silence reminder.
fn reminder_text(login: &str, repo: &str, needed: Role) -> String {
    format!(
        "Still waiting: the {repo} role change to {needed} hasn't happened yet. \
         Open the collaborator settings and set {login}'s role to {needed} — \
         this is my only reminder; I keep checking quietly."
    )
}

/// Success: registry updated, the agent unblocks itself.
fn success_text(login: &str, repo: &str, needed: Role, temporary: bool, back_to: Option<Role>) -> String {
    let mut text = format!(
        "✅ {repo}: {login} now has {needed} — verified and recorded. \
         The agent will retry its request."
    );
    if temporary {
        match back_to {
            Some(to) => text.push_str(&format!(
                " This is a temporary grant — I'm reducing the role back to {to} now; \
                 watch for the reduction steps below."
            )),
            None => text.push_str(
                " This is a temporary grant — I'm removing the access again now; \
                 watch for the reduction steps below.",
            ),
        }
    }
    text
}

// -- the personal-repo ceiling (GATE-010 §9.3) ---------------------------------

/// The agent-facing message for a Maintain/Admin need on a personal repo:
/// the shape is the `role_unavailable` envelope; the text explains why no
/// elevation can exist and what happens instead.
pub fn diy_agent_message(repo: &str, needed: Role) -> String {
    format!(
        "insufficient role: this operation needs '{needed}', but {repo} is a \
         personal repo — GitHub offers role tiers only on organization repos, \
         so no elevation can grant it. The operator must perform this action \
         themselves (DIY instructions were sent to the control plane)."
    )
}

/// The operator's DIY instructions (plain words, GATE-010 §9.3): the one
/// thing the machine account can never be granted, and what to do instead.
pub fn diy_text(repo: &str, needed: Role, path: &str) -> String {
    format!(
        "⚠️ The agent needed {needed} on {repo} ({path}), but {repo} is a \
         personal repo — GitHub only offers role choices on org repos, so \
         the machine account can never hold {needed} there. No elevation \
         was opened.\n\nPlease do this one yourself on github.com: \
         personal-repo owners can perform {needed}-tier actions (settings, \
         topics, Pages) directly in the web UI. Everything below {needed} \
         still works for the agent."
    )
}

/// Best-effort delivery of the DIY instructions to the operator chat.
/// Fire-and-forget: the agent's `role_unavailable` answer is the binding
/// semantic; a failed/absent channel must not change it.
pub async fn maybe_send_diy(
    notify: &Option<(Arc<dyn ControlApi>, i64)>,
    repo: &str,
    needed: Role,
    path: &str,
) {
    let _ = crate::tasks::send_notice(
        notify,
        SendRequest::text(diy_text(repo, needed, path)),
    )
    .await;
}

// -- button handler ---------------------------------------------------------------

const ACTION_GRANT: &str = "grant";
const ACTION_GRANT_TEMP: &str = "grant_temp";
const ACTION_NOT_NOW: &str = "not_now";

/// Nonce resolver for the router's callback validation: elevation flow id →
/// the payload nonce. Only flows WITH a nonce resolve (a pre-consent flow
/// has no buttons, so nothing legitimate can reference it).
pub fn nonce_lookup(state: Arc<Mutex<DbState>>) -> NonceLookup {
    Arc::new(move |flow| {
        let guard = state.lock().expect("state lock must be poison-free");
        let row = guard.flow_get(flow).ok().flatten()?;
        if row.kind != FLOW_KIND {
            return None;
        }
        ElevationPayload::from_json(&row.payload_json).nonce
    })
}

/// Register the elevation button handler on a router.
pub fn register_elevation(
    router: &mut crate::control::ControlRouter,
    deps: Arc<ElevationHandler>,
) {
    router.register(deps);
}

/// The elevation button handler: `[Grant it]` / `[Grant until done, then
/// remove]` / `[Not now]` on consent messages. Callbacks for other flows
/// (approvals, rotate, setup) are ignored — their handlers own them.
pub struct ElevationHandler {
    deps: ElevationDeps,
}

impl ElevationHandler {
    pub fn new(deps: ElevationDeps) -> Self {
        Self { deps }
    }
}

#[async_trait]
impl UpdateHandler for ElevationHandler {
    async fn handle(&self, update: Update, api: Arc<dyn ControlApi>) {
        let Some(callback) = &update.callback else {
            return;
        };
        // Data triple: `{flow_id}:{nonce}:{action}` — the router has
        // already validated the nonce; the action is the third part.
        let mut parts = callback.data.splitn(3, ':');
        let flow_id = parts.next().unwrap_or("").to_string();
        let _nonce = parts.next().unwrap_or("");
        let action = parts.next().unwrap_or("").to_string();
        if !matches!(
            action.as_str(),
            ACTION_GRANT | ACTION_GRANT_TEMP | ACTION_NOT_NOW
        ) {
            return; // not an elevation button
        }
        let (row, payload) = {
            let guard = self.deps.state.lock().expect("state lock must be poison-free");
            match guard.flow_get(&flow_id).ok().flatten() {
                Some(row) if row.kind == FLOW_KIND => {
                    (row.flow_state, ElevationPayload::from_json(&row.payload_json))
                }
                _ => return,
            }
        };
        if row != STATE_AWAITING_USER_CONSENT {
            return; // already decided (or not yet asked): never double-apply
        }
        let repo = payload.repo.clone();
        let mut payload = payload;
        match action.as_str() {
            ACTION_GRANT => {
                payload.temporary = Some(false);
                payload.intended_role = None;
                self.decide(&flow_id, &repo, STATE_AWAITING_ROLE_CHANGE, false, &payload, callback.from_id, &api, update.message_ref)
                    .await;
            }
            ACTION_GRANT_TEMP => {
                // Reduction target: whatever the account holds right now.
                let current = {
                    let guard = self.deps.state.lock().expect("state lock must be poison-free");
                    guard
                        .registry_get(&repo)
                        .ok()
                        .flatten()
                        .and_then(|r| role_from_name(&r.granted_role))
                        .unwrap_or(Role::None)
                };
                payload.temporary = Some(true);
                payload.intended_role = Some(role_name(current).to_string());
                self.decide(&flow_id, &repo, STATE_AWAITING_ROLE_CHANGE, true, &payload, callback.from_id, &api, update.message_ref)
                    .await;
            }
            ACTION_NOT_NOW => {
                self.decide(&flow_id, &repo, STATE_CANCELLED, false, &payload, callback.from_id, &api, update.message_ref)
                    .await;
            }
            _ => unreachable!("action filtered above"),
        }
    }
}

impl ElevationHandler {
    /// Persist the decision (state + payload + audit), then update the
    /// consent message in place so the buttons go away.
    #[allow(clippy::too_many_arguments)]
    async fn decide(
        &self,
        flow_id: &str,
        repo: &str,
        new_state: &str,
        temporary: bool,
        payload: &ElevationPayload,
        approver: i64,
        api: &Arc<dyn ControlApi>,
        pressed: Option<crate::control::send::MsgRef>,
    ) {
        {
            let guard = self.deps.state.lock().expect("state lock must be poison-free");
            let _ = guard.flow_insert(flow_id, FLOW_KIND, new_state, &payload.to_json());
        }
        self.deps.audit.record(AuditEntry {
            ts: state::now(),
            agent: None,
            repo: repo.to_string(),
            action: "elevation.decision".to_string(),
            decision: match new_state {
                STATE_CANCELLED => "cancelled".to_string(),
                _ if temporary => "temporary".to_string(),
                _ => "approved".to_string(),
            },
            basis: format!("elevation {flow_id}: {repo}"),
            approver: Some(approver.to_string()),
        });
        let reply = match new_state {
            STATE_CANCELLED => format!(
                "Elevation for {repo} cancelled — the action stays blocked \
                 (permission_pending) until the role changes."
            ),
            _ => format!(
                "Granting — watch for the exact role-change steps below; \
                 I'll verify and confirm here."
            ),
        };
        let edit_req = SendRequest::text(reply);
        let target = payload
            .msg_ref()
            .or(pressed);
        if let Some(to) = target {
            let _ = api.edit_message(to, edit_req).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(repo: &str, needed: &str, actions: &[(&str, &str)]) -> ElevationPayload {
        ElevationPayload {
            repo: repo.to_string(),
            needed_role: needed.to_string(),
            actions: actions
                .iter()
                .map(|(p, r)| ElevationAction {
                    path: p.to_string(),
                    role: r.to_string(),
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn role_description_covers_every_tier_in_plain_words() {
        for role in [
            Role::None,
            Role::Read,
            Role::Triage,
            Role::Write,
            Role::Maintain,
            Role::Admin,
        ] {
            let d = role_description(role);
            assert!(!d.is_empty(), "{role:?} has a description");
        }
        assert_eq!(role_description(Role::Maintain), "manage repository metadata — topics and Pages");
    }

    #[test]
    fn payload_round_trips_and_parses_the_core_placeholder_shape() {
        let p = payload("acme/api", "maintain", &[("/repos/acme/api/topics", "maintain")]);
        let json = p.to_json();
        assert_eq!(ElevationPayload::from_json(&json), p);

        // Core T8 placeholder: {repo, path, reason} — defaults fill the rest.
        let legacy = ElevationPayload::from_json(
            r#"{"repo":"octo/x","path":"repos/octo/x/hooks","reason":"upstream demands admin rights"}"#,
        );
        assert_eq!(legacy.repo, "octo/x");
        assert_eq!(legacy.actions, Vec::new());
        assert_eq!(legacy.needed_role, "");
        assert_eq!(
            legacy.reason.as_deref(),
            Some("upstream demands admin rights")
        );
    }

    #[test]
    fn open_or_join_creates_one_flow_per_repo_and_raises_the_needed_role() {
        let s = DbState::open(":memory:").unwrap();
        let id1 = open_or_join_flow(&s, "acme/api", "/repos/acme/api/topics", Role::Maintain);
        let id2 = open_or_join_flow(&s, "acme/api", "/repos/acme/api", Role::Admin);
        assert_eq!(id1, id2, "the second gap joins the same repo's flow");
        let rows = s.flows_list(FLOW_KIND).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].flow_state, STATE_DETECTED);
        let p = ElevationPayload::from_json(&rows[0].payload_json);
        assert_eq!(p.needed_role, "admin", "raised to the max");
        assert_eq!(p.actions.len(), 2);

        // A different repo gets its own flow; a terminal flow is not joined.
        let id3 = open_or_join_flow(&s, "acme/other", "/repos/acme/other/topics", Role::Maintain);
        assert_ne!(id1, id3);
        s.flow_insert(&id1, FLOW_KIND, STATE_DONE, &p.to_json()).unwrap();
        let id4 = open_or_join_flow(&s, "acme/api", "/repos/acme/api/topics", Role::Maintain);
        assert_ne!(id1, id4, "DONE is terminal ⇒ a fresh flow");
        assert_eq!(s.flows_list(FLOW_KIND).unwrap().len(), 3);
    }

    #[test]
    fn joining_a_gap_does_not_stamp_updated_at() {
        let s = DbState::open(":memory:").unwrap();
        let id = open_or_join_flow(&s, "acme/api", "/repos/acme/api/topics", Role::Maintain);
        let before = s.flow_get(&id).unwrap().unwrap().updated_at;
        open_or_join_flow(&s, "acme/api", "/repos/acme/api", Role::Admin);
        let after = s.flow_get(&id).unwrap().unwrap().updated_at;
        assert_eq!(before, after, "joins are quiet patches");
    }

    #[test]
    fn is_active_state_covers_the_open_states_only() {
        for s in [STATE_DETECTED, STATE_AWAITING_USER_CONSENT, STATE_AWAITING_ROLE_CHANGE, STATE_VERIFYING] {
            assert!(is_active_state(s), "{s}");
        }
        for s in [STATE_DONE, STATE_CANCELLED, STATE_EXPIRED] {
            assert!(!is_active_state(s), "{s}");
        }
    }

    #[test]
    fn consent_text_names_why_needs_has_and_plain_words() {
        let text = consent_text(
            "rob-agent",
            "acme/api",
            Role::Maintain,
            Role::Read,
            &[
                ElevationAction {
                    path: "/repos/acme/api/topics".to_string(),
                    role: "maintain".to_string(),
                },
            ],
        );
        assert!(text.contains("acme/api"), "{text}");
        assert!(text.contains("/repos/acme/api/topics"), "{text}");
        assert!(text.contains("Needs: Maintain"), "{text}");
        assert!(text.contains("Has: Read"), "{text}");
        assert!(text.contains("topics and Pages"), "{text}");
        assert!(text.contains("rob-agent"), "{text}");
    }

    #[test]
    fn wrong_role_and_reminder_text_name_the_difference() {
        let w = wrong_role_text("rob-agent", "acme/api", Role::Triage, Role::Maintain);
        assert!(w.contains("Triage") && w.contains("Maintain"), "{w}");
        let r = reminder_text("rob-agent", "acme/api", Role::Maintain);
        assert!(r.contains("only reminder"), "{r}");
        let s = success_text("rob-agent", "acme/api", Role::Maintain, false, None);
        assert!(s.contains("The agent will retry its request"), "{s}");
        // Temporary grant: the seam fires AT DONE (task 9) — the copy says
        // the reduction starts now and must not promise a later trigger.
        let t = success_text("rob-agent", "acme/api", Role::Maintain, true, Some(Role::Read));
        assert!(t.contains("back to Read"), "{t}");
        assert!(t.contains("now"), "reduction starts at DONE: {t}");
        assert!(
            !t.contains("once you confirm"),
            "no promise of a later trigger: {t}"
        );
        let n = success_text("rob-agent", "acme/api", Role::Maintain, true, None);
        assert!(n.contains("now"), "{n}");
        assert!(!n.contains("once you confirm"), "{n}");
    }

    #[test]
    fn mark_work_done_records_success_only_on_this_repos_temporary_flows() {
        let s = DbState::open(":memory:").unwrap();
        let mut temp = payload("acme/api", "maintain", &[]);
        temp.temporary = Some(true);
        temp.intended_role = Some("read".to_string());
        let mut other = payload("acme/other", "write", &[]);
        other.temporary = Some(true);
        let mut permanent = payload("acme/api", "admin", &[]);
        permanent.temporary = Some(false);
        let mut cancelled = payload("acme/api", "maintain", &[]);
        cancelled.temporary = Some(true);
        s.flow_insert("temp-done", FLOW_KIND, STATE_DONE, &temp.to_json())
            .unwrap();
        s.flow_insert("temp-active", FLOW_KIND, STATE_VERIFYING, &temp.to_json())
            .unwrap();
        s.flow_insert("other-repo", FLOW_KIND, STATE_DONE, &other.to_json())
            .unwrap();
        s.flow_insert("permanent", FLOW_KIND, STATE_DONE, &permanent.to_json())
            .unwrap();
        s.flow_insert("cancelled", FLOW_KIND, STATE_CANCELLED, &cancelled.to_json())
            .unwrap();
        mark_work_done(&s, "acme/api");
        let payload_of = |id: &str| {
            ElevationPayload::from_json(&s.flow_get(id).unwrap().unwrap().payload_json)
        };
        assert_eq!(
            payload_of("temp-done").work_done,
            Some(1),
            "the DONE temp flow is marked"
        );
        assert_eq!(
            payload_of("temp-active").work_done,
            Some(1),
            "an active temp flow is marked too"
        );
        assert_eq!(
            payload_of("other-repo").work_done,
            None,
            "other repos are untouched"
        );
        assert_eq!(
            payload_of("permanent").work_done,
            None,
            "permanent grants carry no work_done"
        );
        assert_eq!(
            payload_of("cancelled").work_done,
            None,
            "cancelled flows are not marked"
        );
        // Idempotent: a second pass changes nothing.
        mark_work_done(&s, "acme/api");
        assert_eq!(payload_of("temp-done").work_done, Some(1));
    }

    #[test]
    fn nonce_lookup_resolves_only_elevation_flows_with_a_nonce() {
        let s = Arc::new(Mutex::new(DbState::open(":memory:").unwrap()));
        let lookup = nonce_lookup(s.clone());
        let mut p = payload("acme/api", "maintain", &[]);
        p.nonce = Some("abcd0123456789ef".to_string());
        {
            let guard = s.lock().unwrap();
            guard
                .flow_insert("e1", FLOW_KIND, STATE_AWAITING_USER_CONSENT, &p.to_json())
                .unwrap();
            guard
                .flow_insert("other", "approval", "PENDING", r#"{"nonce":"abcd0123456789ef"}"#)
                .unwrap();
        }
        assert_eq!(
            lookup("e1").as_deref(),
            Some("abcd0123456789ef"),
            "elevation flow resolves"
        );
        assert_eq!(lookup("other"), None, "other kinds are not ours");
        assert_eq!(lookup("absent"), None);
    }

    #[test]
    fn expired_sweep_writes_expired_and_leaves_terminal_flows_alone() {
        // Directly exercised through sweep_tick in the integration suite;
        // here pin the state classification the sweep relies on.
        assert!(is_active_state(STATE_DETECTED));
        assert!(!is_active_state(STATE_DONE));
    }
}
