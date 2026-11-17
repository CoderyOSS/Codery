//! Integration: guided Telegram setup walkthrough (GATE-011, task 5).
//! A user with no stored token goes from `/setup` to a working agent
//! setup, step by step, with durable resumable progress:
//!
//! - full happy path steps 0–7 (username 404-then-200, form success via
//!   the one-time-page hook) reaches step 8 and finishes with the summary;
//! - a username that does not exist repeats the ask without advancing;
//! - a taken username suggests `{name}-agent-1` / `{name}-bot`;
//! - paste intake deletes the pasted message, verifies, stores — wrong
//!   login names the login found and returns to step 6 with the form
//!   nonce still valid;
//! - `[Cancel setup]` clears the durable rows;
//! - restart resume keeps the same step, sends no duplicate when the step
//!   row is younger than 1 h, resends when it is older (Review Focus 1);
//! - step 8's checklist updates ONE message in place per repo outcome;
//! - `[I'm stuck]` resends with extended detail;
//! - the one-time form's success path advances a FIRST setup (not a
//!   rotation) to step 8 over real HTTP.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use gate::audit::Audit;
use gate::custody::MachineTokenStore;
use gate::github::{GitHubPlatform, Platform, TOKEN_NEW_URL};
use gate::onepage::{mint_setup_nonce, OnePageDeps, KV_NONCE, KV_NONCE_EXPIRES};
use gate::routes::{router, GateState, RateLimiter};
use gate::state::{self, State as DbState};
use gate::tasks::{KV_ROTATE_IN_PROGRESS, REMINDER_DAYS};
use gate::telegram::commands::{register_commands, CommandDeps};
use gate::telegram::fake::FakeTelegram;
use gate::telegram::lifecycle::nonce_lookup as rotate_nonce_lookup;
use gate::telegram::setup::{
    form_stored_hook, nonce_lookup as setup_nonce_lookup, register_setup, resume, FakeGrantFlow,
    GrantOutcome, SetupDeps,
};
use gate::telegram::{MsgRef, TelegramApi, TelegramRouter, Update};

const MACHINE_LOGIN: &str = "rob-agent";
const MAIN_TOKEN: &str = "ghp_pasteEXAMPLEtoken00000001";
const ENC_KEY: &str = "setup-test-enc-key";
const CHAT: i64 = 4242;
const HOST: &str = "https://gag.example.com";
const USERNAME: &str = "rob-agent";

// ---------------------------------------------------------------------------
// Fixture A — control plane: state + FakeTelegram + router (commands +
// setup handlers, composed nonce lookup), wiremock upstream, direct
// dispatch (no poll loop). `fallback = false` ⇒ GATE_PUBLIC_URL set.
// ---------------------------------------------------------------------------

struct Ctl {
    dir: PathBuf,
    fake: Arc<FakeTelegram>,
    api: Arc<dyn TelegramApi>,
    state: Arc<Mutex<DbState>>,
    upstream: MockServer,
    forwarding: Arc<AtomicBool>,
    grant: Arc<FakeGrantFlow>,
    deps: Arc<SetupDeps>,
    router: TelegramRouter,
    _fallback: bool,
}

impl Drop for Ctl {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Ctl {
    async fn open(fallback: bool) -> Self {
        Self::with_grant(fallback, FakeGrantFlow::new()).await
    }

