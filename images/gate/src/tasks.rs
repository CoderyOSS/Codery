//! Machine-token lifecycle tasks (GATE-004 §6.3–6.5, task 4): the state
//! machine that decides whether the gate may forward at all.
//!
//! - **Startup check** ([`startup_check`]): runs BEFORE the server binds.
//!   Token present ⇒ `verify_token`; pass ⇒ forwarding on; fail ⇒ off +
//!   Telegram notice naming the failed check; absent ⇒ off + "run /setup"
//!   prompt; custody-unreadable or transport-dead ⇒ off + notice (never a
//!   crash).
//! - **Daily verify** ([`daily_tick`], driven by [`run_daily_loop`] at a
//!   24 h interval): re-verifies, stamps `token_last_verified_at` (kv,
//!   surfaced on /status later), then checks expiry reminders.
//! - **Expiry reminders**: 14/3/1 days before `token_expires_at`, one
//!   reminder per tick (tightest eligible threshold); the message carries
//!   a signed `[Start /rotate]` button. `reminder_sent:{days}` is written
//!   ONLY after a confirmed send (`send_notice` returns `Ok`) — a failed
//!   delivery or a missing channel leaves the reminder unmarked, so the
//!   next daily sweep retries it instead of it being lost, and restarts
//!   never re-send a reminder that WAS delivered.
//! - **Post-store completion** ([`apply_stored_token`] +
//!   [`notify_rotate_complete`]): called from the one-time form's success
//!   path. Persists the fresh expiry, resets the reminder cycle, and —
//!   when `rotate_in_progress` matches the submitted form nonce — clears
//!   the marker and notifies with `[Open tokens page]` for deleting the
//!   old token. The rotate state machine is deliberately two-state:
//!   `rotate_in_progress` is set by `/rotate` and consumed (only on an
//!   exact nonce match) by the form; `/disconnect` clears it.
//!
//! kv keys owned here: `token_expires_at`, `token_last_verified_at`,
//! `reminder_sent:{14,3,1}`, `rotate_in_progress`, `rotate_cb_nonce`.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;

use crate::audit::Audit;
use crate::custody::MachineTokenStore;
use crate::github::{Platform, VerifiedToken, VerifyError};
use crate::state::{self, State as DbState};
use crate::telegram::send::{MsgRef, SendRequest};
use crate::telegram::TelegramApi;

/// 24 h: the daily verify + reminder cadence.
pub const DAILY_INTERVAL: Duration = Duration::from_secs(86_400);

/// kv: unix-seconds expiry of the machine token (from the verified
/// `github-authentication-token-expiration` header).
pub const KV_TOKEN_EXPIRES_AT: &str = "token_expires_at";
/// kv: unix-seconds timestamp of the last successful verification.
pub const KV_LAST_VERIFIED_AT: &str = "token_last_verified_at";
/// kv: `rotate_in_progress = <form nonce>` — set by `/rotate`, consumed by
/// the form's success path when it matches the submitted nonce exactly.
pub const KV_ROTATE_IN_PROGRESS: &str = "rotate_in_progress";
/// kv: the signed-callback nonce backing every `[Start /rotate]` button
/// (persists so old reminder buttons keep working across restarts).
pub const KV_ROTATE_CB_NONCE: &str = "rotate_cb_nonce";

/// Reminder thresholds, days before expiry (GATE-004 §6.5).
pub const REMINDER_DAYS: [i64; 3] = [14, 3, 1];

/// kv key for a sent reminder: `reminder_sent:{days}`.
pub fn reminder_key(days: i64) -> String {
    format!("reminder_sent:{days}")
}

/// Everything the lifecycle reads and flips. Cheap to clone (arcs and
/// strings); shared by the startup/daily tasks, the Telegram commands and
/// the one-time form.
#[derive(Clone)]
pub struct LifecycleDeps {
    pub state: Arc<Mutex<DbState>>,
    pub platform: Arc<dyn Platform>,
    /// The machine account login every verification expects.
    pub machine_login: String,
    /// `GATE_TOKEN_ENC_KEY` — custody derives its AES key from it.
    pub enc_key: String,
    /// The gate-wide forwarding switch (shared with `GateState`).
    pub forwarding: Arc<std::sync::atomic::AtomicBool>,
    /// `(api, chat_id)` for gate-initiated notices; `None` = no bot or no
    /// chat configured — checks still run, notices are skipped.
    pub notify: Option<(Arc<dyn TelegramApi>, i64)>,
    /// Gate base URL for the one-time form link in rotate intake.
    pub gate_host: String,
    pub audit: Arc<Audit>,
}

