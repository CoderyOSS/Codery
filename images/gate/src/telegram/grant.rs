//! Repo grant flow (GATE-010 §7.2, task 7): walk the operator through
//! giving the machine account access to one repo — instructions (org vs
//! personal), automatic invitation acceptance, role confirmation with
//! registry update — plus the grant-OFFER hook the API proxy fires when a
//! mutation lands on a repo the machine account cannot see.
//!
//! # The conversation ([`start_grant`])
//!
//! 1. **Instructions** — one message with ONE link button,
//!    `[Open collaborator settings]`. Org repos (`org_plan_note` visible)
//!    get role-choice wording naming the intended role (the account joins
//!    as an outside collaborator); personal repos get the fixed "every
//!    collaborator gets push access" wording (no role to pick). A visible
//!    paid plan adds the seat note ONCE per org (kv `seat_note:{org}`).
//! 2. **Poll invitations** (`repository_invitations`) every
//!    [`GrantDeps::poll`] (GATE-010: 5 s, `GATE_FLOW_POLL_SECS`); a
//!    matching invitation is accepted immediately (`accept_invitation`).
//!    One reminder after [`GrantDeps::reminder_after`] (1 h); the whole
//!    flow gives up at [`GrantDeps::give_up_after`] (7 d — GitHub
//!    invitations expire after 7 days) with a re-invite note.
//! 3. **Confirm the role** — poll `repo_permissions` until it reaches the
//!    intended tier (a HIGHER role is fine and recorded as-is); a LOWER
//!    role gets the exact fix message ("{login} was added with {Found},
//!    but it needs {Needed}") once per distinct wrong role, polling
//!    continues. On success the registry is reconciled via
//!    [`crate::registry::refresh`] and the grant metadata (intended role,
//!    `via = collaborator`) is stamped.
//!
//! The outcome is a [`GrantOutcome`] for the setup checklist (⏳ → ✅ / ❌
//! in place); `/grant` additionally reports the terminal outcome as one
//! message ([`outcome_text`]).
//!
//! # Grant-offer trigger (GATE-010)
//!
//! A mutation whose upstream answer is a repo-scoped 404 (indistinguishable
//! from "private, no access") fires [`maybe_offer_grant`] — one Telegram
//! message pointing at `/grant`, debounced once per repo per 24 h via kv
//! `grant_offer:{repo}:{date}` (epoch-day bucket). Reads never trigger
//! offers; a reachable public repo is third-party and routes through the
//! approval flow instead. The marker is written only after a confirmed
//! send, so an undelivered offer retries on the next trigger.
//!
//! # URL ruling
//!
//! The collaborator-settings link comes from
//! [`crate::github::collaborator_settings_url`] — every `github.com`
//! literal stays in the adapter (same posture as `SIGNUP_URL` & co); UI
//! copy in this layer carries the URL but never a literal.
//!
//! # Reduction (GATE-010 §10, task 9)
//!
//! Temporary grants reduce automatically: the elevation's
//! [`crate::telegram::elevation::ReductionFlow`] seam fires at DONE and
//! [`RealReductionFlow`] starts the reduction conversation —
//! `start_reduction(deps, repo, target)`:
//!
//! 1. **Instructions** — GATE-010 §10 wording ("Change {login} on {repo}
//!    back to {role}: click its role and choose {role}") + ONE
//!    `[Open collaborator settings]` link button. A target of `None` is a
//!    full REMOVAL: the instructions name the trash icon instead (there is
//!    no role to choose off a repo).
//! 2. **Poll-verify** — `repo_permissions` every [`ReductionDeps::poll`]
//!    (GATE-010: 5 s). Reaching the target ⇒ registry row updated
//!    (granted = target, `via` preserved) + ONE ✅ confirmation. The user
//!    instead REMOVING the agent (404/NoAccess, or an all-false
//!    permission answer) ⇒ treated as FULLY reduced: the row is
//!    none-marked per B6's keep-row ruling (`granted_role = none`,
//!    `intended_role`/`via` preserved so /status shows "access removed")
//!    with a "removed entirely" confirmation. A wrong-but-changed role
//!    gets one difference message per distinct role; polling continues.
//! 3. **Durability** — every reduction is a `flows` row
//!    (`kind='reduction'`, `REDUCING → DONE | EXPIRED`); the sweep
//!    ([`run_reduction_sweep`]) expires stale rows after
//!    [`ReductionDeps::expire_after`] (GATE-010: 24 h no-progress),
//!    reusing the elevation sweep's shape.
//!
//! # `/revoke {owner}/{repo}` (GATE-010 §10)
//!
//! One menu, two signed buttons: **[Lower to Write]** starts the reduction
//! conversation targeting Write; **[Remove completely]** targets `None`
//! (the trash-icon removal). Both verify against `repo_permissions` and
//! confirm here. The menu is itself a `flows` row (`kind='revoke'`,
//! `AWAITING_CHOICE → DECIDED`), nonce-validated like every signed
//! button; a second press never double-applies.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::audit::{Audit, AuditEntry};
use crate::github::{Platform, RepoAccessError};
use crate::registry::RegistryDeps;
use crate::rolemapping::{role_from_name, role_name};
use crate::state::{self, State as DbState};
use crate::telegram::send::SendRequest;
use crate::telegram::setup::{GrantFlow, GrantOutcome, NotifyHandle};
use crate::telegram::{nonce, NonceLookup, TelegramApi, TelegramRouter, Update, UpdateHandler};
use crate::types::Role;