    /// Open with a pre-scripted grant flow (the handler consumes the same
    /// Arc the assertions read).
    async fn with_grant(fallback: bool, grant: FakeGrantFlow) -> Self {
        let grant = Arc::new(grant);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "gate-setup-{}-{nanos}-{}",
            std::process::id(),
            fallback as u8
        ));
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
        let fake = Arc::new(FakeTelegram::new());
        let api: Arc<dyn TelegramApi> = fake.clone();
        let deps = Arc::new(SetupDeps {
            state: state.clone(),
            platform,
            machine_login: MACHINE_LOGIN.to_string(),
            enc_key: ENC_KEY.to_string(),
            gate_public_url: if fallback { String::new() } else { HOST.to_string() },
            chat_id: CHAT,
            forwarding: forwarding.clone(),
            grant: grant.clone(),
            audit: audit.clone(),
        });
        let machine_token: Arc<dyn Fn() -> Option<String> + Send + Sync> = {
            let state = state.clone();
            let enc = ENC_KEY.to_string();
            Arc::new(move || {
                let guard = state.lock().unwrap();
                MachineTokenStore::new(&guard, &enc).load().unwrap_or(None)
            })
        };
        let mut router = TelegramRouter::new();
        register_commands(
            &mut router,
            Arc::new(CommandDeps {
                state: state.clone(),
                audit,
                machine_token,
                lifecycle: None,
                grant: None,
            }),
        );
        register_setup(&mut router, deps.clone());
        // Composed nonce lookup like main.rs: approvals, rotate, setup.
        let approvals = gate::telegram::approvals::TelegramApprovalBackend::nonce_lookup(state.clone());
        let rotate = rotate_nonce_lookup(state.clone());
        let setup = setup_nonce_lookup(state.clone());
        router.set_nonce_lookup(Arc::new(move |flow| {
            approvals(flow).or_else(|| rotate(flow)).or_else(|| setup(flow))
        }));
        Self {
            dir,
            fake,
            api,
            state,
            upstream,
            forwarding,
            grant,
            deps,
            router,
            _fallback: fallback,
        }
    }

    /// Dispatch one operator command; the reply must be exactly one edit.
    async fn command(&self, text: &str) -> gate::telegram::send::SendRequest {
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

    /// Dispatch a plain (non-command) operator message.
    async fn plain_text(&self, text: &str) {
        let update = Update {
            sender_id: Some(CHAT),
            text: Some(text.to_string()),
            message_ref: Some(MsgRef {
                chat_id: CHAT,
                message_id: 900 + text.len() as i32,
            }),
            ..Default::default()
        };
        self.router.dispatch(&update, &self.api).await;
    }

    /// Press one of the walkthrough's buttons by ACTION: extract the
    /// signed callback_data from the newest RENDERED outbound message
    /// that carries it (review round 1, F1 — the harness echoes what
    /// production would actually deliver instead of reconstructing the
    /// wire format from state).
    async fn press(&self, action: &str) {
        let data = self.rendered_button_data(action)
            .unwrap_or_else(|| panic!("no rendered setup button for action '{action}'"));
        let update = Update {
            sender_id: Some(CHAT),
            callback: Some(gate::telegram::Callback {
                id: "cbq-1".to_string(),
                data,
                from_id: CHAT,
            }),
            message_ref: Some(MsgRef {
                chat_id: CHAT,
                message_id: 77,
            }),
            ..Default::default()
        };
        self.router.dispatch(&update, &self.api).await;
    }

    /// Newest-first search over rendered outbound messages for a setup
    /// button performing `action`; returns its exact callback_data.
    fn rendered_button_data(&self, action: &str) -> Option<String> {
        let wanted = |data: &str| {
            data.split(':').next() == Some(gate::telegram::setup::SETUP_FLOW)
                && data.rsplit(':').next() == Some(action)
        };
        for (_, req) in self.fake.edits().iter().rev() {
            if let Some((_, d)) = req.inline_buttons.iter().find(|(_, d)| wanted(d)) {
                return Some(d.clone());
            }
        }
        for req in self.fake.sent().iter().rev() {
            if let Some((_, d)) = req.inline_buttons.iter().find(|(_, d)| wanted(d)) {
                return Some(d.clone());
            }
        }
        None
    }

    fn step(&self) -> Option<u8> {
        self.pget("step").and_then(|v| v.parse().ok())
    }

    fn pget(&self, key: &str) -> Option<String> {
        self.state.lock().unwrap().setup_get(key).unwrap()
    }

    fn kv(&self, key: &str) -> Option<String> {
        self.state.lock().unwrap().kv_get(key).unwrap()
    }

    fn custody_load(&self) -> Option<String> {
        let guard = self.state.lock().unwrap();
        MachineTokenStore::new(&guard, ENC_KEY).load().unwrap()
    }

    fn edits(&self) -> Vec<(MsgRef, gate::telegram::send::SendRequest)> {
        self.fake.edits()
    }

    fn sent(&self) -> Vec<gate::telegram::send::SendRequest> {
        self.fake.sent()
    }

    fn deletes(&self) -> Vec<MsgRef> {
        self.fake.deletes()
    }

    /// `GET /users/{name}`: 404 for the first `misses` calls, then 200.
    async fn mount_users_then_ok(&self, name: &str, misses: usize) {
        let counter = Arc::new(AtomicUsize::new(0));
        let body_login = name.to_string();
        Mock::given(method("GET"))
            .and(path(format!("/users/{name}")))
            .respond_with(move |_req: &wiremock::Request| {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n < misses {
                    ResponseTemplate::new(404)
                } else {
                    ResponseTemplate::new(200).set_body_string(format!(
                        r#"{{"login":"{}","id":4242}}"#,
                        body_login
                    ))
                }
            })
            .mount(&self.upstream)
            .await;
    }

    /// `GET /users/{name}`: always answers `status`.
    async fn mount_users_status(&self, name: &str, status: u16) {
        Mock::given(method("GET"))
            .and(path(format!("/users/{name}")))
            .respond_with(ResponseTemplate::new(status))
            .mount(&self.upstream)
            .await;
    }

    /// `GET /user` for token verification: login/2FA/scopes/expiry.
    async fn mount_verify(&self, login: &str, bearer: Option<&str>) {
        let body = format!(
            r#"{{"login":"{login}","id":4242,"two_factor_authentication":true}}"#
        );
        let tpl = ResponseTemplate::new(200)
            .set_body_string(body)
            .append_header("X-OAuth-Scopes", "repo, workflow, read:org")
            .append_header("github-authentication-token-expiration", "2027-05-01T00:00:00Z");
        let mut mock = Mock::given(method("GET")).and(path("/user"));
        if let Some(token) = bearer {
            mock = mock.and(header("authorization", format!("Bearer {token}")));
        }
        mock.respond_with(tpl).mount(&self.upstream).await;
    }

    /// Drive the walkthrough from nothing to `target` (the intake step).
    /// `past` controls how many 404s the username probe returns first.
    async fn drive_to_intake(&self, misses: usize) {
        self.mount_users_then_ok(USERNAME, misses).await;
        self.command("/setup").await; // step 0
        self.press("start").await; // 1
        self.plain_text("rob@example.com").await; // alias hint
        self.press("done").await; // 2
        self.plain_text(USERNAME).await; // (may repeat-ask on 404s)
        while self.step() == Some(2) {
            self.plain_text(USERNAME).await; // retry until the account "exists"
        }
        self.press("done").await; // 4
        self.press("done").await; // 5
        self.press("done").await; // 6 + 7
        assert_eq!(self.step(), Some(7), "drive lands on the intake step");
    }
}