impl LifecycleDeps {
    fn load_token(&self) -> anyhow::Result<Option<String>> {
        let guard = self.state.lock().expect("state lock must be poison-free");
        MachineTokenStore::new(&guard, &self.enc_key).load()
    }

    pub(crate) fn set_forwarding(&self, on: bool) {
        self.forwarding.store(on, Ordering::Relaxed);
    }
}

/// Human text for a failed verification, naming the failed check (GATE-004
/// report order: wrong account → 2FA → scopes → upstream).
pub fn check_failed_text(err: &VerifyError, expected_login: &str) -> String {
    match err {
        VerifyError::WrongLogin(found) => format!(
            "wrong account — the token belongs to '{found}', not the machine account '{expected_login}'"
        ),
        VerifyError::TwoFactorOff => {
            format!("two-factor authentication is off on machine account '{expected_login}'")
        }
        VerifyError::MissingScope(scope) => {
            format!("token is missing the '{scope}' scope (needs repo, workflow, read:org)")
        }
        VerifyError::Upstream(0) => {
            "GitHub is unreachable (transport error)".to_string()
        }
        VerifyError::Upstream(code) => format!("GitHub answered HTTP {code}"),
    }
}

/// Gate-initiated notice: a fresh send into the operator chat. Reports the
/// outcome: `Err` when no channel is configured or the transport failed
/// (both logged to stderr). Fire-and-forget callers (startup/daily
/// notices) may ignore the result — checks never crash on notification
/// problems — but CALLERS THAT MARK DELIVERY IN KV (the expiry-reminder
/// sweep) must write their marker only on `Ok`, so an undelivered notice
/// stays eligible and the next sweep retries it. Shared with the registry
/// sweep's role-drop notices (`registry.rs`).
pub(crate) async fn send_notice(
    notify: &Option<(Arc<dyn TelegramApi>, i64)>,
    req: SendRequest,
) -> anyhow::Result<()> {
    let Some((api, chat)) = notify else {
        eprintln!("lifecycle: no control channel configured — notice skipped");
        anyhow::bail!("no control channel configured");
    };
    let anchored = req.replying_to(MsgRef {
        chat_id: *chat,
        message_id: 0,
    });
    api.send_message(anchored)
        .await
        .map(|_| ())
        .map_err(|e| {
            eprintln!("lifecycle: notice delivery failed: {e}");
            e
        })
}

/// Persist a successful verification: the last-verified stamp and the
/// token's expiry (`None`/unparsable expiry ⇒ no kv row ⇒ no reminders).
/// Does NOT touch reminder markers — reminders survive restarts and daily
/// re-verifications of the SAME token.
fn stamp_verified(guard: &DbState, vt: &VerifiedToken) -> anyhow::Result<()> {
    guard.kv_set(KV_LAST_VERIFIED_AT, &state::now().to_string())?;
    match vt.expires.as_deref().and_then(parse_expires) {
        Some(ts) => guard.kv_set(KV_TOKEN_EXPIRES_AT, &ts.to_string())?,
        None => {
            guard.kv_delete(KV_TOKEN_EXPIRES_AT)?;
        }
    }
    Ok(())
}

/// Post-STORE completion (one-time form success path, both first setup and
/// rotate): stamp verification, then reset the reminder cycle — a fresh
/// token must not inherit the old token's sent reminders.
pub fn apply_stored_token(guard: &DbState, vt: &VerifiedToken) -> anyhow::Result<()> {
    stamp_verified(guard, vt)?;
    for days in REMINDER_DAYS {
        guard.kv_delete(&reminder_key(days))?;
    }
    Ok(())
}