/// Seconds per day — the offer debounce bucket.
const DAY_SECS: i64 = 86_400;

/// Everything the grant flow reads: shared state, the platform to verify
/// against, the machine login named in every message, and the three
/// durations (poll cadence, reminder delay, give-up window).
#[derive(Clone)]
pub struct GrantDeps {
    pub state: Arc<Mutex<DbState>>,
    pub platform: Arc<dyn Platform>,
    /// The machine account login — the invitee in every message.
    pub machine_login: String,
    /// Invitation/permission poll cadence (GATE-010: 5 s;
    /// `GATE_FLOW_POLL_SECS` shrinks it for tests).
    pub poll: Duration,
    /// One invitation reminder after this long (GATE-010: 1 h).
    pub reminder_after: Duration,
    /// Give up on the whole flow after this long (GATE-010: 7 d).
    pub give_up_after: Duration,
}

/// The real grant machinery, implementing setup.rs's [`GrantFlow`] seam
/// (GATE-011 step 8 consumes the trait object; `main.rs` wires this in).
pub struct RealGrantFlow {
    pub deps: GrantDeps,
}

impl RealGrantFlow {
    pub fn new(deps: GrantDeps) -> Self {
        Self { deps }
    }
}

#[async_trait]
impl GrantFlow for RealGrantFlow {
    async fn grant(&self, repo: &str, notify: &NotifyHandle) -> GrantOutcome {
        // Setup step 8 grants the Write tier (GATE-010: "usually write");
        // `/grant` names an explicit tier through `start_grant`.
        start_grant(&self.deps, repo, Role::Write, notify).await
    }
}