// ---------------------------------------------------------------------------
// Happy path: steps 0–8 through to the finish summary
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn happy_path_reaches_step_eight_and_finishes_with_summary() {
    // The step-8 grant flow is scripted up front; the handler consumes the
    // same Arc these assertions read.
    let grant = FakeGrantFlow::new()
        .with_outcome(
            "acme/one",
            GrantOutcome::Granted {
                role: "write".into(),
            },
        )
        .with_outcome(
            "cme/two",
            GrantOutcome::Pending {
                detail: "invitation sent — accept it on GitHub".into(),
            },
        );
    let c = Ctl::with_grant(false, grant).await;
    // A stale rotate marker must be cleared by the setup intake (Task 4 note).
    c.state
        .lock()
        .unwrap()
        .kv_set(KV_ROTATE_IN_PROGRESS, "stale-rotate-nonce")
        .unwrap();

    // Step 0: the /setup command edits the intro into its own bubble.
    let intro = c.command("/setup").await;
    assert_eq!(c.step(), Some(0));
    assert!(
        intro.text.to_lowercase().contains("machine account"),
        "intro explains the machine account: {}",
        intro.text
    );
    assert!(
        intro
            .inline_buttons
            .iter()
            .any(|(label, _)| label == "Start"),
        "intro carries [Start]: {:?}",
        intro.inline_buttons
    );
    assert!(
        intro
            .inline_buttons
            .iter()
            .any(|(label, _)| label == "I'm stuck")
            && intro.inline_buttons.iter().any(|(label, _)| label == "Cancel setup"),
        "every step carries [I'm stuck] + [Cancel setup]: {:?}",
        intro.inline_buttons
    );

    // Steps 1–5.
    c.press("start").await;
    assert_eq!(c.step(), Some(1));
    c.plain_text("rob@example.com").await;
    let with_alias = c.edits().last().unwrap().1.clone();
    assert!(
        with_alias.text.contains("rob+agent@example.com"),
        "plus-address alias rendered from the hint: {}",
        with_alias.text
    );
    c.press("done").await;
    assert_eq!(c.step(), Some(2));

    // Step 2: probes 404 twice (repeat asks, no advance), then 200s.
    c.mount_users_then_ok(USERNAME, 2).await;
    c.plain_text(USERNAME).await;
    assert_eq!(c.step(), Some(2), "username not on GitHub yet ⇒ no advance");
    let ask_again = c.edits().last().unwrap().1.clone();
    assert!(
        ask_again.text.to_lowercase().contains("on github yet"),
        "step 2 repeat-ask names the state: {}",
        ask_again.text
    );
    c.plain_text(USERNAME).await;
    assert_eq!(c.step(), Some(2), "still waiting after the second 404");
    c.plain_text(USERNAME).await;
    assert_eq!(c.step(), Some(3), "username exists ⇒ advance");
    assert_eq!(c.pget("username").as_deref(), Some(USERNAME));

    c.press("done").await; // 3 → 4
    c.press("done").await; // 4 → 5
    c.press("done").await; // 5 → 6 (+7)
    assert_eq!(c.step(), Some(7));

    // Step 6 re-shows in place (prefilled token link); step 7's intake ask
    // is the fresh send that becomes the new anchor.
    let step6 = c.edits().last().unwrap().1.clone();
    assert!(
        step6.link_buttons.iter().any(|(_, url)| url == TOKEN_NEW_URL),
        "step 6 carries the prefilled token page: {:?}",
        step6.link_buttons
    );
    let sent = c.sent();
    let step7 = sent.last().unwrap();
    let (_, form_url) = step7
        .link_buttons
        .iter()
        .find(|(label, _)| label.contains("form"))
        .expect("step 7 form-mode carries the one-time form link");
    let nonce = form_url.split_once("nonce=").unwrap().1.to_string();
    assert_eq!(nonce.len(), 64, "fresh 32-byte hex nonce");
    assert_eq!(c.kv(KV_NONCE).as_deref(), Some(nonce.as_str()));
    assert_eq!(c.pget("nonce").as_deref(), Some(nonce.as_str()));
    assert!(
        c.kv(KV_ROTATE_IN_PROGRESS).is_none(),
        "setup intake clears any stale rotate marker"
    );

    // Step 7 success arrives through the one-time-form hook (the real
    // HTTP round trip is covered by the spawn_form_gate tests below).
    c.mount_verify(MACHINE_LOGIN, Some(MAIN_TOKEN)).await;
    {
        let guard = c.state.lock().unwrap();
        MachineTokenStore::new(&guard, ENC_KEY).store(MAIN_TOKEN).unwrap();
    }
    form_stored_hook(&c.state, &Some((c.api.clone(), CHAT))).await;
    assert_eq!(c.step(), Some(8), "first-setup form success advances to step 8");
    let sent = c.sent();
    let step8_ask = sent.last().unwrap();
    assert!(
        step8_ask.text.contains("owner/repo"),
        "step 8 asks for repos: {}",
        step8_ask.text
    );
    let checklist_ref = c.pget("msg").expect("step 8 ask becomes the checklist anchor");

    // Step 8: two repos, one granted, one pending — in-place updates.
    c.plain_text("acme/one\ncme/two").await;
    assert_eq!(
        c.grant.calls(),
        vec!["acme/one".to_string(), "cme/two".to_string()]
    );
    let anchor: MsgRef = {
        let (chat, msg) = checklist_ref.split_once(':').unwrap();
        MsgRef {
            chat_id: chat.parse().unwrap(),
            message_id: msg.parse().unwrap(),
        }
    };
    let checklist_edits: Vec<_> = c
        .edits()
        .into_iter()
        .filter(|(to, _)| *to == anchor)
        .collect();
    assert!(
        checklist_edits.len() >= 2,
        "checklist updated in place per repo: {} edits",
        checklist_edits.len()
    );
    let final_text = checklist_edits.last().unwrap().1.text.clone();
    assert!(final_text.contains("acme/one") && final_text.contains("✅"), "{final_text}");
    assert!(final_text.contains("cme/two") && final_text.contains("⏳"), "{final_text}");

    // Finish.
    c.press("finish").await;
    let sent = c.sent();
    let summary = sent.last().unwrap();
    let text = &summary.text;
    assert!(text.contains("/pending") && text.contains("/rotate"), "command list: {text}");
    assert!(
        text.to_lowercase().contains("ruleset") || text.to_lowercase().contains("bypass"),
        "ruleset backstop reminder present: {text}"
    );
    assert!(
        summary
            .link_buttons
            .iter()
            .any(|(_, url)| url.contains("gitauthgate-setup.md")),
        "finish links the setup guide: {:?}",
        summary.link_buttons
    );
    assert_eq!(c.step(), None, "finish clears the durable rows");
    assert_eq!(c.pget("main_email_hint"), None, "email hint never outlives setup");
}