/// Rotate-complete notice: `[Open tokens page]` + the explicit
/// delete-the-old-token instruction (the gate cannot delete GitHub-side
/// tokens). Silence when no notify channel is configured.
pub async fn notify_rotate_complete(
    notify: &Option<(Arc<dyn TelegramApi>, i64)>,
    vt: &VerifiedToken,
) {
    if notify.is_none() {
        return;
    }
    let expiry = vt.expires.as_deref().unwrap_or("no expiry");
    let text = format!(
        "Machine token ROTATED — the new credential is verified (login {}, expires {expiry}). \
         Forwarding continued uninterrupted.\n\n\
         Now open the tokens page and DELETE the old token, so it cannot be reused.",
        vt.login
    );
    let _ = send_notice(
        notify,
        SendRequest::text(text).with_link_button("Open tokens page", crate::github::TOKENS_LIST_URL),
    )
    .await;
}

/// The startup credential check (GATE-004 §6.3). Runs BEFORE the server
/// binds — the gate never serves a request while its credential state is
/// unknown. Every failure path lands on forwarding-off + a Telegram notice
/// naming what failed; only an explicit verify pass turns forwarding on.
pub async fn startup_check(deps: &LifecycleDeps) {
    match deps.load_token() {
        Err(e) => {
            deps.set_forwarding(false);
            let _ = send_notice(
                &deps.notify,
                SendRequest::text(format!(
                    "Machine credential could not be read ({e}) — forwarding DISABLED. \
                     Check GATE_TOKEN_ENC_KEY."
                )),
            )
            .await;
        }
        Ok(None) => {
            deps.set_forwarding(false);
            let _ = send_notice(
                &deps.notify,
                SendRequest::text(
                    "No machine token stored — forwarding is DISABLED. \
                     Run /setup to connect the machine account.",
                ),
            )
            .await;
        }
        Ok(Some(token)) => match deps.platform.verify_token(&token, &deps.machine_login).await {
            Ok(vt) => {
                deps.set_forwarding(true);
                let guard = deps.state.lock().expect("state lock must be poison-free");
                let _ = stamp_verified(&guard, &vt);
            }
            Err(e) => {
                deps.set_forwarding(false);
                let _ = send_notice(
                    &deps.notify,
                    SendRequest::text(format!(
                        "Machine credential FAILED verification: {} — forwarding DISABLED. \
                         Run /rotate to replace the token.",
                        check_failed_text(&e, &deps.machine_login)
                    )),
                )
                .await;
            }
        },
    }
}

/// One daily tick: re-verify the stored credential, then check expiry
/// reminders. No token ⇒ off and quiet (the startup notice already
/// prompted; daily repetition would be spam).
pub async fn daily_tick(deps: &LifecycleDeps) {
    match deps.load_token() {
        Err(e) => {
            deps.set_forwarding(false);
            let _ = send_notice(
                &deps.notify,
                SendRequest::text(format!(
                    "Machine credential could not be read ({e}) — forwarding DISABLED. \
                     Check GATE_TOKEN_ENC_KEY."
                )),
            )
            .await;
        }
        Ok(None) => deps.set_forwarding(false),
        Ok(Some(token)) => match deps.platform.verify_token(&token, &deps.machine_login).await {
            Ok(vt) => {
                deps.set_forwarding(true);
                let guard = deps.state.lock().expect("state lock must be poison-free");
                let _ = stamp_verified(&guard, &vt);
            }
            Err(e) => {
                deps.set_forwarding(false);
                let _ = send_notice(
                    &deps.notify,
                    SendRequest::text(format!(
                        "Machine credential FAILED daily verification: {} — forwarding DISABLED. \
                         Run /rotate to replace the token.",
                        check_failed_text(&e, &deps.machine_login)
                    )),
                )
                .await;
            }
        },
    }
    maybe_send_reminders(deps).await;
}

