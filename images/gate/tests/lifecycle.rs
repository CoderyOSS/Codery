//! Integration: machine-token lifecycle (GATE-004 §6.3–6.5, task 4).
//! Startup check (token present ⇒ verify; pass ⇒ forwarding on, fail ⇒ off
//! + Control notice naming the failed check; absent ⇒ off + /setup
//! prompt), daily verify task (24 h), expiry reminders at 14/3/1 days with
//! a `[Start /rotate]` button, `/rotate` (prefilled token page + one-time
//! form, atomic swap on success — old token keeps working on failure) and
//! `/disconnect` (forwarding off immediately, custody erased, manual
//! deletion instructed). The forwarding flag is consulted by both proxies
//! BEFORE custody/upstream: flag off ⇒ credential_unavailable with no
//! probe at all.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use gate::approval::{ApprovalOutcome, StubBackend};
use gate::audit::Audit;
use gate::custody::MachineTokenStore;
use gate::github::{GitHubPlatform, Platform};
use gate::gitproxy::GitDeps;
use gate::apiproxy::ApiDeps;
use gate::onepage::{mint_setup_nonce, OnePageDeps, KV_NONCE};
use gate::policy::{PolicyEngine, PolicyFile};
use gate::routes::{router, GateState, RateLimiter};
use gate::state::{self, State as DbState};
use gate::tasks::{
    daily_tick, parse_expires, startup_check, KV_LAST_VERIFIED_AT, KV_ROTATE_CB_NONCE,
    KV_ROTATE_IN_PROGRESS, KV_TOKEN_EXPIRES_AT,
};
use gate::control::commands::{register_commands, CommandDeps};
use gate::control::fake::FakeTransport;
use gate::control::lifecycle::{
    nonce_lookup, register_lifecycle, rotate_reply, disconnect_reply, ROTATE_FLOW,
};
use gate::control::{MsgRef, ControlApi, ControlRouter, Update};
use gate::tokens::TokenStore;

const MACHINE_LOGIN: &str = "gate-machine";
const OLD_TOKEN: &str = "ghp_oldEXAMPLEtoken0000000001";
const NEW_TOKEN: &str = "ghp_newEXAMPLEtoken0000000002";
const ENC_KEY: &str = "lifecycle-test-enc-key";
const CHAT: i64 = 4242;
const HOST: &str = "https://gag.example.com";
const AGENT: &str = "claude-code";

const DAY: i64 = 86_400;

// ---------------------------------------------------------------------------
// Shared GitHub-mock helpers
// ---------------------------------------------------------------------------

/// Mock `GET /user` on the wiremock upstream. When `require_bearer` is
/// given, only a request carrying exactly that Authorization header
/// matches (a wrong token falls through to wiremock's default 404).
async fn mount_user(
    server: &MockServer,
    login: &str,
    two_fa: bool,
    scopes: &str,
    expires: Option<&str>,
    require_bearer: Option<&str>,
) {
    let body = format!(
        r#"{{"login":"{login}","id":4242,"two_factor_authentication":{two_fa}}}"#
    );
    let mut tpl = ResponseTemplate::new(200)
        .set_body_string(body)
        .append_header("X-OAuth-Scopes", scopes);
    if let Some(exp) = expires {
        tpl = tpl.append_header("github-authentication-token-expiration", exp);
    }
    let mut mock = Mock::given(method("GET")).and(path("/user"));
    if let Some(bearer) = require_bearer {
        mock = mock.and(header("authorization", format!("Bearer {bearer}")));
    }
    mock.respond_with(tpl).mount(server).await;
}

async fn mount_good_user(server: &MockServer, token: &str, expires: Option<&str>) {
    mount_user(
        server,
        MACHINE_LOGIN,
        true,
        "repo, workflow, read:org",
        expires,
        Some(token),
    )
    .await;
}

fn custody_of<'a>(state: &'a DbState, enc_key: &'a str) -> MachineTokenStore<'a> {
    MachineTokenStore::new(state, enc_key)
}

// ---------------------------------------------------------------------------
// Fixture A — control plane: state + FakeTransport + router with the
// command handlers + lifecycle handler registered, direct dispatch (no
// poll loop), wiremock upstream for verify.
// ---------------------------------------------------------------------------

struct Ctl {
    dir: PathBuf,
    fake: Arc<FakeTransport>,
    api: Arc<dyn ControlApi>,
    state: Arc<Mutex<DbState>>,
    upstream: MockServer,
    forwarding: Arc<AtomicBool>,
    deps: gate::tasks::LifecycleDeps,
    router: ControlRouter,
}