/// Run the full grant conversation for `repo` (GATE-010 §7.2). Blocks
/// until the role is confirmed, the window is exhausted, or `repo` is not
/// an `owner/repo` pair at all.
pub async fn start_grant(
    deps: &GrantDeps,
    repo: &str,
    intended_role: Role,
    notify: &NotifyHandle,
) -> GrantOutcome {
    let Some((owner, name)) = repo.split_once('/') else {
        return GrantOutcome::Failed {
            reason: format!("'{repo}' is not owner/repo"),
        };
    };
    let login = &deps.machine_login;
    let settings_url = crate::github::collaborator_settings_url(owner, name);
    let button = ("Open collaborator settings".to_string(), settings_url.clone());

    // 1. Instructions — org vs personal (GATE-010), then the seat note.
    let plan = deps.platform.org_plan_note(owner).await;
    let instructions = match &plan {
        Some(_) => SendRequest::text(format!(
            "Granting {repo} to {login}:\n\
             Open the collaborator settings (button below), press \"Add a collaborator\", \
             enter {login} and pick the {} role — org repos use role choices, and \
             {login} joins as an outside collaborator.\n\
             I'll accept the invitation automatically and confirm here.",
            intended_role
        )),
        None => SendRequest::text(format!(
            "Granting {repo} to {login}:\n\
             Open the collaborator settings (button below) and invite {login} as a \
             collaborator — on a personal repo every collaborator gets push access, \
             there is no role to pick.\n\
             I'll accept the invitation automatically and confirm here."
        )),
    };
    let _ = notify.send(instructions.with_link_button(button.0, button.1)).await;
    if let Some(plan_name) = &plan {
        maybe_seat_note(deps, owner, plan_name, notify).await;
    }

    let started = Instant::now();
    let give_up_note = |reason: String| async move {
        let _ = notify
            .send(SendRequest::text(format!(
                "Giving up on {repo}: {reason}"
            )))
            .await;
        GrantOutcome::Failed { reason }
    };

    // 2. Poll invitations; accept the matching one.
    let mut reminder_sent = false;
    loop {
        if started.elapsed() >= deps.give_up_after {
            let reason = format!(
                "no invitation for {login} arrived within 7 days — GitHub invitations \
                 expire after 7 days, so invite {login} again on the collaborator \
                 settings page and run /grant {repo}"
            );
            return give_up_note(reason).await;
        }
        match deps.platform.repository_invitations().await {
            Ok(list) => {
                if let Some(inv) = list
                    .iter()
                    .find(|i| i.repo_full_name.eq_ignore_ascii_case(repo))
                {
                    let id = inv.id;
                    if deps.platform.accept_invitation(id).await.is_ok() {
                        break; // invitation accepted; polling moves to the role check
                    }
                }
            }
            Err(_) => {} // listing hiccups: keep polling, never guess
        }
        if !reminder_sent && started.elapsed() >= deps.reminder_after {
            reminder_sent = true;
            let _ = notify
                .send(
                    SendRequest::text(format!(
                        "Still waiting: the {repo} collaborator invitation hasn't \
                         appeared. Open the collaborator settings and invite {login} — \
                         invitations expire after 7 days. I keep checking."
                    ))
                    .with_link_button("Open collaborator settings", settings_url.clone()),
                )
                .await;
        }
        tokio::time::sleep(deps.poll).await;
    }

    // 3. Poll permissions until the role reaches the intended tier.
    let mut fixed_roles: Vec<Role> = Vec::new();
    loop {
        if started.elapsed() >= deps.give_up_after {
            let reason = format!(
                "the {repo} invitation was accepted but the role never reached {} — \
                 check the collaborator settings and run /grant {repo} again",
                intended_role
            );
            return give_up_note(reason).await;
        }
        match deps.platform.repo_permissions(owner, name).await {
            Ok(access) if access.role >= intended_role => {
                confirm_registry(deps, repo, intended_role, notify).await;
                return GrantOutcome::Granted {
                    role: role_name(access.role).to_string(),
                };
            }
            Ok(access) if access.role != Role::None && !fixed_roles.contains(&access.role) => {
                fixed_roles.push(access.role);
                let _ = notify
                    .send(
                        SendRequest::text(format!(
                            "{login} was added with {}, but it needs {}. Open the \
                             collaborator settings, change {login}'s role to {} — \
                             I keep checking until it matches.",
                            access.role, intended_role, intended_role
                        ))
                        .with_link_button("Open collaborator settings", settings_url.clone()),
                    )
                    .await;
            }
            _ => {} // invisible (NoAccess), None, or a repeat role: keep polling
        }
        tokio::time::sleep(deps.poll).await;
    }
}

/// Reconcile the registry row after a confirmed grant: [`crate::registry::refresh`]
/// moves granted_role to what GitHub reports, then the grant's own
/// metadata (intended role + `via = collaborator`) is stamped — refresh's
/// first-sight path cannot know either.
async fn confirm_registry(
    deps: &GrantDeps,
    repo: &str,
    intended_role: Role,
    notify: &NotifyHandle,
) {
    let registry = RegistryDeps {
        state: deps.state.clone(),
        platform: deps.platform.clone(),
        machine_login: deps.machine_login.clone(),
        notify: Some((notify.api.clone(), notify.chat_id)),
    };
    crate::registry::refresh(&registry, repo).await;
    let granted = {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        guard
            .registry_get(repo)
            .ok()
            .flatten()
            .map(|r| r.granted_role)
            .unwrap_or_else(|| role_name(intended_role).to_string())
    };
    let _ = deps
        .state
        .lock()
        .expect("state lock must be poison-free")
        .registry_upsert(repo, &granted, role_name(intended_role), "collaborator");
}

/// The org seat note (GATE-010: "Mention paid-plan seat usage once per
/// org"): at most one message per org, ever (kv `seat_note:{org}`,
/// written only after a confirmed send).
async fn maybe_seat_note(deps: &GrantDeps, org: &str, plan: &str, notify: &NotifyHandle) {
    let key = format!("seat_note:{org}");
    let already = {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        matches!(guard.kv_get(&key), Ok(Some(_)))
    };
    if already {
        return;
    }
    let text = format!(
        "Note: {org} runs a paid GitHub plan ({plan}) — {} will take one of its \
         seats as an outside collaborator.",
        deps.machine_login
    );
    if notify.send(SendRequest::text(text)).await.is_ok() {
        let _ = deps
            .state
            .lock()
            .expect("state lock must be poison-free")
            .kv_set(&key, "1");
    }
}

/// kv key for the proxy's daily offer debounce: `grant_offer:{repo}:{date}`
/// (epoch-day bucket — one offer per repo per 24 h, GATE-010).
pub fn grant_offer_key(repo: &str, day: i64) -> String {
    format!("grant_offer:{repo}:{day}")
}