/// Daily reminder sweep: the tightest eligible unsent threshold fires, at
/// most one message per tick. The marker is written only after a
/// successful send, so an undelivered reminder retries the next day
/// rather than being lost; with no notify channel nothing is marked.
async fn maybe_send_reminders(deps: &LifecycleDeps) {
    let expires_at = {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        match guard.kv_get(KV_TOKEN_EXPIRES_AT) {
            Ok(v) => v.and_then(|s| s.parse::<i64>().ok()),
            Err(_) => None,
        }
    };
    let Some(expires_at) = expires_at else {
        return; // no known expiry (non-expiring token) ⇒ nothing to remind
    };
    // Floor division: anything already past expiry lands strictly negative
    // (−3600 s ⇒ −1 day ⇒ the "EXPIRED" wording), not zero.
    let days_left = (expires_at - state::now()).div_euclid(DAY_SECS);
    // Tightest eligible threshold fires: sweep from 1 up to 14, first
    // `days_left <= d` wins (13 days out ⇒ the 14-day reminder; 2 days out
    // ⇒ the 3-day reminder; expired ⇒ the 1-day slot, worded as expired).
    let Some(days) = REMINDER_DAYS.iter().rev().copied().find(|d| days_left <= *d) else {
        return; // more than 14 days out
    };

    let already_sent = {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        matches!(guard.kv_get(&reminder_key(days)), Ok(Some(_)))
    };
    if already_sent {
        return;
    }

    let body = if days_left < 0 {
        format!(
            "Heads-up: the machine token has EXPIRED — forwarding will fail as upstream \
             rejects it. Run /rotate to replace it now."
        )
    } else if days_left == 0 {
        format!(
            "Heads-up: the machine token expires today. Run /rotate to replace it before \
             it lapses."
        )
    } else {
        format!(
            "Heads-up: the machine token expires in {days_left} day(s). Run /rotate to \
             replace it before it lapses."
        )
    };
    let cb_nonce = rotate_cb_nonce(deps);
    let req = match &cb_nonce {
        Some(n) => SendRequest::text(body)
            .with_inline_button("Start /rotate", crate::telegram::lifecycle::ROTATE_FLOW, n, "start"),
        None => SendRequest::text(body),
    };
    // The marker is written ONLY on a confirmed send: a failed delivery
    // (or a missing channel) leaves the reminder unmarked, so the next
    // daily sweep retries it instead of it being lost forever.
    if send_notice(&deps.notify, req).await.is_ok() {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        let _ = guard.kv_set(&reminder_key(days), "1");
    }
}

/// Seconds per day — expiry arithmetic granularity.
const DAY_SECS: i64 = 86_400;

/// The persistent signed-callback nonce backing `[Start /rotate]` buttons;
/// minted once, reused so reminder buttons survive restarts.
fn rotate_cb_nonce(deps: &LifecycleDeps) -> Option<String> {
    let guard = deps.state.lock().expect("state lock must be poison-free");
    match guard.kv_get(KV_ROTATE_CB_NONCE) {
        Ok(Some(n)) => Some(n),
        Ok(None) => {
            let n = crate::telegram::nonce();
            guard.kv_set(KV_ROTATE_CB_NONCE, &n).ok()?;
            Some(n)
        }
        Err(_) => None,
    }
}

/// The 24 h verify + reminder loop. First tick is one full interval after
/// startup (the startup check already covered t=0). Exits on shutdown.
pub async fn run_daily_loop(
    deps: LifecycleDeps,
    mut shutdown: watch::Receiver<bool>,
    interval: Duration,
) {
    shutdown.borrow_and_update();
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep(interval) => daily_tick(&deps).await,
        }
    }
}

// ---------------------------------------------------------------------------
// Expiry header ⇄ unix seconds (no chrono dependency; the two shapes
// GitHub actually sends, plus explicit offsets)
// ---------------------------------------------------------------------------

