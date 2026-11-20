//! Machine-token lifecycle over the control channel (GATE-004 §6.4–6.5, task 4):
//! `/rotate` (and the `[Start /rotate]` reminder button) run the intake
//! steps of the setup flow with the OLD token still live — a prefilled
//! token-page link plus a one-time form link; the form's success path
//! performs the atomic swap (see `crate::tasks`). `/disconnect` stops
//! forwarding immediately, erases custody, instructs the operator to
//! delete the GitHub-side token by hand (the gate has no API for that)
//! and lists a per-repo removal link for every registry row that still
//! holds access (GATE-010 §10, task 9; `/revoke owner/repo` runs the same
//! removal with verification).
//!
//! Rotate state machine (kept deliberately two-state):
//! 1. `/rotate` (or the reminder button) requires a stored token, mints a
//!    fresh form nonce (`onepage::mint_setup_nonce`), marks
//!    `rotate_in_progress = <nonce>` and sends the two links.
//! 2. The form verifies + stores on success (old token untouched on
//!    failure); the success path treats it as a ROTATION exactly when the
//!    stored marker equals the submitted nonce.
//! 3. `/disconnect` clears the marker (and any pending form nonce) along
//!    with the credential.
//!
//! This module owns no secrets: links carry nonces, never tokens.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::audit::AuditEntry;
use crate::custody::MachineTokenStore;
use crate::github::{TOKENS_LIST_URL, TOKEN_NEW_URL};
use crate::state::{self, State};
use crate::tasks::{LifecycleDeps, KV_ROTATE_IN_PROGRESS};
use crate::control::send::SendRequest;
use crate::control::{
    validate_callback, ACTION_NO_LONGER_VALID, NonceLookup, ControlApi, Update, UpdateHandler,
};

/// Flow id of the rotate intake (reminder button + its handler). Never a
/// valid pending-approval id, so the approval handler can safely ignore it.
pub const ROTATE_FLOW: &str = "rotate";

/// `/rotate` intake: guard on a stored token, mint the form nonce, mark
/// the rotation, return the two-link message. Sync — everything here is a
/// local kv write; the actual swap happens later in the form. `approver`
/// (the command sender / button presser, when known) lands in the audit
/// entry.
pub fn rotate_reply(deps: &LifecycleDeps, approver: Option<i64>) -> SendRequest {
    let has_token = {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        MachineTokenStore::new(&guard, &deps.enc_key).has_token()
    };
    if !has_token {
        return SendRequest::text(
            "No machine token is stored — nothing to rotate. Run /setup to connect the \
             machine account first.",
        );
    }
    let nonce = {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        crate::onepage::mint_setup_nonce(&guard)
    };
    let Ok(nonce) = nonce else {
        return SendRequest::text("rotate failed: could not mint the one-time nonce");
    };
    if deps
        .state
        .lock()
        .expect("state lock must be poison-free")
        .kv_set(KV_ROTATE_IN_PROGRESS, &nonce)
        .is_err()
    {
        return SendRequest::text("rotate failed: could not record the rotation state");
    }
    deps.audit.record(AuditEntry {
        ts: state::now(),
        agent: None,
        repo: "-".to_string(),
        action: "custody.rotate".to_string(),
        decision: "start".to_string(),
        basis: "old token stays live until the new one verifies".to_string(),
        approver: approver.map(|id| id.to_string()),
    });
    SendRequest::text(format!(
        "Rotate the machine token — the old one keeps working until the new one is stored.\n\
         1. Create a fresh classic PAT on the machine account \
         (scopes repo, workflow, read:org).\n\
         2. Paste it into the one-time form — the gate verifies it and swaps \
         it in atomically.\n\
         3. Delete the old token on GitHub once the swap is confirmed."
    ))
    .with_link_button("Open prefilled token page", TOKEN_NEW_URL)
    .with_link_button(
        "Open one-time form",
        format!("{}/setup/token?nonce={nonce}", deps.gate_host),
    )
}

