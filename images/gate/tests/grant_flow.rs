//! Integration: repo grant flow (GATE-010 §7.2, task 7). The real grant
//! machinery behind `/grant`, setup step 8 and the proxy's grant offer:
//!
//! - invite appears after 2 polls ⇒ PATCH called with the right id ⇒ role
//!   confirmed ⇒ Granted outcome + registry row via collaborator;
//! - user picks a lower role (permissions answer Triage) ⇒ fix message
//!   names both roles, polling continues until Write;
//! - no invitation within the give-up window ⇒ give-up note, Failed;
//! - one invitation reminder after the reminder delay, exactly once;
//! - personal vs org instructions wording (GATE-010), seat note once per
//!   org (kv `seat_note:{org}`);
//! - grant-offer debounce: two triggering requests the same day ⇒ one
//!   message (`kv grant_offer:{repo}:{date}`);
//! - setup step 8 with the REAL flow (wiremock-backed) ⇒ ✅ in checklist;
//! - `/grant owner/repo [role]` runs the same flow end-to-end.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use gate::audit::Audit;
use gate::github::{GitHubPlatform, Platform};
use gate::state::State as DbState;
use gate::telegram::commands::{register_commands, CommandDeps};
use gate::telegram::fake::FakeTelegram;
use gate::telegram::grant::{
    grant_offer_key, maybe_offer_grant, start_grant, GrantDeps, RealGrantFlow,
};
use gate::telegram::setup::{register_setup, GrantOutcome, NotifyHandle, SetupDeps};
use gate::telegram::{MsgRef, TelegramApi, TelegramRouter, Update};
use gate::types::Role;

const LOGIN: &str = "rob-agent";
const CHAT: i64 = 4242;
const REPO: &str = "acme/api";
const ENC_KEY: &str = "grant-test-enc-key";

// ---------------------------------------------------------------------------
// Harness: in-memory state + FakeTelegram + wiremock upstream + GrantDeps
// with test-scale durations.
// ---------------------------------------------------------------------------

struct H {
    dir: PathBuf,
    state: Arc<Mutex<DbState>>,
    fake: Arc<FakeTelegram>,
    api: Arc<dyn TelegramApi>,
    upstream: MockServer,
    deps: GrantDeps,
}

impl Drop for H {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl H {
    async fn open() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("gate-grant-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _audit = Arc::new(Audit::open(&dir.join("audit.jsonl")));
        let state = Arc::new(Mutex::new(DbState::open(":memory:").unwrap()));
        let upstream = MockServer::start().await;
        let base = reqwest::Url::parse(&upstream.uri()).unwrap();
        let platform: Arc<dyn Platform> = Arc::new(GitHubPlatform::with_bases(
            base.clone(),
            base,
            Arc::new(|| None),
        ));
        let fake = Arc::new(FakeTelegram::new());
        let api: Arc<dyn TelegramApi> = fake.clone();
        let deps = GrantDeps {
            state: state.clone(),
            platform,
            machine_login: LOGIN.to_string(),
            poll: Duration::from_millis(10),
            reminder_after: Duration::from_secs(3600),
            give_up_after: Duration::from_secs(30),
        };
        Self {
            dir,
            state,
            fake,
            api,
            upstream,
            deps,
        }
    }

    fn notify(&self) -> NotifyHandle {
        NotifyHandle {
            api: self.api.clone(),
            chat_id: CHAT,
        }
    }

    fn sent_with(&self, needle: &str) -> Vec<gate::telegram::send::SendRequest> {
        self.fake
            .sent()
            .iter()
            .filter(|r| r.text.contains(needle))
            .cloned()
            .collect()
    }

    fn row(&self, repo: &str) -> gate::state::RegistryRow {
        self.state
            .lock()
            .unwrap()
            .registry_get(repo)
            .expect("registry read")
            .expect("row present")
    }

    fn kv(&self, key: &str) -> Option<String> {
        self.state.lock().unwrap().kv_get(key).unwrap()
    }