// ---------------------------------------------------------------------------
// Step 2: repeat-ask without advancing; taken-username alternatives
// ---------------------------------------------------------------------------

#[tokio::test]
async fn username_wrong_until_exists_repeats_ask_and_never_advances() {
    let c = Ctl::open(false).await;
    c.mount_users_status(USERNAME, 404).await;
    c.command("/setup").await;
    c.press("start").await;
    c.press("done").await; // 1 → 2

    for _ in 0..3 {
        c.plain_text(USERNAME).await;
        assert_eq!(c.step(), Some(2), "no existence ⇒ no advance");
    }
    // Only the /setup intro + step edits happened; no step-3 message.
    assert!(
        c.sent().is_empty(),
        "no fresh sends while stuck on step 2: {:?}",
        c.sent()
    );
}

#[tokio::test]
async fn taken_username_suggests_agent1_and_bot_alternatives() {
    let c = Ctl::open(false).await;
    c.mount_users_status("robsmith", 200).await;
    c.mount_users_status("robsmith-agent-1", 404).await;
    c.mount_users_status("robsmith-bot", 404).await;
    c.command("/setup").await;
    c.press("start").await;
    c.press("done").await; // 1 → 2

    c.plain_text("robsmith").await;

    assert_eq!(c.step(), Some(2), "taken username ⇒ no advance");
    let reply = c.edits().last().unwrap().1.clone();
    assert!(
        reply.text.contains("robsmith-agent-1") && reply.text.contains("robsmith-bot"),
        "suggests the alternatives: {}",
        reply.text
    );
    assert_eq!(c.pget("username"), None, "taken username is never stored");
}

// ---------------------------------------------------------------------------
// Step 7 paste fallback (GATE_PUBLIC_URL unset)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn paste_wrong_login_names_the_login_returns_to_six_nonce_still_valid() {
    let c = Ctl::open(true).await;
    c.drive_to_intake(0).await;
    let nonce = c.kv(KV_NONCE).expect("intake nonce minted");
    let expires: i64 = c.kv(KV_NONCE_EXPIRES).unwrap().parse().unwrap();

    // Fallback mode: NO form link, paste instructions instead — the
    // intake ask is the fresh send (the step-6 re-show went as an edit).
    let sent = c.sent();
    let intake = sent.last().unwrap();
    assert!(
        intake.text.contains("Paste the key here as a message. I'll delete your message right away."),
        "fallback copy verbatim: {}",
        intake.text
    );
    assert!(
        intake.link_buttons.is_empty(),
        "no form link without a public URL: {:?}",
        intake.link_buttons
    );

    // Paste a main-account token: deleted instantly, wrong login named.
    c.mount_verify("robsmith", Some(MAIN_TOKEN)).await;
    c.plain_text(MAIN_TOKEN).await;

    assert_eq!(c.deletes().len(), 1, "the pasted message is deleted");
    let reply = c.edits().last().unwrap().1.clone();
    assert!(
        reply.text.contains("robsmith"),
        "wrong-login message names the login found: {}",
        reply.text
    );
    // The anchor was re-shown as step 6 (with the reason merged in); the
    // durable end state is 7 — the intake ask went out again right after.
    assert!(
        reply.text.contains("Token check failed") && reply.text.contains("Step 6 of 8"),
        "step 6 re-shown with the reason: {}",
        reply.text
    );
    assert_eq!(c.step(), Some(7), "intake ask re-sent (waiting for the retry)");
    // Same nonce — nothing re-minted, the original form link stays valid.
    assert_eq!(c.kv(KV_NONCE).as_deref(), Some(nonce.as_str()), "nonce untouched");
    let still_valid: i64 = c.kv(KV_NONCE_EXPIRES).unwrap().parse().unwrap();
    assert!(still_valid >= expires, "nonce expiry untouched");
    assert_eq!(c.custody_load(), None, "nothing stored on a wrong login");
}