/// What the API proxy's grant-offer hook needs (GATE-010): the shared
/// state for the debounce marker, the Telegram channel for the offer, and
/// the machine login named in the message. `main.rs` fills this when the
/// bot is configured; otherwise the proxy stays offer-silent.
#[derive(Clone)]
pub struct GrantOfferDeps {
    pub state: Arc<Mutex<DbState>>,
    pub notify: Option<(Arc<dyn TelegramApi>, i64)>,
    pub machine_login: String,
}

/// The grant-offer trigger (GATE-010): a MUTATION just hit `repo` and the
/// machine account has no access to it. Offer the grant flow in Telegram —
/// once per repo per 24 h (kv marker written only after a confirmed send).
/// Returns `true` when an offer message went out.
pub async fn maybe_offer_grant(
    state: &Arc<Mutex<DbState>>,
    notify: &Option<(Arc<dyn TelegramApi>, i64)>,
    repo: &str,
    machine_login: &str,
) -> bool {
    let day = state::now().div_euclid(DAY_SECS);
    let key = grant_offer_key(repo, day);
    {
        let guard = state.lock().expect("state lock must be poison-free");
        if matches!(guard.kv_get(&key), Ok(Some(_))) {
            return false;
        }
    }
    let text = format!(
        "An agent action needed {repo}, but the machine account ({machine_login}) has \
         no access to it — GitHub answers 404 for the whole repo. Run /grant {repo} \
         in this chat and I'll walk you through the invitation: you invite, I accept \
         and verify automatically."
    );
    if crate::tasks::send_notice(notify, SendRequest::text(text))
        .await
        .is_ok()
    {
        let _ = state
            .lock()
            .expect("state lock must be poison-free")
            .kv_set(&key, "1");
        true
    } else {
        false
    }
}

/// One-line terminal outcome for command-triggered grants (setup renders
/// the checklist line instead; `/grant` has no checklist).
pub fn outcome_text(repo: &str, machine_login: &str, outcome: &GrantOutcome) -> String {
    match outcome {
        GrantOutcome::Granted { role } => format!(
            "✅ {repo}: {machine_login} now has {role} — access verified and \
             recorded in the registry."
        ),
        GrantOutcome::Pending { detail } => format!("⏳ {repo}: {detail}"),
        GrantOutcome::Failed { reason } => format!("❌ {repo}: {reason}"),
    }
}

// ---------------------------------------------------------------------------
// Reduction (GATE-010 §10, task 9): temporary-grant reductions AND the
// `/revoke` removals all run through this machinery.
// ---------------------------------------------------------------------------

/// `flows.kind` for reductions — temporary-grant reductions and the
/// removal polls `/revoke` starts.
pub const REDUCTION_FLOW_KIND: &str = "reduction";
/// `flows.kind` for the `/revoke` choice menu (the decision row).
pub const REVOKE_FLOW_KIND: &str = "revoke";

/// Open state of a reduction flow: instructions out, polling.
pub const REDUCTION_STATE_REDUCING: &str = "REDUCING";
/// Terminal: GitHub reported the target role (or the removal landed).
pub const REDUCTION_STATE_DONE: &str = "DONE";
/// The sweep's no-progress expiry.
pub const REDUCTION_STATE_EXPIRED: &str = "EXPIRED";

/// The `/revoke` menu waiting for its choice.
pub const REVOKE_STATE_AWAITING_CHOICE: &str = "AWAITING_CHOICE";
/// The menu was decided; the reduction flow carries on the work.
pub const REVOKE_STATE_DECIDED: &str = "DECIDED";
/// A menu nobody pressed for the whole expiry window.
pub const REVOKE_STATE_EXPIRED: &str = "EXPIRED";

const ACTION_LOWER: &str = "lower";
const ACTION_REMOVE: &str = "remove";

/// JSON payload of a reduction flow row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReductionPayload {
    /// The repo being reduced (`owner/repo`) — the flow key.
    #[serde(default)]
    pub repo: String,
    /// Registry spelling of the role to reduce TO (`none` = full removal).
    #[serde(default)]
    pub target_role: String,
}

impl ReductionPayload {
    fn to_json(&self) -> String {
        serde_json::to_string(self).expect("reduction payload must serialize")
    }

    fn from_json(raw: &str) -> Self {
        serde_json::from_str(raw).unwrap_or_default()
    }
}

/// JSON payload of a revoke-menu row: the signed buttons' anchor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokePayload {
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub nonce: Option<String>,
}

impl RevokePayload {
    fn to_json(&self) -> String {
        serde_json::to_string(self).expect("revoke payload must serialize")
    }

    fn from_json(raw: &str) -> Self {
        serde_json::from_str(raw).unwrap_or_default()
    }
}