    /// `GET /orgs/acme`: 404 (personal / invisible) unless a body given.
    async fn mount_org(&self, status: u16, body: &str) {
        Mock::given(method("GET"))
            .and(path("/orgs/acme"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&self.upstream)
            .await;
    }

    async fn mount_org_personal(&self) {
        self.mount_org(404, r#"{"message":"Not Found"}"#).await;
    }

    /// `GET /user/repository_invitations`: empty for the first `misses`
    /// calls, then a single invitation for `invite_repo` with `invite_id`.
    async fn mount_invitations_then(&self, misses: usize, invite_repo: &str, invite_id: i64) {
        let counter = Arc::new(AtomicUsize::new(0));
        let after = format!(
            r#"[{{"id":{invite_id},"repository":{{"full_name":"{invite_repo}"}}}}]"#
        );
        Mock::given(method("GET"))
            .and(path("/user/repository_invitations"))
            .respond_with(move |_: &wiremock::Request| {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n < misses {
                    ResponseTemplate::new(200).set_body_string("[]")
                } else {
                    ResponseTemplate::new(200).set_body_string(after.clone())
                }
            })
            .mount(&self.upstream)
            .await;
    }

    /// `GET /user/repository_invitations`: always empty — no invite ever.
    async fn mount_invitations_never(&self) {
        Mock::given(method("GET"))
            .and(path("/user/repository_invitations"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .mount(&self.upstream)
            .await;
    }

    /// `PATCH /user/repository_invitations/{id}`: 204.
    async fn mount_accept(&self, id: i64) {
        Mock::given(method("PATCH"))
            .and(path(format!("/user/repository_invitations/{id}")))
            .respond_with(ResponseTemplate::new(204))
            .mount(&self.upstream)
            .await;
    }

    /// `GET /repos/acme/api`: `lower_body` for the first `lower_calls`
    /// calls, then `final_body` — the wrong-role-then-fixed sequence.
    async fn mount_repo_role_then(&self, lower_calls: usize, lower: Role, final_role: Role) {
        let counter = Arc::new(AtomicUsize::new(0));
        let lower_body = permissions_body(lower);
        let final_body = permissions_body(final_role);
        Mock::given(method("GET"))
            .and(path("/repos/acme/api"))
            .respond_with(move |_: &wiremock::Request| {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n < lower_calls {
                    ResponseTemplate::new(200).set_body_string(lower_body.clone())
                } else {
                    ResponseTemplate::new(200).set_body_string(final_body.clone())
                }
            })
            .mount(&self.upstream)
            .await;
    }

    async fn mount_repo_role(&self, role: Role) {
        let body = permissions_body(role);
        Mock::given(method("GET"))
            .and(path("/repos/acme/api"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&self.upstream)
            .await;
    }

    /// PATCH + invitation-list requests recorded upstream, filtered by path.
    async fn upstream_patches(&self, id: i64) -> usize {
        let want = format!("/user/repository_invitations/{id}");
        let requests = self.upstream.received_requests().await.expect("recorded");
        requests
            .iter()
            .filter(|r| r.method == "PATCH" && r.url.path() == want)
            .count()
    }
}

/// Minimal-but-valid `permissions` object for a role (registry test shape).
fn permissions_body(role: Role) -> String {
    let (admin, maintain, push, triage, pull) = match role {
        Role::Admin => (true, true, true, true, true),
        Role::Maintain => (false, true, true, true, true),
        Role::Write => (false, false, true, true, true),
        Role::Triage => (false, false, false, true, true),
        Role::Read => (false, false, false, false, true),
        Role::None => (false, false, false, false, false),
    };
    format!(
        r#"{{"private":false,"permissions":{{"admin":{admin},"maintain":{maintain},"push":{push},"triage":{triage},"pull":{pull}}}}}"#
    )
}

// ---------------------------------------------------------------------------
// Happy path: invite after 2 polls ⇒ PATCH ⇒ role confirmed ⇒ registry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invite_after_two_polls_patch_then_role_confirmed_with_registry_row() {
    let h = H::open().await;
    h.mount_org_personal().await;
    h.mount_invitations_then(1, REPO, 7).await; // poll 1: [], poll 2: invite
    h.mount_accept(7).await;
    h.mount_repo_role(Role::Write).await;

    let outcome = start_grant(&h.deps, REPO, Role::Write, &h.notify()).await;

    assert_eq!(
        outcome,
        GrantOutcome::Granted {
            role: "write".to_string()
        }
    );
    assert_eq!(h.upstream_patches(7).await, 1, "PATCH called with id 7");
    let row = h.row(REPO);
    assert_eq!(row.granted_role, "write");
    assert_eq!(row.intended_role, "write");
    assert_eq!(row.via, "collaborator");

    // Instructions: ONE link button to the collaborator settings page.
    let sent = h.fake.sent();
    let instructions = sent.first().expect("instructions sent first");
    assert!(
        instructions.text.contains(LOGIN),
        "instructions name the invitee: {}",
        instructions.text
    );
    assert_eq!(
        instructions.link_buttons,
        vec![(
            "Open collaborator settings".to_string(),
            "https://github.com/acme/api/settings/access".to_string()
        )],
        "exactly the one link button: {:?}",
        instructions.link_buttons
    );
    // Personal repo: no seat note (org invisible), no role choice wording.
    assert!(h.sent_with("seat").is_empty(), "no seat note on personal");
}

// ---------------------------------------------------------------------------
// Wrong lower role: fix message names both roles, polling continues
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lower_role_fix_message_names_both_roles_and_keeps_polling() {
    let h = H::open().await;
    h.mount_org_personal().await;
    h.mount_invitations_then(0, REPO, 8).await;
    h.mount_accept(8).await;
    // Triage twice, then the user fixes it to Write.
    h.mount_repo_role_then(2, Role::Triage, Role::Write).await;

    let outcome = start_grant(&h.deps, REPO, Role::Write, &h.notify()).await;

    assert_eq!(
        outcome,
        GrantOutcome::Granted {
            role: "write".to_string()
        },
        "polling continues until the role matches"
    );
    let fixes = h.sent_with("was added with");
    assert_eq!(fixes.len(), 1, "one fix per distinct wrong role");
    assert!(
        fixes[0].text.contains("rob-agent was added with Triage, but it needs Write"),
        "the fix message names both roles verbatim: {}",
        fixes[0].text
    );
    assert_eq!(h.row(REPO).granted_role, "write");
}

// ---------------------------------------------------------------------------
// Give-up: no invitation inside the window
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_invitation_within_give_up_window_sends_give_up_note() {
    let h = H::open().await;
    h.mount_org_personal().await;
    h.mount_invitations_never().await;
    let mut deps = h.deps.clone();
    deps.give_up_after = Duration::from_millis(60);
    deps.poll = Duration::from_millis(10);

    let outcome = start_grant(&deps, REPO, Role::Write, &h.notify()).await;

    assert!(
        matches!(outcome, GrantOutcome::Failed { .. }),
        "give up ⇒ Failed: {outcome:?}"
    );
    let notes = h.sent_with("Giving up");
    assert_eq!(notes.len(), 1, "exactly one give-up note");
    assert!(
        notes[0].text.contains("7 days") && notes[0].text.contains("/grant"),
        "the note explains the 7-day expiry and the re-run: {}",
        notes[0].text
    );
    assert!(
        h.state
            .lock()
            .unwrap()
            .registry_get(REPO)
            .unwrap()
            .is_none(),
        "a give-up grants nothing to the registry"
    );
}

// ---------------------------------------------------------------------------
// Reminder: one, after the reminder delay
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invitation_reminder_fires_once_after_the_delay() {
    let h = H::open().await;
    h.mount_org_personal().await;
    h.mount_invitations_never().await;
    let mut deps = h.deps.clone();
    deps.reminder_after = Duration::from_millis(30);
    deps.give_up_after = Duration::from_millis(150);
    deps.poll = Duration::from_millis(10);

    let outcome = start_grant(&deps, REPO, Role::Write, &h.notify()).await;

    assert!(matches!(outcome, GrantOutcome::Failed { .. }));
    let reminders = h.sent_with("Still waiting");
    assert_eq!(reminders.len(), 1, "ONE reminder, ever: {:?}", h.fake.sent());
    assert!(
        reminders[0].text.contains(LOGIN) && reminders[0].text.contains(REPO),
        "reminder names the repo and the invitee: {}",
        reminders[0].text
    );
}

// ---------------------------------------------------------------------------
// Grant-offer debounce (proxy trigger, GATE-010)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn offer_debounce_two_triggers_same_day_one_message() {
    let h = H::open().await;
    let notify = Some((h.api.clone(), CHAT));

    let first = maybe_offer_grant(&h.state, &notify, REPO, LOGIN).await;
    let second = maybe_offer_grant(&h.state, &notify, REPO, LOGIN).await;

    assert!(first, "first trigger offers");
    assert!(!second, "same-day second trigger is debounced");
    assert_eq!(
        h.fake.sent().len(),
        1,
        "exactly one offer message: {:?}",
        h.fake.sent()
    );
    assert!(
        h.fake.sent()[0].text.contains(REPO) && h.fake.sent()[0].text.contains("/grant"),
        "the offer names the repo and the /grant escape hatch: {}",
        h.fake.sent()[0].text
    );
    let day = gate::state::now().div_euclid(86_400);
    assert_eq!(
        h.kv(&grant_offer_key(REPO, day)).as_deref(),
        Some("1"),
        "the daily marker is persisted"
    );
    // A different repo has its own bucket.
    let third = maybe_offer_grant(&h.state, &notify, "acme/other", LOGIN).await;
    assert!(third, "debounce is per repo");
}

// ---------------------------------------------------------------------------
// Instructions: org wording + role choice; seat note once per org
// ---------------------------------------------------------------------------

#[tokio::test]
async fn org_repo_instructions_name_the_role_and_seat_note_fires_once_per_org() {
    let h = H::open().await;
    h.mount_org(200, r#"{"login":"acme","plan":{"name":"team"}}"#).await;
    h.mount_invitations_never().await;
    let mut deps = h.deps.clone();
    deps.give_up_after = Duration::from_millis(50);
    deps.poll = Duration::from_millis(10);

    let first = start_grant(&deps, "acme/one", Role::Write, &h.notify()).await;
    let second = start_grant(&deps, "acme/two", Role::Write, &h.notify()).await;

    assert!(matches!(first, GrantOutcome::Failed { .. }));
    assert!(matches!(second, GrantOutcome::Failed { .. }));
    let instructions = h.sent_with("Granting acme/");
    assert_eq!(instructions.len(), 2, "instructions per grant");
    for (text, repo) in instructions.iter().map(|r| &r.text).zip(["acme/one", "acme/two"]) {
        assert!(
            text.contains(&format!("Granting {repo} to {LOGIN}")),
            "instructions name repo + invitee: {text}"
        );
        assert!(
            text.contains("Write") && text.contains("outside collaborator"),
            "org instructions name the intended role: {text}"
        );
    }
    let seat = h.sent_with("paid GitHub plan");
    assert_eq!(seat.len(), 1, "seat note ONCE per org (kv seat_note:acme)");
    assert_eq!(h.kv("seat_note:acme").as_deref(), Some("1"));
}

#[tokio::test]
async fn personal_repo_instructions_say_every_collaborator_gets_push_access() {
    let h = H::open().await;
    h.mount_org_personal().await;
    h.mount_invitations_never().await;
    let mut deps = h.deps.clone();
    deps.give_up_after = Duration::from_millis(40);
    deps.poll = Duration::from_millis(10);

    let _ = start_grant(&deps, REPO, Role::Write, &h.notify()).await;

    let instructions = h.sent_with("Granting acme/api");
    assert_eq!(instructions.len(), 1);
    assert!(
        instructions[0].text.contains("every collaborator gets push access"),
        "personal wording pinned: {}",
        instructions[0].text
    );
    assert!(
        !instructions[0].text.contains("outside collaborator"),
        "no org wording on a personal repo: {}",
        instructions[0].text
    );
}

// ---------------------------------------------------------------------------
// Setup step 8 end-to-end with the REAL grant flow (the seam wiring)
// ---------------------------------------------------------------------------

struct SetupH {
    dir: PathBuf,
    state: Arc<Mutex<DbState>>,
    fake: Arc<FakeTelegram>,
    api: Arc<dyn TelegramApi>,
    upstream: MockServer,
    router: TelegramRouter,
}

impl Drop for SetupH {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn spawn_setup_with_real_grant() -> SetupH {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "gate-grant-setup-{}-{nanos}",
        std::process::id()
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
    let fake = Arc::new(FakeTelegram::new());
    let api: Arc<dyn TelegramApi> = fake.clone();
    let grant_deps = GrantDeps {
        state: state.clone(),
        platform: platform.clone(),
        machine_login: LOGIN.to_string(),
        poll: Duration::from_millis(10),
        reminder_after: Duration::from_secs(3600),
        give_up_after: Duration::from_secs(30),
    };
    let grant = Arc::new(RealGrantFlow::new(grant_deps));
    let deps = Arc::new(SetupDeps {
        state: state.clone(),
        platform,
        machine_login: LOGIN.to_string(),
        enc_key: ENC_KEY.to_string(),
        gate_public_url: String::new(),
        chat_id: CHAT,
        forwarding: Arc::new(AtomicBool::new(false)),
        grant,
        audit,
    });
    let mut router = TelegramRouter::new();
    register_setup(&mut router, deps);
    SetupH {
        dir,
        state,
        fake,
        api,
        upstream,
        router,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_step_eight_with_real_grant_shows_the_check_mark() {
    let h = spawn_setup_with_real_grant().await;
    // Personal repo, invite ready immediately, Write confirmed.
    Mock::given(method("GET"))
        .and(path("/orgs/acme"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&h.upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/user/repository_invitations"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                r#"[{"id":9,"repository":{"full_name":"acme/api"}}]"#,
            ),
        )
        .mount(&h.upstream)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/user/repository_invitations/9"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&h.upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/api"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(permissions_body(Role::Write)),
        )
        .mount(&h.upstream)
        .await;
    // Park the walkthrough at step 8 with a checklist anchor.
    {
        let guard = h.state.lock().unwrap();
        guard.setup_set("step", "8").unwrap();
        guard.setup_set("msg", &format!("{CHAT}:77")).unwrap();
    }

    let update = Update {
        sender_id: Some(CHAT),
        text: Some(REPO.to_string()),
        message_ref: Some(MsgRef {
            chat_id: CHAT,
            message_id: 999,
        }),
        ..Default::default()
    };
    h.router.dispatch(&update, &h.api).await;

    // The instructions went out as a fresh send; the checklist anchor was
    // edited in place from ⏳ to ✅.
    assert!(
        h.fake.sent().iter().any(|r| r.text.contains("Granting acme/api")),
        "instructions sent: {:?}",
        h.fake.sent()
    );
    let anchor = MsgRef {
        chat_id: CHAT,
        message_id: 77,
    };
    let edits: Vec<_> = h
        .fake
        .edits()
        .into_iter()
        .filter(|(to, _)| *to == anchor)
        .collect();
    assert!(!edits.is_empty(), "checklist edited in place");
    let final_text = edits.last().unwrap().1.text.clone();
    assert!(
        final_text.contains("acme/api ✅ granted (write)"),
        "the checklist shows the grant: {final_text}"
    );
    let row = h
        .state
        .lock()
        .unwrap()
        .registry_get(REPO)
        .unwrap()
        .expect("registry row written by the grant flow");
    assert_eq!(row.granted_role, "write");
    assert_eq!(row.via, "collaborator");
}

// ---------------------------------------------------------------------------
// /grant command: same flow, immediate ack, outcome message
// ---------------------------------------------------------------------------

struct CmdH {
    dir: PathBuf,
    state: Arc<Mutex<DbState>>,
    fake: Arc<FakeTelegram>,
    api: Arc<dyn TelegramApi>,
    upstream: MockServer,
    router: TelegramRouter,
}

impl Drop for CmdH {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn spawn_cmd_gate() -> CmdH {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gate-grant-cmd-{}-{nanos}", std::process::id()));
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
    let fake = Arc::new(FakeTelegram::new());
    let api: Arc<dyn TelegramApi> = fake.clone();
    let grant_deps = GrantDeps {
        state: state.clone(),
        platform,
        machine_login: LOGIN.to_string(),
        poll: Duration::from_millis(10),
        reminder_after: Duration::from_secs(3600),
        give_up_after: Duration::from_secs(30),
    };
    let grant = Arc::new(RealGrantFlow::new(grant_deps));
    let machine_token: Arc<dyn Fn() -> Option<String> + Send + Sync> = Arc::new(|| None);
    let mut router = TelegramRouter::new();
    register_commands(
        &mut router,
        Arc::new(CommandDeps {
            state: state.clone(),
            audit,
            machine_token,
            lifecycle: None,
            grant: Some((grant, CHAT)),
        }),
    );
    CmdH {
        dir,
        state,
        fake,
        api,
        upstream,
        router,
    }
}

impl CmdH {
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

    async fn wait_for_sent(&self, needle: &str) -> Option<gate::telegram::send::SendRequest> {
        for _ in 0..600 {
            if let Some(r) = self
                .fake
                .sent()
                .iter()
                .find(|r| r.text.contains(needle))
                .cloned()
            {
                return Some(r);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        None
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grant_command_runs_the_flow_and_reports_the_outcome() {
    let h = spawn_cmd_gate().await;
    Mock::given(method("GET"))
        .and(path("/orgs/acme"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&h.upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/user/repository_invitations"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                r#"[{"id":5,"repository":{"full_name":"acme/api"}}]"#,
            ),
        )
        .mount(&h.upstream)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/user/repository_invitations/5"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&h.upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/api"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(permissions_body(Role::Write)),
        )
        .mount(&h.upstream)
        .await;

    let reply = h.command(&format!("/grant {REPO}")).await;
    assert!(
        reply.text.contains("Grant flow started for acme/api"),
        "immediate ack: {}",
        reply.text
    );
    // The flow runs in the background: instructions + outcome message.
    let outcome = h.wait_for_sent("✅ acme/api").await.expect("outcome message");
    assert!(
        outcome.text.contains("write"),
        "outcome names the confirmed role: {}",
        outcome.text
    );
    let row = h
        .state
        .lock()
        .unwrap()
        .registry_get(REPO)
        .unwrap()
        .expect("registry row");
    assert_eq!(row.granted_role, "write");
    assert_eq!(row.via, "collaborator");
}

#[tokio::test]
async fn grant_command_usage_and_validation() {
    let h = spawn_cmd_gate().await;

    let bare = h.command("/grant").await;
    assert!(
        bare.text.contains("Usage: /grant owner/repo [role]"),
        "bare /grant answers usage: {}",
        bare.text
    );
    let bad_repo = h.command("/grant not-a-repo").await;
    assert!(
        bad_repo.text.contains("owner/repo"),
        "bad repo format names the grammar: {}",
        bad_repo.text
    );
    let bad_role = h.command("/grant acme/api emperor").await;
    assert!(
        bad_role.text.contains("Unknown role"),
        "unknown role rejected: {}",
        bad_role.text
    );
    let with_role = h.command("/grant acme/api triage").await;
    assert!(
        with_role.text.contains("intended role: Triage"),
        "explicit role acked: {}",
        with_role.text
    );
    assert!(
        h.fake.sent().is_empty(),
        "validation happens before any flow starts: {:?}",
        h.fake.sent()
    );
}

#[tokio::test]
async fn unknown_command_listing_names_grant() {
    let h = spawn_cmd_gate().await;
    let reply = h.command("/nope").await;
    assert!(
        reply.text.contains("/grant"),
        "the unknown-command listing mentions /grant: {}",
        reply.text
    );
}