#[tokio::test]
async fn paste_success_stores_token_enables_forwarding_and_advances() {
    let c = Ctl::open(true).await;
    c.drive_to_intake(0).await;
    c.mount_verify(MACHINE_LOGIN, Some(MAIN_TOKEN)).await;

    c.plain_text(MAIN_TOKEN).await;

    assert_eq!(c.deletes().len(), 1, "paste deleted instantly");
    assert_eq!(c.custody_load().as_deref(), Some(MAIN_TOKEN), "token stored");
    assert!(c.forwarding.load(Ordering::Relaxed), "verified store ⇒ forwarding on");
    assert_eq!(c.step(), Some(8), "first setup complete ⇒ repo checklist");
    assert!(
        c.sent().last().unwrap().text.contains("owner/repo"),
        "step-8 ask delivered: {:?}",
        c.sent().last().unwrap().text
    );
}

#[tokio::test]
async fn paste_delete_failure_is_reported_and_intake_continues() {
    let c = Ctl::open(true).await;
    c.drive_to_intake(0).await;
    c.fake.fail_next_deletes(1);
    c.mount_verify(MACHINE_LOGIN, Some(MAIN_TOKEN)).await;

    c.plain_text(MAIN_TOKEN).await;

    let told: Vec<String> = c
        .edits()
        .iter()
        .map(|(_, r)| r.text.clone())
        .chain(c.sent().iter().map(|r| r.text.clone()))
        .filter(|t| t.to_lowercase().contains("could not delete"))
        .collect();
    assert!(
        !told.is_empty(),
        "the user is told the message could not be deleted"
    );
    assert_eq!(c.custody_load().as_deref(), Some(MAIN_TOKEN), "intake continues");
    assert_eq!(c.step(), Some(8));
}