/// `/disconnect`: forwarding off immediately, custody erased, expiry +
/// reminders + pending rotate state cleared, manual GitHub-side deletion
/// instructed, plus per-repo removal links (GATE-010 §10, task 9): every
/// registry row that still holds access gets its collaborator-settings
/// link so the operator can strip the machine account everywhere (the
/// gate has no API for repo memberships; `/revoke owner/repo` runs the
/// same removal with verification). The approver id (when known) lands in
/// the audit entry.
pub fn disconnect_reply(deps: &LifecycleDeps, approver: Option<i64>) -> SendRequest {
    deps.set_forwarding(false);
    let cleanup: Vec<String> = {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        let store = MachineTokenStore::new(&guard, &deps.enc_key);
        let _ = store.erase();
        for key in [
            crate::tasks::KV_TOKEN_EXPIRES_AT,
            crate::tasks::KV_ROTATE_IN_PROGRESS,
            crate::onepage::KV_NONCE,
            crate::onepage::KV_NONCE_EXPIRES,
        ] {
            let _ = guard.kv_delete(key);
        }
        for days in crate::tasks::REMINDER_DAYS {
            let _ = guard.kv_delete(&crate::tasks::reminder_key(days));
        }
        // Per-repo cleanup work list (GATE-010 §10): rows that still hold
        // access. A none-row is already removed — no link for it.
        guard
            .registry_all()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.granted_role != "none")
            .map(|r| r.repo)
            .collect()
    };
    deps.audit.record(AuditEntry {
        ts: state::now(),
        agent: None,
        repo: "-".to_string(),
        action: "custody.disconnect".to_string(),
        decision: "disconnected".to_string(),
        basis: "control command".to_string(),
        approver: approver.map(|id| id.to_string()),
    });
    let mut text = String::from(
        "Machine token disconnected. Forwarding is disabled immediately — agents now get \
         'machine credential unavailable'.\n\n\
         Delete the revoked token on GitHub yourself — the gate has no API to remove it \
         server-side (tokens page button below).\n\n",
    );
    if cleanup.is_empty() {
        text.push_str(
            "The registry lists no repos with access — there is nothing to clean up \
             per-repo.\n\nToken deletion on the API side was covered above.",
        );
    } else {
        text.push_str(&format!(
            "Per-repo cleanup: remove {login} from every repo it still has access to. \
             Open each repo's collaborator settings below, press the trash icon next to \
             {login} and confirm the removal — or run /revoke owner/repo here and I'll \
             verify each removal. Token deletion on the API side was covered above; \
             these links are for the repo memberships.",
            login = deps.machine_login
        ));
    }
    let mut req = SendRequest::text(text).with_link_button("Open tokens page", TOKENS_LIST_URL);
    for repo in &cleanup {
        if let Some((owner, name)) = repo.split_once('/') {
            req = req.with_link_button(
                repo.clone(),
                crate::github::collaborator_settings_url(owner, name),
            );
        }
    }
    req
}

/// The nonce resolver for the router's callback validation: the lifecycle
/// owns exactly one flow — `rotate`, keyed by the persistent
/// `rotate_cb_nonce` kv value. Everything else resolves to `None` and is
/// left to the approval lookup (or fails closed at the router).
pub fn nonce_lookup(state: Arc<Mutex<State>>) -> NonceLookup {
    Arc::new(move |flow| {
        if flow != ROTATE_FLOW {
            return None;
        }
        let guard = state.lock().expect("state lock must be poison-free");
        guard.kv_get(crate::tasks::KV_ROTATE_CB_NONCE).ok().flatten()
    })
}

/// The handler for the reminder's `[Start /rotate]` button. The router has
/// already nonce-validated the callback; the nonce is re-checked here so a
/// marker that vanished between validation and handling still fails
/// closed. A valid press runs the exact same intake as `/rotate`.
struct RotateStart {
    deps: Arc<LifecycleDeps>,
}

#[async_trait]
impl UpdateHandler for RotateStart {
    async fn handle(&self, update: Update, api: Arc<dyn ControlApi>) {
        let Some(cb) = update.callback else {
            return;
        };
        let Some(to) = update.message_ref else {
            return;
        };
        let expected = nonce_lookup_once(&self.deps);
        let Some(expected) = expected else {
            let _ = api.edit_message(to, SendRequest::text(ACTION_NO_LONGER_VALID)).await;
            return;
        };
        let Some((flow, action)) = validate_callback(&cb.data, &expected) else {
            let _ = api.edit_message(to, SendRequest::text(ACTION_NO_LONGER_VALID)).await;
            return;
        };
        match (flow.as_str(), action.as_str()) {
            (ROTATE_FLOW, "start") => {
                let reply = rotate_reply(&self.deps, Some(cb.from_id));
                let _ = api.edit_message(to, reply).await;
            }
            _ => {
                let _ = api.edit_message(to, SendRequest::text(ACTION_NO_LONGER_VALID)).await;
            }
        }
    }
}

fn nonce_lookup_once(deps: &LifecycleDeps) -> Option<String> {
    let guard = deps.state.lock().expect("state lock must be poison-free");
    guard
        .kv_get(crate::tasks::KV_ROTATE_CB_NONCE)
        .ok()
        .flatten()
}

/// Wire the lifecycle layer onto a router: the `[Start /rotate]` button
/// handler. (`rotate_reply`/`disconnect_reply` are invoked by the command
/// dispatcher in `commands`, keeping one reply per operator command.)
pub fn register_lifecycle(router: &mut crate::control::ControlRouter, deps: Arc<LifecycleDeps>) {
    router.register(Arc::new(RotateStart { deps }));
}