/// Everything the reduction conversation, the `/revoke` menu and their
/// sweep read. Cloneable (arcs + strings); durations shrink in tests.
#[derive(Clone)]
pub struct ReductionDeps {
    pub state: Arc<Mutex<DbState>>,
    pub platform: Arc<dyn Platform>,
    /// The machine account login named in every message.
    pub machine_login: String,
    /// `(api, chat_id)` for gate-initiated sends; `None` ⇒ conversations
    /// cannot start (nobody to instruct) — rows expire with the sweep.
    pub notify: Option<(Arc<dyn TelegramApi>, i64)>,
    /// Verify-poll cadence (GATE-010: 5 s).
    pub poll: Duration,
    /// No-progress expiry for reduction rows and revoke menus
    /// (GATE-010: 24 h — the elevation sweep's value).
    pub expire_after: Duration,
    pub audit: Arc<Audit>,
}

/// The real reducer behind the elevation module's [`crate::telegram::
/// elevation::ReductionFlow`] seam (temporary grants call it at DONE) and
/// the machinery `/revoke` drives directly. `main.rs` wires this in.
pub struct RealReductionFlow {
    pub deps: ReductionDeps,
}

impl RealReductionFlow {
    pub fn new(deps: ReductionDeps) -> Self {
        Self { deps }
    }
}

#[async_trait]
impl crate::telegram::elevation::ReductionFlow for RealReductionFlow {
    async fn reduce(&self, repo: &str, to: Role) {
        // Fire-and-forget: DONE is durable and the agent is unblocked; the
        // reduction conversation polls in its own task.
        let deps = self.deps.clone();
        let repo = repo.to_string();
        tokio::spawn(async move {
            start_reduction(&deps, &repo, to).await;
        });
    }
}

/// Insert the durable reduction row (`kind='reduction'`, REDUCING)
/// WITHOUT starting the conversation. `start_reduction` = this +
/// [`run_reduction`]; the sweep test (and future restart-resume work) can
/// open rows directly.
pub fn open_reduction_row(deps: &ReductionDeps, repo: &str, to: Role) -> String {
    let id = state::uuid_v4();
    let payload = ReductionPayload {
        repo: repo.to_string(),
        target_role: role_name(to).to_string(),
    };
    let guard = deps.state.lock().expect("state lock must be poison-free");
    let _ = guard.flow_insert(
        &id,
        REDUCTION_FLOW_KIND,
        REDUCTION_STATE_REDUCING,
        &payload.to_json(),
    );
    id
}

/// Run the full reduction conversation for `repo` (GATE-010 §10): insert
/// the durable flow row, send the instructions, poll `repo_permissions`
/// until the target lands (or the agent was removed entirely) and confirm.
/// Returns the flow id once the conversation is terminal. Callers that
/// cannot afford to block (button handlers) spawn it — see
/// [`RealReductionFlow::reduce`].
pub async fn start_reduction(deps: &ReductionDeps, repo: &str, to: Role) -> String {
    let id = open_reduction_row(deps, repo, to);
    run_reduction(deps.clone(), id.clone()).await;
    id
}

/// The reduction conversation: instructions → poll-verify → registry
/// update + ✅. The row is the coordination point — the sweep may expire
/// it mid-conversation, which this loop respects (it bails when the row
/// leaves REDUCING).
async fn run_reduction(deps: ReductionDeps, flow_id: String) {
    let (repo, target) = {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        let row = match guard.flow_get(&flow_id) {
            Ok(Some(row)) if row.flow_state == REDUCTION_STATE_REDUCING => row,
            _ => return, // expired/cancelled between insert and start
        };
        let payload = ReductionPayload::from_json(&row.payload_json);
        (payload.repo, payload.target_role)
    };
    let Some(target) = role_from_name(&target) else {
        return; // unparsable target: leave the row for the sweep to expire
    };
    // Without a channel there is nobody to instruct; the row expires.
    if deps.notify.is_none() {
        return;
    }
    let Some((owner, name)) = repo.split_once('/') else {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        let _ = guard.flow_insert(
            &flow_id,
            REDUCTION_FLOW_KIND,
            REDUCTION_STATE_DONE,
            &ReductionPayload {
                repo,
                target_role: role_name(target).to_string(),
            }
            .to_json(),
        );
        return;
    };
    let settings_url = crate::github::collaborator_settings_url(owner, name);
    let instructions = if target == Role::None {
        SendRequest::text(removal_text(&deps.machine_login, &repo))
    } else {
        SendRequest::text(reduction_text(&deps.machine_login, &repo, target))
    };
    let _ = crate::tasks::send_notice(
        &deps.notify,
        instructions.with_link_button("Open collaborator settings", settings_url),
    )
    .await;

    // Wrong-but-changed roles get ONE difference message each (the grant
    // and elevation flows' pattern), then polling continues silently.
    let mut told: Vec<Role> = Vec::new();
    loop {
        tokio::time::sleep(deps.poll).await;
        let live = {
            let guard = deps.state.lock().expect("state lock must be poison-free");
            guard.flow_state(&flow_id).ok().flatten().as_deref()
                == Some(REDUCTION_STATE_REDUCING)
        };
        if !live {
            return; // the sweep expired the flow mid-conversation
        }
        match deps.platform.repo_permissions(owner, name).await {
            Ok(access) if access.role == target => {
                // A None target IS a removal (there is nothing lower):
                // route through finish_removed so the row keeps its
                // intended_role and the copy says "removed entirely".
                if target == Role::None {
                    finish_removed(&deps, &flow_id, &repo).await;
                } else {
                    finish_reduced(&deps, &flow_id, &repo, target).await;
                }
                return;
            }
            // Removed entirely: 404 (invisible ⇒ no access) or an all-false
            // permission answer. Both read as "the agent is off the repo".
            Err(RepoAccessError::NoAccess) => {
                finish_removed(&deps, &flow_id, &repo).await;
                return;
            }
            Ok(access) if access.role == Role::None => {
                finish_removed(&deps, &flow_id, &repo).await;
                return;
            }
            Ok(access) if !told.contains(&access.role) => {
                told.push(access.role);
                let _ = crate::tasks::send_notice(
                    &deps.notify,
                    SendRequest::text(wrong_target_text(
                        &deps.machine_login,
                        &repo,
                        access.role,
                        target,
                    ))
                    .with_link_button(
                        "Open collaborator settings",
                        crate::github::collaborator_settings_url(owner, name),
                    ),
                )
                .await;
            }
            _ => {} // unchanged, a repeat wrong role, or an upstream hiccup
        }
    }
}