impl Drop for Ctl {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Ctl {
    async fn open() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("gate-lifecycle-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let audit = Arc::new(Audit::open(&dir.join("audit.jsonl")));
        let state = Arc::new(Mutex::new(DbState::open(":memory:").unwrap()));
        let upstream = MockServer::start().await;
        let base = reqwest::Url::parse(&upstream.uri()).unwrap();
        let platform: Arc<dyn Platform> = Arc::new(GitHubPlatform::with_bases(
            base.clone(),
            base,
            Arc::new(|| None),
        ));
        let forwarding = Arc::new(AtomicBool::new(false));
        let fake = Arc::new(FakeTransport::new());
        let api: Arc<dyn ControlApi> = fake.clone();
        let deps = gate::tasks::LifecycleDeps {
            state: state.clone(),
            platform,
            machine_login: MACHINE_LOGIN.to_string(),
            enc_key: ENC_KEY.to_string(),
            forwarding: forwarding.clone(),
            notify: Some((api.clone(), CHAT)),
            gate_host: HOST.to_string(),
            audit: audit.clone(),
        };
        let machine_token: Arc<dyn Fn() -> Option<String> + Send + Sync> = {
            let state = state.clone();
            let enc = ENC_KEY.to_string();
            Arc::new(move || {
                let guard = state.lock().unwrap();
                custody_of(&guard, &enc).load().unwrap_or(None)
            })
        };
        let mut router = ControlRouter::new();
        register_commands(
            &mut router,
            Arc::new(CommandDeps {
                state: state.clone(),
                audit: audit.clone(),
                machine_token,
                lifecycle: Some(deps.clone().into()),
                grant: None,
                reduction: None,
            }),
        );
        register_lifecycle(&mut router, deps.clone().into());
        // Composed nonce lookup exactly like main.rs: approval rows first,
        // then the lifecycle rotate flow.
        let approvals = gate::control::approvals::ControlApprovalBackend::nonce_lookup(state.clone());
        let rotate = nonce_lookup(state.clone());
        router.set_nonce_lookup(Arc::new(move |flow| approvals(flow).or_else(|| rotate(flow))));
        Self {
            dir,
            fake,
            api,
            state,
            upstream,
            forwarding,
            deps,
            router,
        }
    }

    /// Dispatch one operator command; return the single reply edit.
    async fn command(&self, text: &str) -> gate::control::send::SendRequest {
        let before = self.fake.edits().len();
        let update = Update {
            sender_id: Some(CHAT),
            text: Some(text.to_string()),
            message_ref: Some(MsgRef {
                chat_id: CHAT,
                message_id: 500,
            }),
            ..Default::default()
        };
        self.router.dispatch(&update, &self.api).await;
        let edits = self.fake.edits();
        assert_eq!(
            edits.len(),
            before + 1,
            "one command ⇒ exactly one reply edit: {edits:?}"
        );
        edits.last().unwrap().1.clone()
    }

    /// Press a signed button; return the reply edit it produced.
    async fn press(&self, data: &str) -> gate::control::send::SendRequest {
        let before = self.fake.edits().len();
        let update = Update {
            sender_id: Some(CHAT),
            callback: Some(gate::control::Callback {
                id: "cbq-1".to_string(),
                data: data.to_string(),
                from_id: CHAT,
            }),
            message_ref: Some(MsgRef {
                chat_id: CHAT,
                message_id: 77,
            }),
            ..Default::default()
        };
        self.router.dispatch(&update, &self.api).await;
        let edits = self.fake.edits();
        assert_eq!(edits.len(), before + 1, "button press ⇒ one edit: {edits:?}");
        edits.last().unwrap().1.clone()
    }

    fn kv(&self, key: &str) -> Option<String> {
        self.state.lock().unwrap().kv_get(key).unwrap()
    }

    fn set_kv(&self, key: &str, value: &str) {
        self.state.lock().unwrap().kv_set(key, value).unwrap();
    }

    fn store_token(&self, token: &str) {
        let guard = self.state.lock().unwrap();
        custody_of(&guard, ENC_KEY).store(token).unwrap();
    }

    fn custody_load(&self) -> Option<String> {
        let guard = self.state.lock().unwrap();
        custody_of(&guard, ENC_KEY).load().unwrap()
    }

    fn forwarding_on(&self) -> bool {
        self.forwarding.load(Ordering::Relaxed)
    }

    async fn startup(&self) {
        startup_check(&self.deps).await;
    }

    async fn tick(&self) {
        daily_tick(&self.deps).await;
    }

    /// Gate-initiated notices (startup/daily/reminders) land in `sent()`.
    fn notices(&self) -> Vec<gate::control::send::SendRequest> {
        self.fake.sent()
    }

    fn marked(&self, days: i64) -> bool {
        self.kv(&format!("reminder_sent:{days}")).is_some()
    }