// ---------------------------------------------------------------------------
// Cancel + resume durability (Review Focus 1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_clears_rows_and_setup_starts_fresh_again() {
    let c = Ctl::open(false).await;
    c.mount_users_then_ok(USERNAME, 0).await;
    c.command("/setup").await;
    c.press("start").await;
    c.press("done").await; // step 2
    assert_eq!(c.step(), Some(2));

    c.press("cancel").await;

    assert_eq!(c.step(), None, "cancel clears the rows");
    assert_eq!(c.pget("username"), None);
    let ack = c.edits().last().unwrap().1.clone();
    assert!(
        ack.text.to_lowercase().contains("cancelled"),
        "cancel acknowledged: {}",
        ack.text
    );

    // Starting over begins at step 0 again.
    let fresh = c.command("/setup").await;
    assert_eq!(c.step(), Some(0));
    assert!(
        fresh.inline_buttons.iter().any(|(label, _)| label == "Start"),
        "fresh intro: {:?}",
        fresh.inline_buttons
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_resume_same_step_no_duplicate_under_an_hour_resend_over_an_hour() {
    let c = Ctl::open(false).await;
    c.drive_to_intake(0).await;
    c.press("cancel").await; // reset, then park mid-flow at step 3
    c.mount_users_then_ok(USERNAME, 0).await;
    c.command("/setup").await;
    c.press("start").await;
    c.press("done").await;
    c.plain_text(USERNAME).await;
    assert_eq!(c.step(), Some(3));
    let sends_before = c.sent().len();

    // Restart (fresh deps, same durable state), step row fresh (<1 h):
    // resume must NOT resend the step message.
    resume(&c.deps, c.api.clone()).await;
    assert_eq!(
        c.sent().len(),
        sends_before,
        "fresh step row ⇒ no duplicate message"
    );
    assert_eq!(c.step(), Some(3), "same step after restart");

    // Step row older than 1 h: resume resends the current step.
    let stale = state::now() - 7200;
    c.state
        .lock()
        .unwrap()
        .setup_set_with_ts("step", "3", stale)
        .unwrap();
    resume(&c.deps, c.api.clone()).await;
    let sent = c.sent();
    let resent = sent.last().unwrap();
    assert!(
        resent.text.contains("Step 3") || resent.text.contains("step 3"),
        "the current step is resent after 1 h: {}",
        resent.text
    );
    assert_eq!(c.step(), Some(3), "still at the same step");
}

// ---------------------------------------------------------------------------
// Step 8 checklist: one message, in-place updates per repo outcome
// ---------------------------------------------------------------------------

#[tokio::test]
async fn checklist_updates_in_place_with_ok_wait_and_fail_marks() {
    let grant = FakeGrantFlow::new()
        .with_outcome("acme/ok", GrantOutcome::Granted { role: "write".into() })
        .with_outcome(
            "acme/wait",
            GrantOutcome::Pending {
                detail: "invitation sent".into(),
            },
        )
        .with_outcome(
            "acme/bad",
            GrantOutcome::Failed {
                reason: "role_unavailable".into(),
            },
        );
    let c = Ctl::with_grant(false, grant).await;
    c.drive_to_intake(0).await;
    c.mount_verify(MACHINE_LOGIN, Some(MAIN_TOKEN)).await;
    c.plain_text(MAIN_TOKEN).await; // → step 8
    assert_eq!(c.step(), Some(8));

    c.plain_text("acme/ok\nacme/wait\nacme/bad").await;

    let anchor: MsgRef = {
        let stored = c.pget("msg").unwrap();
        let (chat, msg) = stored.split_once(':').unwrap();
        MsgRef {
            chat_id: chat.parse().unwrap(),
            message_id: msg.parse().unwrap(),
        }
    };
    let edits: Vec<_> = c
        .edits()
        .into_iter()
        .filter(|(to, _)| *to == anchor)
        .collect();
    assert!(
        edits.len() >= 3,
        "in-place update per repo (>=3 edits): {}",
        edits.len()
    );
    let final_text = edits.last().unwrap().1.text.clone();
    assert!(final_text.contains("✅"), "granted mark: {final_text}");
    assert!(final_text.contains("⏳"), "pending mark: {final_text}");
    assert!(final_text.contains("❌"), "failed mark: {final_text}");
    assert!(final_text.contains("role_unavailable"), "failure detail: {final_text}");
}

// ---------------------------------------------------------------------------
// Signed callbacks end-to-end: rendered presses through the real router
// (review round 1, F1 ruling 4)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_nonce_bounces_at_the_router_and_a_rendered_press_advances() {
    let c = Ctl::open(false).await;
    let intro = c.command("/setup").await;
    assert_eq!(c.step(), Some(0));
    // The rendered [Start] button carries the SHORT cb nonce, within the
    // 64-byte callback_data cap (echoed verbatim by the harness).
    let (_, start_data) = intro
        .inline_buttons
        .iter()
        .find(|(label, _)| label == "Start")
        .expect("rendered [Start] button");
    assert!(
        start_data.len() <= 64,
        "callback_data must fit Telegram's limit: {start_data}"
    );
    let cb_nonce = start_data
        .split(':')
        .nth(1)
        .expect("signed triple")
        .to_string();
    assert_eq!(cb_nonce.len(), 16, "buttons sign the SHORT cb nonce");
    assert_eq!(start_data, &format!("setup:{cb_nonce}:start"));

    // Wrong-nonce press: the ROUTER rejects before any handler runs —
    // one "no longer valid" edit, no advance, spinner answered.
    let edits_before = c.fake.edits().len();
    let forged = Update {
        sender_id: Some(CHAT),
        callback: Some(gate::telegram::Callback {
            id: "cbq-forged".to_string(),
            data: "setup:0000000000000000:start".to_string(),
            from_id: CHAT,
        }),
        message_ref: Some(MsgRef {
            chat_id: CHAT,
            message_id: 77,
        }),
        ..Default::default()
    };
    c.router.dispatch(&forged, &c.api).await;
    assert_eq!(c.step(), Some(0), "forged nonce ⇒ no advance");
    let edits = c.fake.edits();
    assert_eq!(edits.len(), edits_before + 1, "exactly the bounce edit");
    assert_eq!(
        edits.last().unwrap().1.text,
        gate::telegram::ACTION_NO_LONGER_VALID
    );
    assert!(c.fake.answered().contains(&"cbq-forged".to_string()));

    // Happy path: the RENDERED button, through the same router dispatch.
    c.press("start").await;
    assert_eq!(c.step(), Some(1), "rendered button advances end-to-end");
    assert!(
        c.fake.answered().contains(&"cbq-1".to_string()),
        "spinner stopped on the valid press"
    );
}

// ---------------------------------------------------------------------------
// [I'm stuck]
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stuck_resends_with_extended_detail() {
    let c = Ctl::open(false).await;
    c.command("/setup").await;
    c.press("start").await;
    let base = c.edits().last().unwrap().1.text.clone();

    c.press("stuck").await;

    let stuck = c.edits().last().unwrap().1.clone();
    assert!(
        stuck.text.len() > base.len(),
        "extended detail is longer: {} vs {}",
        stuck.text.len(),
        base.len()
    );
    assert!(
        stuck.text.to_lowercase().contains("common problems"),
        "stuck adds common problems: {}",
        stuck.text
    );
    assert_eq!(c.step(), Some(1), "stuck does not advance");
}

// ---------------------------------------------------------------------------
// One-time form (HTTP) — first setup advances, rotation does not
// ---------------------------------------------------------------------------

struct FormGate {
    port: u16,
    dir: PathBuf,
    state: Arc<Mutex<DbState>>,
    upstream: MockServer,
    fake: Arc<FakeTelegram>,
    api: Arc<dyn TelegramApi>,
    forwarding: Arc<AtomicBool>,
    router: TelegramRouter,
}

