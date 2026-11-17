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

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::github::Platform;
use crate::registry::RegistryDeps;
use crate::rolemapping::role_name;
use crate::state::{self, State as DbState};
use crate::telegram::send::SendRequest;
use crate::telegram::setup::{GrantFlow, GrantOutcome, NotifyHandle};
use crate::telegram::TelegramApi;
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