    /// Point kv expiry `days` from now (may be negative = expired).
    fn set_expiry_days(&self, days: i64) {
        let ts = state::now() + days * DAY;
        self.set_kv(KV_TOKEN_EXPIRES_AT, &ts.to_string());
    }
}

// ---------------------------------------------------------------------------
// Startup check
// ---------------------------------------------------------------------------

#[tokio::test]
async fn startup_wrong_login_token_disables_forwarding_and_names_the_check() {
    let c = Ctl::open().await;
    c.store_token(OLD_TOKEN);
    // The token belongs to the operator's main account, not the machine.
    mount_user(&c.upstream, "robsmith", true, "repo, workflow, read:org", None, None).await;

    c.startup().await;

    assert!(!c.forwarding_on(), "wrong-login token must refuse forwarding");
    let notices = c.notices();
    assert_eq!(notices.len(), 1, "one startup notice: {:?}", notices);
    let text = &notices[0].text;
    assert!(text.to_lowercase().contains("wrong account"), "names the check: {text}");
    assert!(text.contains("robsmith"), "names the login found: {text}");
    assert!(text.contains("DISABLED") || text.to_lowercase().contains("disabled"), "{text}");
}

#[tokio::test]
async fn startup_no_token_disables_forwarding_and_prompts_setup() {
    let c = Ctl::open().await;

    c.startup().await;

    assert!(!c.forwarding_on(), "no token ⇒ forwarding off");
    let notices = c.notices();
    assert_eq!(notices.len(), 1, "one startup notice: {:?}", notices);
    assert!(
        notices[0].text.contains("/setup"),
        "absent token prompts /setup: {}", notices[0].text
    );
}

#[tokio::test]
async fn startup_transport_error_disables_forwarding_with_notice_not_crash() {
    let c = Ctl::open().await;
    c.store_token(OLD_TOKEN);
    // Dead upstream: capture the uri, then drop the server — every request
    // fails at the transport level (VerifyError::Upstream(0)). The Ctl's
    // live platform stays untouched for the other tests in this file.
    let dead = reqwest::Url::parse(&c.upstream.uri()).unwrap();
    let platform: Arc<dyn Platform> = Arc::new(GitHubPlatform::with_bases(
        dead,
        reqwest::Url::parse("http://git.example.internal/").unwrap(),
        Arc::new(|| None),
    ));
    let dead_deps = gate::tasks::LifecycleDeps {
        state: c.state.clone(),
        platform,
        machine_login: MACHINE_LOGIN.to_string(),
        enc_key: ENC_KEY.to_string(),
        forwarding: c.forwarding.clone(),
        notify: Some((c.api.clone(), CHAT)),
        gate_host: HOST.to_string(),
        audit: c.deps.audit.clone(),
    };
    startup_check(&dead_deps).await;

    assert!(!c.forwarding_on(), "transport error ⇒ forwarding off");
    let notices = c.notices();
    assert_eq!(notices.len(), 1, "transport failure still notifies: {:?}", notices);
    let text = notices[0].text.to_lowercase();
    assert!(
        text.contains("unreachable") || text.contains("transport") || text.contains("github"),
        "notice names the upstream failure: {}", notices[0].text
    );
}

#[tokio::test]
async fn startup_good_token_enables_forwarding_and_persists_expiry() {
    let c = Ctl::open().await;
    c.store_token(OLD_TOKEN);
    mount_good_user(&c.upstream, OLD_TOKEN, Some("2027-05-01T00:00:00Z")).await;

    c.startup().await;

    assert!(c.forwarding_on(), "verified token ⇒ forwarding on");
    assert_eq!(c.notices().len(), 0, "no notice on the happy path");
    assert!(
        c.kv(KV_LAST_VERIFIED_AT).is_some(),
        "last-verified timestamp persisted for /status"
    );
    let expiry = c.kv(KV_TOKEN_EXPIRES_AT).expect("expiry persisted");
    assert_eq!(
        expiry,
        parse_expires("2027-05-01T00:00:00Z").unwrap().to_string(),
        "token_expires_at comes from the verified expiry header"
    );
}

#[tokio::test]
async fn startup_unreadable_custody_disables_forwarding_with_notice() {
    let c = Ctl::open().await;
    // Store under one key, verify under another — custody load errors.
    c.store_token(OLD_TOKEN);
    let wrong_key_deps = gate::tasks::LifecycleDeps {
        state: c.state.clone(),
        platform: c.deps.platform.clone(),
        machine_login: MACHINE_LOGIN.to_string(),
        enc_key: "a-different-enc-key".to_string(),
        forwarding: c.forwarding.clone(),
        notify: Some((c.api.clone(), CHAT)),
        gate_host: HOST.to_string(),
        audit: c.deps.audit.clone(),
    };
    startup_check(&wrong_key_deps).await;

    assert!(!c.forwarding_on(), "unreadable custody ⇒ forwarding off");
    assert_eq!(c.notices().len(), 1, "unreadable custody notifies: {:?}", c.notices());
}

// ---------------------------------------------------------------------------
// Daily verify
// ---------------------------------------------------------------------------

#[tokio::test]
async fn daily_verify_pass_keeps_forwarding_on_and_stamps_last_verified() {
    let c = Ctl::open().await;
    c.store_token(OLD_TOKEN);
    mount_good_user(&c.upstream, OLD_TOKEN, Some("2027-05-01T00:00:00Z")).await;
    c.startup().await;
    let notices_before = c.notices().len();

    c.tick().await;

    assert!(c.forwarding_on(), "daily pass keeps forwarding on");
    assert_eq!(
        c.notices().len(),
        notices_before,
        "a pass sends no notice"
    );
    assert!(c.kv(KV_LAST_VERIFIED_AT).is_some(), "stamp refreshed");
}

#[tokio::test]
async fn daily_verify_fail_disables_forwarding_and_notices_the_check() {
    let c = Ctl::open().await;
    c.store_token(OLD_TOKEN);
    mount_good_user(&c.upstream, OLD_TOKEN, Some("2027-05-01T00:00:00Z")).await;
    c.startup().await;
    assert!(c.forwarding_on());

    // The token gets revoked upstream: the daily tick runs against a
    // second server whose /user answers 401 (wiremock matches the
    // first-mounted mock, so the failure must live on its own server).
    let revoked = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&revoked)
        .await;
    let revoked_deps = gate::tasks::LifecycleDeps {
        platform: Arc::new(GitHubPlatform::with_bases(
            reqwest::Url::parse(&revoked.uri()).unwrap(),
            reqwest::Url::parse("http://git.example.internal/").unwrap(),
            Arc::new(|| None),
        )),
        ..c.deps.clone()
    };
    daily_tick(&revoked_deps).await;

    assert!(!c.forwarding_on(), "daily fail ⇒ forwarding off");
    let notices = c.notices();
    assert_eq!(notices.len(), 1, "one failure notice: {:?}", notices);
    assert!(
        notices[0].text.to_lowercase().contains("github answered http 401")
            || notices[0].text.to_lowercase().contains("upstream"),
        "notice names the check: {}", notices[0].text
    );
}

#[tokio::test]
async fn daily_no_token_stays_quiet_and_off() {
    let c = Ctl::open().await;
    c.startup().await; // off + /setup prompt
    let notices_before = c.notices().len();

    c.tick().await;

    assert!(!c.forwarding_on());
    assert_eq!(
        c.notices().len(),
        notices_before,
        "the daily run must not re-spam the /setup prompt"
    );
}