/// Target reached: registry row moves to the target (granted = intended =
/// target, `via` preserved), flow DONE, ✅ confirmation, audit entry
/// (GATE-010: temporary reductions are recorded). The registry write is a
/// direct reconcile (NOT `registry::refresh`): the observed role is fresh
/// from the very poll that verified the reduction, and a refresh here
/// would fire the generic UNEXPECTED-drop notice right before this flow's
/// own ✅ — two messages for one intended event.
async fn finish_reduced(deps: &ReductionDeps, flow_id: &str, repo: &str, target: Role) {
    {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        let via = guard
            .registry_get(repo)
            .ok()
            .flatten()
            .map(|r| r.via)
            .unwrap_or_else(|| "reduction".to_string());
        let _ = guard.registry_upsert(repo, role_name(target), role_name(target), &via);
        if let Ok(Some(row)) = guard.flow_get(flow_id) {
            let _ = guard.flow_insert(
                flow_id,
                REDUCTION_FLOW_KIND,
                REDUCTION_STATE_DONE,
                &row.payload_json,
            );
        }
    }
    deps.audit.record(AuditEntry {
        ts: state::now(),
        agent: None,
        repo: repo.to_string(),
        action: "access.reduction".to_string(),
        decision: format!("reduced to {}", role_name(target)),
        basis: format!("reduction flow {flow_id}: verified via repo_permissions"),
        approver: None,
    });
    let _ = crate::tasks::send_notice(
        &deps.notify,
        SendRequest::text(format!(
            "✅ {repo}: {} is back to {target} — verified and recorded.",
            deps.machine_login
        )),
    )
    .await;
}

/// The agent was removed entirely (NoAccess / all-false permissions):
/// treated as FULLY reduced. Registry: B6's keep-row ruling — the row is
/// KEPT none-marked (`granted_role = none`, `intended_role`/`via`
/// preserved) so /status shows "access removed"; deletion was considered
/// and rejected there because it loses `intended_role`. Confirmed with the
/// "removed entirely" wording.
async fn finish_removed(deps: &ReductionDeps, flow_id: &str, repo: &str) {
    {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        if let Some(row) = guard.registry_get(repo).ok().flatten() {
            let _ = guard.registry_upsert(repo, "none", &row.intended_role, &row.via);
        } else {
            let _ = guard.registry_upsert(repo, "none", "none", "reduction");
        }
        if let Ok(Some(row)) = guard.flow_get(flow_id) {
            let _ = guard.flow_insert(
                flow_id,
                REDUCTION_FLOW_KIND,
                REDUCTION_STATE_DONE,
                &row.payload_json,
            );
        }
    }
    deps.audit.record(AuditEntry {
        ts: state::now(),
        agent: None,
        repo: repo.to_string(),
        action: "access.reduction".to_string(),
        decision: "removed entirely".to_string(),
        basis: format!("reduction flow {flow_id}: GitHub reports no access"),
        approver: None,
    });
    let _ = crate::tasks::send_notice(
        &deps.notify,
        SendRequest::text(format!(
            "✅ {repo}: {} was removed entirely — no access left; the registry \
             records it as removed.",
            deps.machine_login
        )),
    )
    .await;
}