impl Drop for FormGate {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn spawn_form_gate() -> FormGate {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gate-setup-form-{}-{nanos}", std::process::id()));
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
    let fake = Arc::new(FakeTelegram::new());
    let api: Arc<dyn TelegramApi> = fake.clone();
    let notify = Some((api.clone(), CHAT));
    let grant = Arc::new(FakeGrantFlow::new());
    let deps = Arc::new(SetupDeps {
        state: state.clone(),
        platform: platform.clone(),
        machine_login: MACHINE_LOGIN.to_string(),
        enc_key: ENC_KEY.to_string(),
        gate_public_url: HOST.to_string(),
        chat_id: CHAT,
        forwarding: forwarding.clone(),
        grant: grant.clone(),
        audit: audit.clone(),
    });
    let gs = GateState {
        state: state.clone(),
        audit: audit.clone(),
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
        telegram_live: std::sync::atomic::AtomicBool::new(false),
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
    let machine_token: Arc<dyn Fn() -> Option<String> + Send + Sync> = {
        let state = state.clone();
        let enc = ENC_KEY.to_string();
        Arc::new(move || {
            let guard = state.lock().unwrap();
            MachineTokenStore::new(&guard, &enc).load().unwrap_or(None)
        })
    };
    let mut router = TelegramRouter::new();
    register_commands(
        &mut router,
        Arc::new(CommandDeps {
            state: state.clone(),
            audit,
            machine_token,
            lifecycle: None,
            grant: None,
        }),
    );
    register_setup(&mut router, deps.clone());
    let approvals = gate::telegram::approvals::TelegramApprovalBackend::nonce_lookup(state.clone());
    let rotate = rotate_nonce_lookup(state.clone());
    let setup = setup_nonce_lookup(state.clone());
    router.set_nonce_lookup(Arc::new(move |flow| {
        approvals(flow).or_else(|| rotate(flow)).or_else(|| setup(flow))
    }));
    FormGate {
        port,
        dir,
        state,
        upstream,
        fake,
        api,
        forwarding,
        router,
    }
}

impl FormGate {
    fn step(&self) -> Option<u8> {
        self.state
            .lock()
            .unwrap()
            .setup_get("step")
            .unwrap()
            .and_then(|v| v.parse().ok())
    }

    fn custody_load(&self) -> Option<String> {
        let guard = self.state.lock().unwrap();
        MachineTokenStore::new(&guard, ENC_KEY).load().unwrap()
    }

    async fn command(&self, text: &str) {
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
    }

    async fn press(&self, action: &str) {
        // Review round 1 (F1): echo a RENDERED button's callback_data —
        // never reconstruct the signed triple from state.
        let wanted = |data: &str| {
            data.split(':').next() == Some(gate::telegram::setup::SETUP_FLOW)
                && data.rsplit(':').next() == Some(action)
        };
        let data = self
            .fake
            .edits()
            .iter()
            .rev()
            .find_map(|(_, req)| req.inline_buttons.iter().find(|(_, d)| wanted(d)).map(|(_, d)| d.clone()))
            .or_else(|| {
                self.fake
                    .sent()
                    .iter()
                    .rev()
                    .find_map(|req| req.inline_buttons.iter().find(|(_, d)| wanted(d)).map(|(_, d)| d.clone()))
            })
            .unwrap_or_else(|| panic!("no rendered setup button for action '{action}'"));
        let update = Update {
            sender_id: Some(CHAT),
            callback: Some(gate::telegram::Callback {
                id: "cbq-1".to_string(),
                data,
                from_id: CHAT,
            }),
            message_ref: Some(MsgRef {
                chat_id: CHAT,
                message_id: 77,
            }),
            ..Default::default()
        };
        self.router.dispatch(&update, &self.api).await;
    }

    async fn plain_text(&self, text: &str) {
        let update = Update {
            sender_id: Some(CHAT),
            text: Some(text.to_string()),
            message_ref: Some(MsgRef {
                chat_id: CHAT,
                message_id: 950,
            }),
            ..Default::default()
        };
        self.router.dispatch(&update, &self.api).await;
    }

    /// Drive to the intake step, return the form nonce from the link.
    async fn drive_to_intake(&self) -> String {
        Mock::given(method("GET"))
            .and(path(format!("/users/{USERNAME}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(format!(
                    r#"{{"login":"{USERNAME}","id":4242}}"#
                )),
            )
            .mount(&self.upstream)
            .await;
        self.command("/setup").await;
        self.press("start").await;
        self.plain_text("rob@example.com").await;
        self.press("done").await;
        self.plain_text(USERNAME).await;
        self.press("done").await;
        self.press("done").await;
        self.press("done").await;
        assert_eq!(self.step(), Some(7));
        // The step-7 message is a fresh send carrying the form link.
        let sent = self.fake.sent();
        let req = sent
            .iter()
            .rev()
            .find(|r| r.link_buttons.iter().any(|(l, _)| l.contains("form")))
            .expect("step-7 intake message sent");
        req.link_buttons
            .iter()
            .find(|(_, url)| url.contains("nonce="))
            .expect("form link")
            .1
            .split_once("nonce=")
            .unwrap()
            .1
            .to_string()
    }