// ---------------------------------------------------------------------------
// Expiry reminders (14 / 3 / 1 days) — kv-driven simulation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reminder_14_days_fires_once_no_duplicate_on_rerun() {
    let c = Ctl::open().await;
    c.store_token(OLD_TOKEN);
    // Verify passes and stamps an expiry 13 days out ⇒ within the 14-day
    // window. The mock's expiry header drives the kv value.
    let soon = gate::tasks::format_expires(state::now() + 13 * DAY);
    mount_good_user(&c.upstream, OLD_TOKEN, Some(&soon)).await;

    c.tick().await;

    assert!(c.marked(14), "reminder_sent:14 recorded");
    assert!(!c.marked(3) && !c.marked(1), "tighter thresholds not eligible yet");
    let notices = c.notices();
    assert_eq!(notices.len(), 1, "exactly one reminder: {:?}", notices);
    let reminder = &notices[0];
    assert!(
        reminder.text.contains("expires"),
        "reminder mentions expiry: {}",
        reminder.text
    );
    assert!(
        reminder
            .inline_buttons
            .iter()
            .any(|(label, _)| label == "Start /rotate"),
        "reminder carries the [Start /rotate] button: {:?}",
        reminder.inline_buttons
    );
    let cb_nonce = c.kv(KV_ROTATE_CB_NONCE).expect("callback nonce persisted");
    assert!(
        reminder.inline_buttons.iter().any(|(_, data)| data
            == &format!("{ROTATE_FLOW}:{cb_nonce}:start")),
        "button is the signed rotate triple: {:?}",
        reminder.inline_buttons
    );

    // Second run the same day: marked in kv ⇒ no duplicate.
    c.tick().await;
    assert_eq!(c.notices().len(), 1, "no duplicate reminder on re-run");
}

#[tokio::test]
async fn reminder_3_days_fires_once() {
    let c = Ctl::open().await;
    c.store_token(OLD_TOKEN);
    let soon = gate::tasks::format_expires(state::now() + 2 * DAY);
    mount_good_user(&c.upstream, OLD_TOKEN, Some(&soon)).await;

    c.tick().await;

    assert!(c.marked(3), "3-day reminder fires (2 days left)");
    assert!(!c.marked(14) && !c.marked(1), "only the tightest eligible threshold");
    assert_eq!(c.notices().len(), 1);
    c.tick().await;
    assert_eq!(c.notices().len(), 1, "no duplicate");
}

#[tokio::test]
async fn reminder_send_failure_leaves_marker_unset_and_retries_next_sweep() {
    let c = Ctl::open().await;
    c.store_token(OLD_TOKEN);
    let soon = gate::tasks::format_expires(state::now() + 13 * DAY);
    mount_good_user(&c.upstream, OLD_TOKEN, Some(&soon)).await;

    // First sweep: the Control send fails — the marker must NOT be
    // written (an undelivered reminder must never be lost).
    c.fake.fail_next_sends(1);
    c.tick().await;
    assert!(!c.marked(14), "undelivered reminder stays unmarked");
    assert!(
        c.notices().is_empty(),
        "failed sends record nothing: {:?}",
        c.notices()
    );

    // Second sweep: delivery succeeds — the reminder is sent and only now
    // marked, so restarts will not re-send it.
    c.tick().await;
    assert!(c.marked(14), "marker written only after a confirmed send");
    assert_eq!(c.notices().len(), 1, "retry delivered the reminder");
    c.tick().await;
    assert_eq!(c.notices().len(), 1, "delivered reminder never re-sends");
}

#[tokio::test]
async fn reminder_without_a_channel_is_never_marked() {
    let c = Ctl::open().await;
    c.store_token(OLD_TOKEN);
    let soon = gate::tasks::format_expires(state::now() + 13 * DAY);
    mount_good_user(&c.upstream, OLD_TOKEN, Some(&soon)).await;
    // No bot/chat configured: the verify still runs (forwarding on), but
    // the reminder has nowhere to go and must stay eligible (unmarked).
    let silent_deps = gate::tasks::LifecycleDeps {
        notify: None,
        ..c.deps.clone()
    };

    daily_tick(&silent_deps).await;

    assert!(c.forwarding_on(), "verify still passes without a channel");
    assert!(!c.marked(14), "no channel ⇒ nothing marked");
}

#[tokio::test]
async fn reminder_1_day_and_expired_fire_once() {
    let c = Ctl::open().await;
    c.store_token(OLD_TOKEN);
    let soon = gate::tasks::format_expires(state::now() + 10 * 3600);
    mount_good_user(&c.upstream, OLD_TOKEN, Some(&soon)).await;
    c.tick().await;
    assert!(c.marked(1), "1-day reminder fires (same-day expiry)");
    assert_eq!(c.notices().len(), 1);

    // Expired token: still one urgent reminder (once).
    let c2 = Ctl::open().await;
    c2.store_token(OLD_TOKEN);
    let past = gate::tasks::format_expires(state::now() - 3600);
    mount_good_user(&c2.upstream, OLD_TOKEN, Some(&past)).await;
    c2.tick().await;
    assert!(c2.marked(1), "expired token marks the tightest threshold");
    let notices = c2.notices();
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0].text.to_lowercase().contains("expired"),
        "expired reminder says so: {}",
        notices[0].text
    );
    c2.tick().await;
    assert_eq!(c2.notices().len(), 1, "no duplicate");
}