// -- reduction sweep (the elevation sweep's shape, reduction rows) ------------

/// The reduction sweep: expires stale reduction rows and un-pressed
/// `/revoke` menus. Runs whether or not the bot is configured — expiry
/// must always make progress.
pub async fn run_reduction_sweep(
    deps: ReductionDeps,
    mut shutdown: watch::Receiver<bool>,
    interval: Duration,
) {
    shutdown.borrow_and_update();
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep(interval) => {
                reduction_sweep_tick(&deps).await;
            }
        }
    }
}

/// One sweep pass: expire every reduction row still REDUCING and every
/// revoke menu still AWAITING_CHOICE whose last progress is older than
/// `expire_after`. Terminal rows are never touched. Returns how many rows
/// were expired.
pub async fn reduction_sweep_tick(deps: &ReductionDeps) -> usize {
    let mut rows = Vec::new();
    for kind in [REDUCTION_FLOW_KIND, REVOKE_FLOW_KIND] {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        rows.extend(guard.flows_list(kind).unwrap_or_default());
    }
    let now = state::now();
    let cutoff = now.saturating_sub(deps.expire_after.as_secs() as i64);
    let mut expired = 0;
    for row in rows {
        let open = row.flow_state == REDUCTION_STATE_REDUCING
            || row.flow_state == REVOKE_STATE_AWAITING_CHOICE;
        if open && row.updated_at < cutoff {
            let guard = deps.state.lock().expect("state lock must be poison-free");
            let kind = if row.kind == REDUCTION_FLOW_KIND {
                REDUCTION_FLOW_KIND
            } else {
                REVOKE_FLOW_KIND
            };
            let expired_state = if kind == REDUCTION_FLOW_KIND {
                REDUCTION_STATE_EXPIRED
            } else {
                REVOKE_STATE_EXPIRED
            };
            if guard.flow_insert(&row.id, kind, expired_state, &row.payload_json).is_ok() {
                expired += 1;
            }
        }
    }
    expired
}

// -- /revoke: the lower-or-remove menu -----------------------------------------

/// Build the `/revoke {owner}/{repo}` menu (GATE-010 §10): one durable
/// `flows` row (kind='revoke') anchored to two signed buttons —
/// [Lower to Write] and [Remove completely]. Sync: everything here is a
/// local row + message build.
pub fn revoke_menu_reply(deps: &ReductionDeps, repo: &str) -> SendRequest {
    let id = state::uuid_v4();
    let nonce = nonce();
    let payload = RevokePayload {
        repo: repo.to_string(),
        nonce: Some(nonce.clone()),
    };
    {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        let _ = guard.flow_insert(
            &id,
            REVOKE_FLOW_KIND,
            REVOKE_STATE_AWAITING_CHOICE,
            &payload.to_json(),
        );
    }
    let login = &deps.machine_login;
    SendRequest::text(format!(
        "Revoke access on {repo} — pick one:\n\
         • [Lower to Write]: change {login}'s role back to Write — the usual agent \
          tier; pushes and pull requests keep working.\n\
         • [Remove completely]: remove {login} from {repo} entirely — the agent \
          loses all access.\n\
         I verify whichever you pick against GitHub and confirm here."
    ))
    .with_inline_button("Lower to Write", &id, &nonce, ACTION_LOWER)
    .with_inline_button("Remove completely", &id, &nonce, ACTION_REMOVE)
}

/// The nonce resolver for the router's callback validation: revoke menu id
/// → the payload nonce.
pub fn revoke_nonce_lookup(state: Arc<Mutex<DbState>>) -> NonceLookup {
    Arc::new(move |flow| {
        let guard = state.lock().expect("state lock must be poison-free");
        let row = guard.flow_get(flow).ok().flatten()?;
        if row.kind != REVOKE_FLOW_KIND {
            return None;
        }
        RevokePayload::from_json(&row.payload_json).nonce
    })
}

/// Register the `/revoke` button handler on a router.
pub fn register_revoke(router: &mut TelegramRouter, deps: ReductionDeps) {
    router.register(Arc::new(RevokeHandler { deps }));
}

/// The `/revoke` menu button handler: [Lower to Write] starts the
/// reduction conversation targeting Write; [Remove completely] targets
/// `None` (the trash-icon removal). Both verify and confirm through
/// [`run_reduction`]. A press on anything but an AWAITING_CHOICE menu is
/// ignored — never double-apply.
struct RevokeHandler {
    deps: ReductionDeps,
}