    async fn post_form(&self, nonce: &str, token: &str) -> reqwest::StatusCode {
        reqwest::Client::new()
            .post(format!(
                "http://127.0.0.1:{}/setup/token?nonce={nonce}",
                self.port
            ))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("token={token}"))
            .send()
            .await
            .unwrap()
            .status()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn form_success_first_setup_advances_to_step_eight_over_http() {
    let g = spawn_form_gate().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("authorization", format!("Bearer {MAIN_TOKEN}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!(
                    r#"{{"login":"{MACHINE_LOGIN}","id":4242,"two_factor_authentication":true}}"#
                ))
                .append_header("X-OAuth-Scopes", "repo, workflow, read:org"),
        )
        .mount(&g.upstream)
        .await;

    let nonce = g.drive_to_intake().await;
    let status = g.post_form(&nonce, MAIN_TOKEN).await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(g.custody_load().as_deref(), Some(MAIN_TOKEN), "form stored the token");
    assert!(g.forwarding.load(Ordering::Relaxed), "forwarding on after store");
    assert_eq!(g.step(), Some(8), "hook advanced the first setup to step 8");
    let sent = g.fake.sent();
    let asks: Vec<_> = sent.iter().filter(|r| r.text.contains("owner/repo")).collect();
    assert_eq!(asks.len(), 1, "exactly one step-8 ask: {:?}", sent);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn form_success_during_rotation_does_not_advance_the_setup_flow() {
    let g = spawn_form_gate().await;
    // A pending ROTATION owns the form nonce.
    let nonce = mint_setup_nonce(&g.state.lock().unwrap()).unwrap();
    g.state
        .lock()
        .unwrap()
        .kv_set(KV_ROTATE_IN_PROGRESS, &nonce)
        .unwrap();
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("authorization", format!("Bearer {MAIN_TOKEN}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!(
                    r#"{{"login":"{MACHINE_LOGIN}","id":4242,"two_factor_authentication":true}}"#
                ))
                .append_header("X-OAuth-Scopes", "repo, workflow, read:org"),
        )
        .mount(&g.upstream)
        .await;
    // An active setup flow parked early (never reached the intake).
    g.command("/setup").await;
    g.press("start").await;
    g.press("done").await;
    assert_eq!(g.step(), Some(2));

    let status = g.post_form(&nonce, MAIN_TOKEN).await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(g.step(), Some(2), "rotation completion leaves the setup flow alone");
    assert!(
        g.fake
            .sent()
            .iter()
            .any(|r| r.text.contains("ROTATED")),
        "rotate-complete notice went out instead: {:?}",
        g.fake.sent()
    );
    assert!(
        !g.fake
            .sent()
            .iter()
            .any(|r| r.text.contains("owner/repo")),
        "no step-8 ask on a rotation"
    );
}

// ---------------------------------------------------------------------------
// Copy-deck pins (GATE-011 §"The eight steps")
// ---------------------------------------------------------------------------

#[tokio::test]
async fn step_messages_carry_the_adr_buttons_and_links() {
    let c = Ctl::open(false).await;
    c.mount_users_then_ok(USERNAME, 0).await;
    let intro = c.command("/setup").await;
    assert!(intro.text.contains("10 minutes"), "intro gives the estimate: {}", intro.text);

    c.press("start").await;
    let step1 = c.edits().last().unwrap().1.clone();
    assert!(
        step1
            .inline_buttons
            .iter()
            .any(|(label, _)| label.contains("Done")),
        "step 1 carries a Done-style button: {:?}",
        step1.inline_buttons
    );
    c.press("done").await;
    let step2 = c.edits().last().unwrap().1.clone();
    assert!(
        step2
            .inline_buttons
            .iter()
            .any(|(label, _)| label.contains("Check again")),
        "step 2 carries the retry button: {:?}",
        step2.inline_buttons
    );
    assert!(
        step2.link_buttons.iter().any(|(_, url)| url.contains("github.com/signup")),
        "step 2 links the signup page: {:?}",
        step2.link_buttons
    );
    c.plain_text(USERNAME).await; // → 3
    c.press("done").await; // → 4
    let step4 = c.edits().last().unwrap().1.clone();
    assert!(
        step4.text.contains("Keep my email addresses private")
            && step4.text.contains("Block command line pushes that expose my email"),
        "step 4 names both checkboxes bold: {}",
        step4.text
    );
    c.press("done").await; // → 5
    let step5 = c.edits().last().unwrap().1.clone();
    assert!(
        step5.text.contains("two-factor authentication")
            || step5.text.contains("Two-factor authentication"),
        "step 5 is 2FA: {}",
        step5.text
    );
    c.press("done").await; // → 6+7
    // Step 6 re-shows as an edit; step 7's intake is the fresh send.
    let step6 = c.edits().last().unwrap().1.clone();
    assert!(
        step6.text.contains("14, 3 and 1"),
        "step 6 promises the 14/3/1-day reminders: {}",
        step6.text
    );
    let sent = c.sent();
    let step7 = sent.last().unwrap();
    assert!(
        step7
            .inline_buttons
            .iter()
            .any(|(label, _)| label == "I'm stuck")
            && step7
                .inline_buttons
                .iter()
                .any(|(label, _)| label == "Cancel setup"),
        "intake step carries stuck + cancel: {:?}",
        step7.inline_buttons
    );
    // Reminder constants still pinned (step-6 copy promises them).
    assert_eq!(REMINDER_DAYS, [14, 3, 1]);
}