// ---------------------------------------------------------------------------
// /rotate and /disconnect over Control
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rotate_without_a_stored_token_answers_guidance_and_marks_nothing() {
    let c = Ctl::open().await;
    let reply = c.command("/rotate").await;
    assert!(
        reply.text.to_lowercase().contains("no machine token"),
        "rotate with empty custody guides to setup instead: {}",
        reply.text
    );
    assert!(c.kv(KV_ROTATE_IN_PROGRESS).is_none(), "no rotate marker");
    assert!(c.kv(KV_NONCE).is_none(), "no form nonce minted");
}

#[tokio::test]
async fn rotate_command_sends_prefilled_link_one_time_form_and_marks_rotating() {
    let c = Ctl::open().await;
    c.store_token(OLD_TOKEN);

    let reply = c.command("/rotate").await;

    // Prefilled token-page link (exact GATE-004 URL) as a link button.
    let expected_page = "https://github.com/settings/tokens/new?scopes=repo,workflow,read:org&description=GitHub%20agent%20proxy";
    assert!(
        reply
            .link_buttons
            .iter()
            .any(|(_, url)| url == expected_page),
        "prefilled token-page link button present: {:?}",
        reply.link_buttons
    );
    // One-time form link with a fresh 15-minute nonce, and the marker set.
    let (_, form_url) = reply
        .link_buttons
        .iter()
        .find(|(label, _)| label.contains("form"))
        .expect("one-time form link button");
    let nonce = form_url
        .split_once("nonce=")
        .map(|(_, n)| n.to_string())
        .expect("form url carries the nonce");
    assert_eq!(nonce.len(), 64, "fresh setup nonce (32 bytes hex)");
    assert_eq!(
        c.kv(KV_NONCE).as_deref(),
        Some(nonce.as_str()),
        "form nonce stored in kv"
    );
    assert_eq!(
        c.kv(KV_ROTATE_IN_PROGRESS).as_deref(),
        Some(nonce.as_str()),
        "rotate_in_progress marks this nonce as a ROTATION"
    );
    assert!(
        form_url.starts_with(&format!("{HOST}/setup/token")),
        "form link uses the gate host: {form_url}"
    );
}

#[tokio::test]
async fn reminder_start_rotate_button_runs_the_same_intake() {
    let c = Ctl::open().await;
    c.store_token(OLD_TOKEN);
    let soon = gate::tasks::format_expires(state::now() + 13 * DAY);
    mount_good_user(&c.upstream, OLD_TOKEN, Some(&soon)).await;
    c.tick().await;
    let cb_nonce = c.kv(KV_ROTATE_CB_NONCE).unwrap();

    let reply = c
        .press(&format!("{ROTATE_FLOW}:{cb_nonce}:start"))
        .await;

    // Same intake as /rotate: form link + rotating marker.
    assert!(
        reply
            .link_buttons
            .iter()
            .any(|(_, url)| url.contains("/setup/token?nonce=")),
        "button press delivers the one-time form: {:?}",
        reply.link_buttons
    );
    assert!(c.kv(KV_ROTATE_IN_PROGRESS).is_some(), "marker set");
    assert!(
        c.fake.answered().contains(&"cbq-1".to_string()),
        "spinner stopped on the press"
    );
}

#[tokio::test]
async fn disconnect_erases_custody_disables_forwarding_and_instructs_manual_deletion() {
    let c = Ctl::open().await;
    c.store_token(OLD_TOKEN);
    c.forwarding.store(true, Ordering::Relaxed);
    c.set_expiry_days(13);
    c.set_kv("reminder_sent:14", "1");

    let reply = c.command("/disconnect").await;

    assert!(!c.forwarding_on(), "forwarding stops immediately");
    assert_eq!(c.custody_load(), None, "custody erased");
    assert!(c.kv(KV_TOKEN_EXPIRES_AT).is_none(), "expiry cleared");
    assert!(!c.marked(14), "reminders cleared");
    assert!(c.kv(KV_ROTATE_IN_PROGRESS).is_none(), "pending rotate cancelled");
    assert!(
        reply
            .link_buttons
            .iter()
            .any(|(label, url)| label == "Open tokens page"
                && url == "https://github.com/settings/tokens"),
        "[Open tokens page] button for manual deletion: {:?}",
        reply.link_buttons
    );
    let text = reply.text.to_lowercase();
    assert!(
        text.contains("delete"),
        "reply instructs manual GitHub-side deletion: {}",
        reply.text
    );
    assert!(
        text.contains("revoke"),
        "reply points at /revoke for verified per-repo cleanup: {}",
        reply.text
    );
    let audit = std::fs::read_to_string(c.dir.join("audit.jsonl")).unwrap();
    assert!(
        audit.contains("custody.disconnect"),
        "disconnect audited: {audit}"
    );
}