#[async_trait]
impl UpdateHandler for RevokeHandler {
    async fn handle(&self, update: Update, api: Arc<dyn TelegramApi>) {
        let Some(callback) = &update.callback else {
            return;
        };
        // Signed triple: `{flow_id}:{nonce}:{action}` — the router has
        // already validated the nonce; the action is the third part.
        let mut parts = callback.data.splitn(3, ':');
        let flow_id = parts.next().unwrap_or("").to_string();
        let _nonce = parts.next().unwrap_or("");
        let action = parts.next().unwrap_or("").to_string();
        let target = match action.as_str() {
            ACTION_LOWER => Role::Write,
            ACTION_REMOVE => Role::None,
            _ => return, // not a revoke button
        };
        let repo = {
            let guard = self.deps.state.lock().expect("state lock must be poison-free");
            match guard.flow_get(&flow_id).ok().flatten() {
                Some(row) if row.kind == REVOKE_FLOW_KIND => {
                    RevokePayload::from_json(&row.payload_json).repo
                }
                _ => return,
            }
        };
        // Decide-once: write DECIDED first; a racing second press reads a
        // decided row and returns without starting anything.
        {
            let guard = self.deps.state.lock().expect("state lock must be poison-free");
            let decided = match guard.flow_get(&flow_id) {
                Ok(Some(row)) if row.flow_state == REVOKE_STATE_AWAITING_CHOICE => {
                    guard
                        .flow_insert(&flow_id, REVOKE_FLOW_KIND, REVOKE_STATE_DECIDED, &row.payload_json)
                        .is_ok()
                }
                _ => false,
            };
            if !decided {
                return;
            }
        }
        let lower = target != Role::None;
        self.deps.audit.record(AuditEntry {
            ts: state::now(),
            agent: None,
            repo: repo.clone(),
            action: "access.revoke".to_string(),
            decision: if lower { "lower".to_string() } else { "remove".to_string() },
            basis: format!("revoke menu {flow_id}: {repo}"),
            approver: Some(callback.from_id.to_string()),
        });
        let verb = if lower { "Reducing" } else { "Removing" };
        if let Some(to) = update.message_ref {
            let _ = api
                .edit_message(
                    to,
                    SendRequest::text(format!(
                        "{verb} {repo} — the instructions are on the way; I'll \
                         verify against GitHub and confirm here."
                    )),
                )
                .await;
        }
        // The conversation must never run on the router's dispatch loop —
        // it polls until the operator acts on GitHub — so it spawns (same
        // posture as the /grant command handler).
        let deps = self.deps.clone();
        tokio::spawn(async move {
            start_reduction(&deps, &repo, target).await;
        });
    }
}

// -- reduction copy --------------------------------------------------------------

/// GATE-010 §10 wording for a role reduction (verbatim shape).
fn reduction_text(login: &str, repo: &str, target: Role) -> String {
    format!(
        "Change {login} on {repo} back to {target}: click its role and choose {target} \
         on the collaborator settings page (button below). I check every few seconds \
         and confirm here when it lands."
    )
}

/// Full-removal instructions: there is no role to choose OFF a repo, so the
/// copy names the trash icon instead.
fn removal_text(login: &str, repo: &str) -> String {
    format!(
        "Removing {login} from {repo} entirely: open the collaborator settings \
         (button below), press the 🗑 trash icon next to {login} and confirm the \
         removal. I check every few seconds and confirm here when it lands."
    )
}

/// Wrong-but-changed role during a reduction: name found vs target.
fn wrong_target_text(login: &str, repo: &str, found: Role, target: Role) -> String {
    format!(
        "{login} now has {found} on {repo}, but it should go back to {target} — open \
         the collaborator settings and change {login}'s role to {target}. I keep \
         checking."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_offer_key_is_repo_plus_epoch_day() {
        assert_eq!(grant_offer_key("acme/api", 20_000), "grant_offer:acme/api:20000");
    }

    #[test]
    fn outcome_text_renders_each_terminal_shape() {
        let g = outcome_text(
            "acme/api",
            "rob-agent",
            &GrantOutcome::Granted {
                role: "write".to_string(),
            },
        );
        assert!(g.contains("✅ acme/api") && g.contains("rob-agent") && g.contains("write"), "{g}");
        let f = outcome_text(
            "acme/api",
            "rob-agent",
            &GrantOutcome::Failed {
                reason: "no invitation".to_string(),
            },
        );
        assert!(f.contains("❌ acme/api") && f.contains("no invitation"), "{f}");
        let p = outcome_text(
            "acme/api",
            "rob-agent",
            &GrantOutcome::Pending {
                detail: "in flight".to_string(),
            },
        );
        assert!(p.contains("⏳ acme/api") && p.contains("in flight"), "{p}");
    }
}