/// Parse the `github-authentication-token-expiration` value to unix
/// seconds. Accepts `YYYY-MM-DD[ T]HH:MM:SS` with `Z`, `±HH:MM`/`±HHMM`
/// offsets, or the trailing ` UTC` GitHub also emits. Fractional seconds
/// are ignored. `None` on anything unparsable (⇒ no reminders).
pub fn parse_expires(raw: &str) -> Option<i64> {
    let s = raw.trim();
    let s = s.strip_suffix(" UTC").unwrap_or(s);
    let (date, rest) = match s.split_once('T') {
        Some((d, r)) => (d, r),
        None => match s.split_once(' ') {
            Some((d, r)) => (d, r),
            None => (s, ""),
        },
    };
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // Time-of-day: strip offset/fraction first, then H:M:S.
    let mut time = rest;
    let mut offset_secs: i64 = 0;
    if let Some(stripped) = time.strip_suffix('Z') {
        time = stripped;
    } else if let Some(idx) = time.find(['+', '-']) {
        let (t, off_with_sign) = time.split_at(idx);
        time = t;
        let sign: i64 = if off_with_sign.starts_with('-') { -1 } else { 1 };
        let off = &off_with_sign[1..];
        let (oh, om) = match off.split_once(':') {
            Some((h, m)) => (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?),
            None if off.len() == 4 => (off[..2].parse().ok()?, off[2..].parse().ok()?),
            None => return None,
        };
        offset_secs = sign * (oh * 3600 + om * 60);
    }
    let time = time.split('.').next().unwrap_or(time);
    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next().filter(|p| !p.is_empty()).map(|p| p.parse().ok()).unwrap_or(Some(0))?;
    let minute: i64 = time_parts.next().filter(|p| !p.is_empty()).map(|p| p.parse().ok()).unwrap_or(Some(0))?;
    let second: i64 = time_parts.next().filter(|p| !p.is_empty()).map(|p| p.parse().ok()).unwrap_or(Some(0))?;
    if time_parts.next().is_some()
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }
    Some(
        days_from_civil(year, month, day) * DAY_SECS
            + hour * 3600
            + minute * 60
            + second
            - offset_secs,
    )
}

/// Render unix seconds as `YYYY-MM-DDTHH:MM:SSZ` (inverse of
/// [`parse_expires`] for UTC; used by tests to drive reminders).
pub fn format_expires(ts: i64) -> String {
    let days = ts.div_euclid(DAY_SECS);
    let secs = ts.rem_euclid(DAY_SECS);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Days since 1970-01-01 ⇒ civil date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// Civil date ⇒ days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_format_roundtrip_across_shapes() {
        assert_eq!(parse_expires("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_expires("2027-05-01T00:00:00Z"), Some(1_809_129_600));
        assert_eq!(
            parse_expires("2026-06-13 20:21:32 UTC"),
            Some(1_781_382_092),
            "GitHub's space-separated UTC shape"
        );
        assert_eq!(
            parse_expires("2026-06-13T22:21:32+02:00"),
            Some(1_781_382_092),
            "offsets normalise to UTC"
        );
        assert_eq!(
            parse_expires("2026-06-13T18:21:32-02:00"),
            Some(1_781_382_092),
            "negative offsets normalise to UTC"
        );
        assert_eq!(
            parse_expires("2026-06-13T18:21:32-0200"),
            Some(1_781_382_092),
            "compact offsets accepted"
        );
        assert_eq!(
            parse_expires("2026-06-13T20:21:32.500Z"),
            Some(1_781_382_092),
            "fractional seconds ignored"
        );
        assert_eq!(parse_expires("2026-13-01T00:00:00Z"), None, "month 13");
        assert_eq!(parse_expires("nope"), None);
        assert_eq!(parse_expires(""), None);
        for ts in [0, 1_781_382_092, 1_809_129_600, 4_102_444_800] {
            assert_eq!(parse_expires(&format_expires(ts)), Some(ts), "roundtrip {ts}");
        }
    }

    #[test]
    fn reminder_keys_and_thresholds_are_plan_pinned() {
        assert_eq!(reminder_key(14), "reminder_sent:14");
        assert_eq!(REMINDER_DAYS, [14, 3, 1]);
        assert_eq!(DAILY_INTERVAL, Duration::from_secs(86_400));
    }

    #[test]
    fn check_failed_text_names_each_check() {
        assert_eq!(
            check_failed_text(&VerifyError::WrongLogin("robsmith".into()), "gate-machine"),
            "wrong account — the token belongs to 'robsmith', not the machine account 'gate-machine'"
        );
        assert!(check_failed_text(&VerifyError::TwoFactorOff, "gate-machine").contains("two-factor"));
        assert_eq!(
            check_failed_text(&VerifyError::MissingScope("workflow".into()), "gate-machine"),
            "token is missing the 'workflow' scope (needs repo, workflow, read:org)"
        );
        assert_eq!(
            check_failed_text(&VerifyError::Upstream(0), "gate-machine"),
            "GitHub is unreachable (transport error)"
        );
        assert_eq!(
            check_failed_text(&VerifyError::Upstream(401), "gate-machine"),
            "GitHub answered HTTP 401"
        );
    }
}