/// Task 9 (GATE-010 §10): /disconnect completes with per-repo removal
/// links — every registry row with access gets its collaborator-settings
/// link so the operator can strip the machine account everywhere.
#[tokio::test]
async fn disconnect_lists_per_repo_removal_links_from_the_registry() {
    let c = Ctl::open().await;
    c.store_token(OLD_TOKEN);
    {
        let guard = c.state.lock().unwrap();
        guard
            .registry_upsert("acme/api", "write", "write", "collaborator")
            .unwrap();
        guard
            .registry_upsert("acme/blog", "read", "read", "org_team")
            .unwrap();
        guard
            .registry_upsert("acme/gone", "none", "write", "collaborator")
            .unwrap();
    }

    let reply = c.command("/disconnect").await;

    assert!(
        reply.text.contains("Per-repo cleanup"),
        "the reply carries the per-repo cleanup section: {}",
        reply.text
    );
    assert!(
        !reply.text.contains("arrives in a later update"),
        "the placeholder sentence is gone: {}",
        reply.text
    );
    // One link button per row that still has access; none-rows are skipped.
    for repo in ["acme/api", "acme/blog"] {
        assert!(
            reply.link_buttons.iter().any(|(label, url)| label.as_str() == repo
                && url.as_str() == format!("https://github.com/{repo}/settings/access")),
            "link button for {repo}: {:?}",
            reply.link_buttons
        );
    }
    assert!(
        !reply
            .link_buttons
            .iter()
            .any(|(label, _)| label.as_str() == "acme/gone"),
        "a none-row needs no removal link: {:?}",
        reply.link_buttons
    );
    assert!(
        reply
            .link_buttons
            .iter()
            .any(|(label, _)| label == "Open tokens page"),
        "the manual token-deletion link stays: {:?}",
        reply.link_buttons
    );
    // Empty registry ⇒ the section explains there is nothing to clean.
    let c2 = Ctl::open().await;
    c2.store_token(OLD_TOKEN);
    let bare = c2.command("/disconnect").await;
    assert!(
        bare.text.contains("no per-repo cleanup")
            || bare.text.to_lowercase().contains("nothing to clean"),
        "empty registry explains the noop: {}",
        bare.text
    );
}

// ---------------------------------------------------------------------------
// Rotate completion through the real one-time form (HTTP)
// ---------------------------------------------------------------------------

struct Gate {
    port: u16,
    dir: PathBuf,
    state: Arc<Mutex<DbState>>,
    upstream: MockServer,
    fake: Arc<FakeTransport>,
    forwarding: Arc<AtomicBool>,
    deps: gate::tasks::LifecycleDeps,
}

impl Drop for Gate {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Gate {
    fn kv(&self, key: &str) -> Option<String> {
        self.state.lock().unwrap().kv_get(key).unwrap()
    }

    fn custody_load(&self) -> Option<String> {
        let guard = self.state.lock().unwrap();
        custody_of(&guard, ENC_KEY).load().unwrap()
    }

    fn forwarding_on(&self) -> bool {
        self.forwarding.load(Ordering::Relaxed)
    }

    fn set_kv(&self, key: &str, value: &str) {
        self.state.lock().unwrap().kv_set(key, value).unwrap();
    }

    fn marked(&self, days: i64) -> bool {
        self.kv(&format!("reminder_sent:{days}")).is_some()
    }

    async fn start_rotate_via_reply(&self) -> String {
        let reply = rotate_reply(&self.deps, None);
        let (_, url) = reply
            .link_buttons
            .iter()
            .find(|(_, url)| url.contains("/setup/token?nonce="))
            .expect("rotate intake carries the form link");
        url.split_once("nonce=").unwrap().1.to_string()
    }

    async fn post_form(&self, nonce: &str, token: &str) -> (reqwest::StatusCode, String) {
        let resp = reqwest::Client::new()
            .post(format!(
                "http://127.0.0.1:{}/setup/token?nonce={nonce}",
                self.port
            ))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("token={token}"))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body = String::from_utf8_lossy(&resp.bytes().await.unwrap()).into_owned();
        (status, body)
    }
}

async fn spawn_gate() -> Gate {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gate-lifecycle-form-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let audit = Arc::new(Audit::open(&dir.join("audit.jsonl")));
    let state = Arc::new(Mutex::new(DbState::open(":memory:").unwrap()));
    let upstream = MockServer::start().await;
    let base = reqwest::Url::parse(&upstream.uri()).unwrap();
    let platform: Arc<dyn Platform> = Arc::new(GitHubPlatform::with_bases(
        base.clone(),
        base,
        Arc::new(|| None),
    ));
    let forwarding = Arc::new(AtomicBool::new(false));
    let fake = Arc::new(FakeTransport::new());
    let api: Arc<dyn ControlApi> = fake.clone();
    let notify = Some((api, CHAT));
    let deps = gate::tasks::LifecycleDeps {
        state: state.clone(),
        platform: platform.clone(),
        machine_login: MACHINE_LOGIN.to_string(),
        enc_key: ENC_KEY.to_string(),
        forwarding: forwarding.clone(),
        notify: notify.clone(),
        gate_host: HOST.to_string(),
        audit: audit.clone(),
    };
    let gs = GateState {
        state: state.clone(),
        audit,
        rate: Mutex::new(RateLimiter::default()),
        git: None,
        api: None,
        onepage: Some(Arc::new(OnePageDeps {
            gate_host: HOST.to_string(),
            machine_login: MACHINE_LOGIN.to_string(),
            platform,
            enc_key: ENC_KEY.to_string(),
            notify,
        })),
        max_body_bytes: gate::config::DEFAULT_MAX_BODY_BYTES,
        control_live: std::sync::atomic::AtomicBool::new(false),
        forwarding_enabled: forwarding.clone(),
    };
    let app = router(gs);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    Gate {
        port,
        dir,
        state,
        upstream,
        fake,
        forwarding,
        deps,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotate_form_success_swaps_custody_keeps_forwarding_and_notifies_old_removal() {
    let g = spawn_gate().await;
    // Old token live, forwarding on, a 14-day reminder already sent.
    {
        let guard = g.state.lock().unwrap();
        custody_of(&guard, ENC_KEY).store(OLD_TOKEN).unwrap();
    }
    g.forwarding.store(true, Ordering::Relaxed);
    g.set_kv(KV_TOKEN_EXPIRES_AT, &(state::now() + 13 * DAY).to_string());
    g.set_kv("reminder_sent:14", "1");

    // Start the rotation, then submit the NEW token through the form.
    let nonce = g.start_rotate_via_reply().await;
    mount_good_user(&g.upstream, NEW_TOKEN, Some("2027-08-01T00:00:00Z")).await;
    let (status, page) = g.post_form(&nonce, NEW_TOKEN).await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(
        !page.contains(NEW_TOKEN) && !page.contains(OLD_TOKEN),
        "no token ever echoes back: {page}"
    );
    // Atomic swap: custody now decrypts to the NEW token only.
    assert_eq!(g.custody_load().as_deref(), Some(NEW_TOKEN), "custody swapped");
    assert!(g.forwarding_on(), "forwarding uninterrupted across the swap");
    // Marker consumed; fresh expiry; reminder cycle reset.
    assert!(g.kv(KV_ROTATE_IN_PROGRESS).is_none(), "rotate marker cleared");
    assert_eq!(
        g.kv(KV_TOKEN_EXPIRES_AT).as_deref(),
        Some(parse_expires("2027-08-01T00:00:00Z").unwrap().to_string().as_str()),
        "expiry updated from the new token"
    );
    assert!(!g.marked(14), "reminders cleared on rotate");
    // Success notice with the old-token-removal instruction.
    let notices = g.fake.sent();
    assert_eq!(notices.len(), 1, "one rotate-complete notice: {:?}", notices);
    let text = notices[0].text.to_lowercase();
    assert!(text.contains("rotated") || text.contains("rotate"), "{}", notices[0].text);
    assert!(
        text.contains("delete") && text.contains("old"),
        "notice instructs old-token deletion: {}",
        notices[0].text
    );
    assert!(
        notices[0]
            .link_buttons
            .iter()
            .any(|(label, url)| label == "Open tokens page"
                && url == "https://github.com/settings/tokens"),
        "[Open tokens page] button: {:?}",
        notices[0].link_buttons
    );
    let audit = std::fs::read_to_string(g.dir.join("audit.jsonl")).unwrap();
    assert!(audit.contains("custody.rotate"), "rotation audited: {audit}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotate_form_failure_keeps_old_token_forwarding_and_marker() {
    let g = spawn_gate().await;
    {
        let guard = g.state.lock().unwrap();
        custody_of(&guard, ENC_KEY).store(OLD_TOKEN).unwrap();
    }
    g.forwarding.store(true, Ordering::Relaxed);

    let nonce = g.start_rotate_via_reply().await;
    // Upstream rejects the new token with a 500 (verification failure path).
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&g.upstream)
        .await;
    let (status, page) = g.post_form(&nonce, NEW_TOKEN).await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(
        page.contains("GitHub answered HTTP 500") || page.contains("GitHub error"),
        "failure page names the upstream failure: {page}"
    );
    // Review Focus 5: the old token and forwarding are untouched.
    assert_eq!(
        g.custody_load().as_deref(),
        Some(OLD_TOKEN),
        "failed verification must keep the old token"
    );
    assert!(g.forwarding_on(), "forwarding unchanged on failure");
    assert!(
        g.kv(KV_ROTATE_IN_PROGRESS).is_some(),
        "rotate marker survives for retry"
    );
    assert!(
        g.kv(KV_NONCE).is_some(),
        "form nonce alive for retry after a failed verify"
    );
    assert!(
        g.fake.sent().is_empty(),
        "no success notice when nothing was stored: {:?}",
        g.fake.sent()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_setup_form_still_stores_and_enables_forwarding() {
    // Non-rotate form use: no marker ⇒ same storage path, flag flips on
    // (the token was verified by the form), but NO rotate notice.
    let g = spawn_gate().await;
    let nonce = mint_setup_nonce(&g.state.lock().unwrap()).unwrap();
    mount_good_user(&g.upstream, NEW_TOKEN, Some("2027-08-01T00:00:00Z")).await;
    let (status, _page) = g.post_form(&nonce, NEW_TOKEN).await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(g.custody_load().as_deref(), Some(NEW_TOKEN));
    assert!(g.forwarding_on(), "verified stored token enables forwarding");
    assert!(
        g.fake.sent().is_empty(),
        "first setup is Task 5's business — no rotate notice: {:?}",
        g.fake.sent()
    );
    assert!(g.kv(KV_TOKEN_EXPIRES_AT).is_some(), "expiry persisted");
}

// ---------------------------------------------------------------------------
// Forwarding flag short-circuit: flag off ⇒ credential_unavailable BEFORE
// any custody load or upstream probe.
// ---------------------------------------------------------------------------

struct ProxyGate {
    port: u16,
    dir: PathBuf,
    upstream: MockServer,
    state: Arc<Mutex<DbState>>,
    forwarding: Arc<AtomicBool>,
    agent_token: String,
}

impl Drop for ProxyGate {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const POLICY: &str = "\
[defaults]
push = \"allow\"
force-push = \"allow\"
branch-delete = \"allow\"
tag-delete = \"allow\"

[api]
mutations = \"allow\"
";

async fn spawn_proxy_gate() -> ProxyGate {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gate-lifecycle-proxy-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let upstream = MockServer::start().await;
    let audit = Arc::new(Audit::open(&dir.join("audit.jsonl")));
    let db = DbState::open(":memory:").unwrap();
    let agent_token = {
        let store = TokenStore::new(&db, &audit);
        store.mint(AGENT).unwrap()
    };
    let state = Arc::new(Mutex::new(db));
    // Custody-backed getter (mirrors main.rs): only the flag check sits
    // ahead of it in the request path.
    let enc = ENC_KEY.to_string();
    let getter_state = state.clone();
    let machine_token: Arc<dyn Fn() -> Option<String> + Send + Sync> =
        Arc::new(move || {
            let guard = getter_state.lock().unwrap();
            custody_of(&guard, &enc).load().unwrap_or(None)
        });
    let base = reqwest::Url::parse(&upstream.uri()).unwrap();
    let platform: Arc<dyn Platform> = Arc::new(GitHubPlatform::with_bases(
        base.clone(),
        base,
        machine_token.clone(),
    ));
    let policy = PolicyEngine::new(&PolicyFile::parse(POLICY).unwrap());
    let backend = Arc::new(StubBackend::new(state.clone(), audit.clone(), ApprovalOutcome::Denied).unwrap());
    let git = GitDeps::new(
        state.clone(),
        platform.clone(),
        policy.clone(),
        backend.clone(),
        MACHINE_LOGIN.to_string(),
        machine_token.clone(),
        Duration::from_secs(3),
    )
    .unwrap();
    let api = ApiDeps::new(
        state.clone(),
        platform,
        policy,
        backend,
        machine_token,
        Duration::from_secs(3),
    )
    .unwrap();
    let forwarding = Arc::new(AtomicBool::new(false));
    let gs = GateState {
        state: state.clone(),
        audit,
        rate: Mutex::new(RateLimiter::default()),
        git: Some(Arc::new(git)),
        api: Some(Arc::new(api)),
        onepage: None,
        max_body_bytes: gate::config::DEFAULT_MAX_BODY_BYTES,
        control_live: std::sync::atomic::AtomicBool::new(false),
        forwarding_enabled: forwarding.clone(),
    };
    let app = router(gs);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    ProxyGate {
        port,
        dir,
        upstream,
        state,
        forwarding,
        agent_token,
    }
}

impl ProxyGate {
    fn seed_token(&self) {
        let guard = self.state.lock().unwrap();
        custody_of(&guard, ENC_KEY).store(OLD_TOKEN).unwrap();
    }

    async fn upstream_requests(&self) -> Vec<wiremock::Request> {
        self.upstream.received_requests().await.unwrap()
    }
}

fn basic_auth_header(token: &str) -> String {
    use base64::Engine as _;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{AGENT}:{token}"))
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_proxy_flag_off_short_circuits_before_the_credential_probe() {
    let g = spawn_proxy_gate().await;
    g.seed_token();
    // Canary: ANY successful /user probe would hit this and forward.
    mount_good_user(&g.upstream, OLD_TOKEN, None).await;

    // Flag off (the startup state with an unverified credential).
    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{}/api/v3/user", g.port))
        .header("authorization", format!("Bearer {}", g.agent_token))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "credential_unavailable");
    assert!(
        g.upstream_requests().await.is_empty(),
        "flag off ⇒ NO upstream probe (canary untouched)"
    );

    // Flag on: the very same request now probes and forwards.
    g.forwarding.store(true, Ordering::Relaxed);
    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{}/api/v3/user", g.port))
        .header("authorization", format!("Bearer {}", g.agent_token))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert!(
        !g.upstream_requests().await.is_empty(),
        "flag on ⇒ probe + forward happen"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_proxy_flag_off_short_circuits_before_custody_load() {
    let g = spawn_proxy_gate().await;
    g.seed_token();

    let resp = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{}/git/octo/hello/info/refs?service=git-upload-pack",
            g.port
        ))
        .header("authorization", basic_auth_header(&g.agent_token))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_GATEWAY);
    let body = String::from_utf8_lossy(&resp.bytes().await.unwrap()).into_owned();
    assert!(
        body.contains("machine credential unavailable"),
        "existing credential_unavailable shape: {body}"
    );
    assert!(
        g.upstream_requests().await.is_empty(),
        "flag off ⇒ nothing forwarded, no custody churn upstream"
    );

    // Flag on: the request is forwarded upstream (default 404 passthrough).
    g.forwarding.store(true, Ordering::Relaxed);
    let resp = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{}/git/octo/hello/info/refs?service=git-upload-pack",
            g.port
        ))
        .header("authorization", basic_auth_header(&g.agent_token))
        .send()
        .await
        .unwrap();
    assert_ne!(resp.status(), reqwest::StatusCode::BAD_GATEWAY);
    assert!(
        !g.upstream_requests().await.is_empty(),
        "flag on ⇒ forwarded upstream"
    );
}

// ---------------------------------------------------------------------------
// /rotate + /disconnect reply builders used directly (unit-ish coverage of
// the exact strings) — kept here so the Control command surface and the
// form both speak identical text.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disconnect_reply_builder_flips_everything_off() {
    let c = Ctl::open().await;
    c.store_token(OLD_TOKEN);
    c.forwarding.store(true, Ordering::Relaxed);
    c.set_expiry_days(3);

    let reply = disconnect_reply(&c.deps, Some(CHAT));

    assert!(!c.forwarding_on());
    assert_eq!(c.custody_load(), None);
    assert!(c.kv(KV_TOKEN_EXPIRES_AT).is_none());
    let _ = reply;
}

#[tokio::test]
async fn daily_interval_is_24_hours() {
    assert_eq!(gate::tasks::DAILY_INTERVAL, Duration::from_secs(86_400));
}

#[test]
fn parse_expires_handles_both_github_header_shapes() {
    assert_eq!(parse_expires("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(
        parse_expires("2027-05-01T00:00:00Z"),
        Some(1_809_129_600),
        "the canonical test expiry"
    );
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
    assert_eq!(parse_expires("garbage"), None);
    assert_eq!(parse_expires(""), None);
}
