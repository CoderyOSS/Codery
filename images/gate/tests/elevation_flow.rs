//! Integration: elevation flows (GATE-010 §8–9, task 8) — fail-fast
//! pre-flight role checks on the API and git paths, the consent→verify
//! state machine on Control, and temporary grants with auto-reduction.
//!
//! The fail-fast ruling is the spine of every scenario: a role gap answers
//! `permission_pending` immediately (the elevation conversation starts in
//! the background); only the bounded approval hold ever waits on a human.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use tokio::sync::watch;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use gate::audit::Audit;
use gate::github::{GitHubPlatform, Platform};
use gate::gitproxy::GitDeps;
use gate::policy::{PolicyEngine, PolicyFile};
use gate::routes::{router, GateState};
use gate::state::State as DbState;
use gate::control::commands::{register_commands, CommandDeps};
use gate::control::elevation::{
    nonce_lookup as elevation_nonce_lookup, register_elevation, ElevationDeps, ReductionFlow,
};
use gate::control::fake::FakeTransport;
use gate::control::send::SendRequest;
use gate::control::{Callback, MsgRef, ControlApi, ControlRouter, Update};
use gate::tokens::TokenStore;
use gate::types::Role;

use gate::apiproxy::ApiDeps;
use gate::approval::{ApprovalOutcome, StubBackend};

const AGENT: &str = "claude-code";
const MACHINE_LOGIN: &str = "gate-machine";
const MACHINE_TOKEN: &str = "test-machine-token";
const CHAT: i64 = 4242;
const OWNER: &str = "acme";
const REPO: &str = "api";
const REPO_FULL: &str = "acme/api";
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(3);

const POLICY_ALLOW_ALL: &str = "\
[defaults]
push = \"allow\"
force-push = \"allow\"
branch-delete = \"allow\"
tag-delete = \"allow\"
";

// ---------------------------------------------------------------------------
// Harness: one gate serving BOTH proxies, an elevation sweep running on
// test-scale durations, a FakeTransport channel and a recorded reduction seam.
// ---------------------------------------------------------------------------

struct FakeReduction {
    calls: Mutex<Vec<(String, Role)>>,
}

impl FakeReduction {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<(String, Role)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl ReductionFlow for FakeReduction {
    async fn reduce(&self, repo: &str, to: Role) {
        self.calls.lock().unwrap().push((repo.to_string(), to));
    }
}

struct H {
    dir: PathBuf,
    port: u16,
    agent_token: String,
    state: Arc<Mutex<DbState>>,
    fake: Arc<FakeTransport>,
    api: Arc<dyn ControlApi>,
    tg_router: ControlRouter,
    upstream: MockServer,
    reduction: Arc<FakeReduction>,
    audit_path: PathBuf,
    _shutdown: watch::Sender<bool>,
}

impl Drop for H {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Test-scale elevation durations. `batch`/`poll` are per-call so the
/// batching test can widen the window while the transitions test widens the
/// verify cadence (making AWAITING_ROLE_CHANGE observable at the endpoint).
/// `org` decides whether `GET /orgs/acme` answers 200 (org repo) or 404
/// (personal repo — the GATE-010 §9.3 ceiling path).
async fn spawn_gate(batch: Duration, poll: Duration) -> H {
    spawn_gate_in(batch, poll, true).await
}

async fn spawn_gate_in(batch: Duration, poll: Duration, org: bool) -> H {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gate-elevation-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"login":"gate-machine"}"#),
        )
        .mount(&upstream)
        .await;
    if org {
        Mock::given(method("GET"))
            .and(path("/orgs/acme"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"login":"acme"}"#))
            .mount(&upstream)
            .await;
    }

    let db_path = dir.join("gate.db");
    let db = DbState::open(db_path.to_str().unwrap()).unwrap();
    let audit_path = dir.join("audit.jsonl");
    let audit = Arc::new(Audit::open(&audit_path));
    let agent_token = {
        let store = TokenStore::new(&db, &audit);
        store.mint(AGENT).unwrap()
    };
    let state = Arc::new(Mutex::new(db));

    let token_getter: Arc<dyn Fn() -> Option<String> + Send + Sync> =
        Arc::new(move || Some(MACHINE_TOKEN.to_string()));
    let base = reqwest::Url::parse(&upstream.uri()).unwrap();
    let platform: Arc<dyn Platform> = Arc::new(GitHubPlatform::with_bases(
        base.clone(),
        base,
        token_getter.clone(),
    ));
    let policy = PolicyEngine::new(&PolicyFile::parse(POLICY_ALLOW_ALL).unwrap());
    let backend = Arc::new(StubBackend::new(state.clone(), audit.clone(), ApprovalOutcome::Approved).unwrap());

    let fake = Arc::new(FakeTransport::new());
    let tg_api: Arc<dyn ControlApi> = fake.clone();
    let reduction = FakeReduction::new();

    let deps = ElevationDeps {
        state: state.clone(),
        platform: platform.clone(),
        machine_login: MACHINE_LOGIN.to_string(),
        notify: Some((tg_api.clone(), CHAT)),
        poll,
        batch_window: batch,
        reminder_after: Duration::from_secs(3600),
        expire_after: Duration::from_secs(3600),
        reduction: reduction.clone(),
        audit: audit.clone(),
    };
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(gate::control::elevation::run_elevation_sweep(
        deps.clone(),
        shutdown_rx,
        Duration::from_millis(10),
    ));

    let mut tg_router = ControlRouter::new();
    tg_router.set_nonce_lookup(elevation_nonce_lookup(state.clone()));
    register_elevation(
        &mut tg_router,
        Arc::new(gate::control::elevation::ElevationHandler::new(deps)),
    );
    // The command dispatcher too: /elevate opens flows through the SAME
    // open_or_join path the proxies use (the command handlers below only
    // read text updates; the elevation handler above only reads callbacks).
    register_commands(
        &mut tg_router,
        Arc::new(CommandDeps {
            state: state.clone(),
            audit: audit.clone(),
            machine_token: Arc::new(|| None),
            lifecycle: None,
            grant: None,
            reduction: None,
        }),
    );

    let mut git = GitDeps::new(
        state.clone(),
        platform.clone(),
        policy.clone(),
        backend.clone(),
        MACHINE_LOGIN.to_string(),
        token_getter.clone(),
        APPROVAL_TIMEOUT,
    )
    .unwrap();
    git.elevation_notify = Some((tg_api.clone(), CHAT));
    let mut api = ApiDeps::new(
        state.clone(),
        platform.clone(),
        policy,
        backend,
        token_getter,
        APPROVAL_TIMEOUT,
    )
    .unwrap();
    api.elevation_notify = Some((tg_api.clone(), CHAT));

    let mut gate_state = GateState::with_git(state.clone(), audit, git);
    gate_state.api = Some(Arc::new(api));
    gate_state.set_forwarding(true);
    let app = router(gate_state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    H {
        dir,
        port,
        agent_token,
        state,
        fake,
        api: tg_api,
        tg_router,
        upstream,
        reduction,
        audit_path,
        _shutdown: shutdown_tx,
    }
}

impl H {
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    async fn send(&self, method: &str, url_path: &str, body: &[u8]) -> reqwest::Response {
        let auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD
                .encode(format!("{AGENT}:{}", self.agent_token))
        );
        let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut rb = reqwest::Client::new()
            .request(m, self.url(url_path))
            .header("authorization", auth);
        if !body.is_empty() {
            rb = rb.body(body.to_vec());
        }
        rb.send().await.unwrap()
    }

    async fn body_json(resp: reqwest::Response) -> serde_json::Value {
        let bytes = resp.bytes().await.unwrap();
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "body must be json: {e}; body: {:?}",
                String::from_utf8_lossy(&bytes)
            )
        })
    }

    fn grant_role(&self, granted: &str) {
        self.state
            .lock()
            .unwrap()
            .registry_upsert(REPO_FULL, granted, granted, "device-flow")
            .unwrap();
    }

    fn registry_row(&self) -> gate::state::RegistryRow {
        self.state
            .lock()
            .unwrap()
            .registry_get(REPO_FULL)
            .expect("registry read")
            .expect("row present")
    }

    fn flow_states(&self) -> Vec<(String, String, String)> {
        let guard = self.state.lock().unwrap();
        guard
            .flows_list("elevation")
            .expect("flows read")
            .into_iter()
            .map(|f| (f.id, f.flow_state, f.payload_json))
            .collect()
    }

    fn flow_state_of(&self, id: &str) -> Option<String> {
        let guard = self.state.lock().unwrap();
        guard.flow_state(id).expect("flows read")
    }

    fn payload_of(&self, id: &str) -> gate::control::elevation::ElevationPayload {
        let guard = self.state.lock().unwrap();
        let row = guard.flow_get(id).expect("flows read").expect("row present");
        serde_json::from_str(&row.payload_json).expect("payload parses")
    }

    fn audit_lines(&self) -> Vec<serde_json::Value> {
        let contents = std::fs::read_to_string(&self.audit_path).unwrap();
        contents
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    async fn upstream_requests(&self) -> Vec<wiremock::Request> {
        self.upstream.received_requests().await.unwrap()
    }

    /// Sent messages whose text contains `needle`.
    fn sent_with(&self, needle: &str) -> Vec<SendRequest> {
        self.fake
            .sent()
            .iter()
            .filter(|r| r.text.contains(needle))
            .cloned()
            .collect()
    }

    /// Dispatch one operator command through the router; the reply must
    /// be exactly one edit of the command's bubble.
    async fn command(&self, text: &str) -> SendRequest {
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
        self.tg_router.dispatch(&update, &self.api.clone()).await;
        let edits = self.fake.edits();
        assert_eq!(
            edits.len(),
            before + 1,
            "one command ⇒ exactly one reply edit: {edits:?}"
        );
        edits.last().unwrap().1.clone()
    }

    /// The `(label, callback_data)` buttons of the consent message.
    fn consent_buttons(&self) -> Vec<(String, String)> {
        let sent = self.fake.sent();
        let consent = sent
            .iter()
            .find(|r| r.text.contains("Role elevation needed"))
            .expect("consent message sent");
        consent.inline_buttons.clone()
    }

    /// Dispatch a button press on the consent message.
    async fn press(&self, label: &str) {
        let data = self
            .consent_buttons()
            .iter()
            .find(|(l, _)| l == label)
            .map(|(_, d)| d.clone())
            .unwrap_or_else(|| panic!("no '{label}' button"));
        let update = Update {
            callback: Some(Callback {
                id: "cbq-1".to_string(),
                data,
                from_id: CHAT,
            }),
            message_ref: Some(MsgRef {
                chat_id: CHAT,
                message_id: 1,
            }),
            ..Default::default()
        };
        self.tg_router.dispatch(&update, &self.api.clone()).await;
    }

    /// `GET /repos/{owner}/{repo}` mock with a fixed permission body.
    async fn mount_repo_role(&self, body: String) {
        Mock::given(method("GET"))
            .and(path(format!("/repos/{OWNER}/{REPO}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&self.upstream)
            .await;
    }

    /// `GET /repos/{owner}/{repo}`: `lower` for the first `lower_calls`
    /// calls, then `final` (the wrong-role-then-fixed sequence).
    async fn mount_repo_role_then(&self, lower_calls: usize, lower: Role, final_role: Role) {
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let lower_body = permissions_body(lower);
        let final_body = permissions_body(final_role);
        Mock::given(method("GET"))
            .and(path(format!("/repos/{OWNER}/{REPO}")))
            .respond_with(move |_: &wiremock::Request| {
                let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n < lower_calls {
                    ResponseTemplate::new(200).set_body_string(lower_body.clone())
                } else {
                    ResponseTemplate::new(200).set_body_string(final_body.clone())
                }
            })
            .mount(&self.upstream)
            .await;
    }
}

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
        r#"{{"private":true,"permissions":{{"admin":{admin},"maintain":{maintain},
           "push":{push},"triage":{triage},"pull":{pull}}}}}"#
    )
    .replace('\n', "")
}

/// Poll a condition for up to ~15 s (10 ms steps). `wait_until` is the
/// variant for async conditions (HTTP probes).
async fn wait_for(mut cond: impl FnMut() -> bool) {
    for _ in 0..1500 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not met within 15s");
}

async fn wait_until<T, F>(mut cond: T)
where
    T: FnMut() -> F,
    F: std::future::Future<Output = bool>,
{
    for _ in 0..1500 {
        if cond().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not met within 15s");
}

// ---------------------------------------------------------------------------
// 1. Fail-fast pre-flight: held API mutation needing Maintain ⇒ immediate
//    `permission_pending` with an elevation id; upstream NOT hit.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_api_mutation_needing_maintain_fails_fast_without_upstream() {
    let h = spawn_gate(Duration::from_millis(200), Duration::from_millis(20)).await;
    h.grant_role("read");

    let resp = h
        .send("PUT", "/api/v3/repos/acme/api/topics", br#"{"names":["go"]}"#)
        .await;
    assert_eq!(resp.status(), 403);
    let v = H::body_json(resp).await;
    assert_eq!(v["error"], "permission_pending", "{v}");
    let id = v["elevation_id"].as_str().expect("elevation_id").to_string();
    assert!(!id.is_empty());
    assert_eq!(v["needs_role"], "maintain");
    assert!(v["message"].is_string());

    // The flow row exists in the DETECTED state, carrying the gap.
    let flows = h.flow_states();
    assert_eq!(flows.len(), 1, "{flows:?}");
    assert_eq!(flows[0].0, id);
    assert_eq!(flows[0].1, "DETECTED");
    let payload = h.payload_of(&id);
    assert_eq!(payload.repo, REPO_FULL);
    assert_eq!(payload.needed_role, "maintain");

    // Fail-fast: the mutation never left — no topics PUT, and no
    // permissions probe (the registry gap decides before the live check).
    // (Assert on OUR paths only: wiremock port recycling can drop a dying
    // sibling test's stray onto this server — see github.rs's note.)
    wait_until(|| async { !h.upstream_requests().await.is_empty() }).await;
    let requests = h.upstream_requests().await;
    assert!(
        requests
            .iter()
            .all(|r| r.url.path() != "/repos/acme/api/topics"
                && r.url.path() != "/repos/acme/api"),
        "the mutation and the permissions probe must not be forwarded: {requests:?}"
    );
}

// ---------------------------------------------------------------------------
// 1b. Personal-repo ceiling (GATE-010 §9.3): a Maintain/Admin need on a
//     personal repo CANNOT be elevated — role_unavailable, no flow, DIY
//     instructions.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn personal_repo_maintain_need_is_role_unavailable_without_a_flow() {
    let h = spawn_gate_in(Duration::from_millis(100), Duration::from_millis(20), false).await;
    h.grant_role("read");

    let resp = h
        .send("PUT", "/api/v3/repos/acme/api/topics", br#"{"names":["go"]}"#)
        .await;
    assert_eq!(resp.status(), 403);
    let v = H::body_json(resp).await;
    // Exact role_unavailable shape: error + message, nothing else (no
    // elevation_id, no needs_role — there IS no elevation to poll).
    assert_eq!(v["error"], "role_unavailable", "{v}");
    let message = v["message"].as_str().expect("message").to_string();
    assert!(message.contains("acme/api"), "{message}");
    assert!(message.contains("Maintain"), "{message}");
    assert!(message.contains("personal"), "{message}");
    assert_eq!(
        v.as_object().unwrap().len(),
        2,
        "exact shape: {{error, message}} — {v}"
    );

    // NO flow was opened, and no consent conversation started.
    assert!(
        h.flow_states().is_empty(),
        "personal repos never open elevation flows: {:?}",
        h.flow_states()
    );
    assert!(h.sent_with("Role elevation needed").is_empty());

    // The operator gets the DIY instructions instead.
    let diy = h.sent_with("do this one yourself");
    assert_eq!(diy.len(), 1, "one DIY notice: {:?}", h.fake.sent());
    assert!(diy[0].text.contains("acme/api"), "{}", diy[0].text);
    assert!(diy[0].text.contains("Maintain"), "{}", diy[0].text);

    // The org check ran exactly once — and nothing else left the gate.
    wait_until(|| async { !h.upstream_requests().await.is_empty() }).await;
    let requests = h.upstream_requests().await;
    let org_calls: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path() == "/orgs/acme")
        .collect();
    assert_eq!(org_calls.len(), 1, "one /orgs probe: {requests:?}");
    assert!(
        requests
            .iter()
            .all(|r| r.url.path() != "/repos/acme/api/topics" && r.url.path() != "/repos/acme/api"),
        "the mutation must not be forwarded: {requests:?}"
    );

    // The denial is audited as a pre-flight deny.
    let audit = h.audit_lines();
    let api: Vec<_> = audit
        .iter()
        .filter(|l| l["action"] == "api.request")
        .collect();
    assert!(
        api.iter()
            .any(|l| l["decision"] == "deny"
                && l["basis"]
                    .as_str()
                    .unwrap()
                    .contains("personal-repo ceiling")),
        "{api:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. Consent ⇒ role change ⇒ DONE + registry Maintain + retried request
//    hits upstream.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consent_role_change_reaches_done_and_retry_hits_upstream() {
    let h = spawn_gate(Duration::from_millis(100), Duration::from_millis(20)).await;
    h.grant_role("read");
    h.mount_repo_role(permissions_body(Role::Maintain)).await;
    Mock::given(method("PUT"))
        .and(path("/repos/acme/api/topics"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&h.upstream)
        .await;

    let resp = h
        .send("PUT", "/api/v3/repos/acme/api/topics", br#"{"names":["go"]}"#)
        .await;
    assert_eq!(resp.status(), 403);
    let v = H::body_json(resp).await;
    let id = v["elevation_id"].as_str().unwrap().to_string();

    // Consent message: why / needs / has / one plain-words line + buttons.
    wait_for(|| !h.sent_with("Role elevation needed").is_empty()).await;
    let consent = h.sent_with("Role elevation needed").remove(0);
    assert!(consent.text.contains(REPO_FULL), "{}", consent.text);
    assert!(consent.text.contains("/repos/acme/api/topics"), "{}", consent.text);
    assert!(consent.text.contains("Maintain"), "{}", consent.text);
    assert!(consent.text.contains("Read"), "{}", consent.text);
    assert!(
        consent.text.contains("topics"),
        "plain-words line present: {}",
        consent.text
    );
    let labels: Vec<&str> = consent
        .inline_buttons
        .iter()
        .map(|(l, _)| l.as_str())
        .collect();
    assert_eq!(
        labels,
        vec!["Grant it", "Grant until done, then remove", "Not now"],
        "{}",
        consent.text
    );

    h.press("Grant it").await;

    wait_for(|| h.flow_state_of(&id).as_deref() == Some("DONE")).await;
    let row = h.registry_row();
    assert_eq!(row.granted_role, "maintain", "registry records the new role");
    assert_eq!(row.via, "elevation");

    // Success message promises the retry.
    assert!(
        !h.sent_with("The agent will retry its request").is_empty(),
        "success message sent: {:?}",
        h.fake.sent()
    );
    // A permanent grant must NOT fire the reduction seam.
    assert!(
        h.reduction.calls().is_empty(),
        "permanent grant never reduces: {:?}",
        h.reduction.calls()
    );

    // The retried request now flows to upstream.
    let resp = h
        .send("PUT", "/api/v3/repos/acme/api/topics", br#"{"names":["go"]}"#)
        .await;
    assert_eq!(resp.status(), 204, "retry succeeds after the elevation");
    let requests = h.upstream_requests().await;
    assert!(
        requests
            .iter()
            .any(|r| r.method == wiremock::http::Method::PUT
                && r.url.path() == "/repos/acme/api/topics"),
        "retry hit upstream: {requests:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. Wrong role (Triage) ⇒ difference message naming both roles.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_role_triage_gets_the_difference_message() {
    let h = spawn_gate(Duration::from_millis(100), Duration::from_millis(20)).await;
    h.grant_role("read");
    h.mount_repo_role_then(50, Role::Triage, Role::Maintain).await;

    let resp = h
        .send("PUT", "/api/v3/repos/acme/api/topics", br#"{"names":["go"]}"#)
        .await;
    let v = H::body_json(resp).await;
    let id = v["elevation_id"].as_str().unwrap().to_string();

    wait_for(|| !h.sent_with("Role elevation needed").is_empty()).await;
    h.press("Grant it").await;

    // Difference message: names the found role AND the needed role.
    wait_for(|| !h.sent_with("Triage").is_empty()).await;
    let diff = h
        .sent_with("Triage")
        .into_iter()
        .find(|m| m.text.contains("Maintain"))
        .expect("difference message names both roles");
    assert!(diff.text.contains(MACHINE_LOGIN), "{}", diff.text);
    assert!(diff.text.contains(REPO_FULL), "{}", diff.text);

    // Polling continues until the right role lands ⇒ DONE.
    wait_for(|| h.flow_state_of(&id).as_deref() == Some("DONE")).await;
    assert_eq!(h.registry_row().granted_role, "maintain");
}

// ---------------------------------------------------------------------------
// 4. [Not now] ⇒ CANCELLED.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_now_cancels_the_flow() {
    let h = spawn_gate(Duration::from_millis(100), Duration::from_millis(20)).await;
    h.grant_role("read");

    let resp = h
        .send("PUT", "/api/v3/repos/acme/api/topics", br#"{"names":["go"]}"#)
        .await;
    let v = H::body_json(resp).await;
    let id = v["elevation_id"].as_str().unwrap().to_string();

    wait_for(|| !h.sent_with("Role elevation needed").is_empty()).await;
    h.press("Not now").await;

    wait_for(|| h.flow_state_of(&id).as_deref() == Some("CANCELLED")).await;
    let edits = h.fake.edits();
    assert!(
        edits
            .iter()
            .any(|(_, r)| r.text.to_lowercase().contains("cancel")),
        "the consent message is updated with the cancellation: {edits:?}"
    );
    // The decision is audited with the presser as approver.
    let audit = h.audit_lines();
    assert!(
        audit
            .iter()
            .any(|l| l["action"] == "elevation.decision" && l["decision"] == "cancelled"),
        "{audit:?}"
    );
}

// ---------------------------------------------------------------------------
// 5. Temporary grant: DONE ⇒ reduction seam auto-started.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporary_grant_auto_starts_the_reduction() {
    let h = spawn_gate(Duration::from_millis(100), Duration::from_millis(20)).await;
    h.grant_role("read");
    h.mount_repo_role(permissions_body(Role::Maintain)).await;

    let resp = h
        .send("PUT", "/api/v3/repos/acme/api/topics", br#"{"names":["go"]}"#)
        .await;
    let v = H::body_json(resp).await;
    let id = v["elevation_id"].as_str().unwrap().to_string();

    wait_for(|| !h.sent_with("Role elevation needed").is_empty()).await;
    h.press("Grant until done, then remove").await;

    wait_for(|| h.flow_state_of(&id).as_deref() == Some("DONE")).await;
    let payload = h.payload_of(&id);
    assert_eq!(payload.temporary, Some(true));
    assert_eq!(
        payload.intended_role.as_deref(),
        Some("read"),
        "reduction target = the role held before the elevation"
    );

    // The seam was called with the repo and the pre-elevation role.
    wait_for(|| !h.reduction.calls().is_empty()).await;
    assert_eq!(
        h.reduction.calls(),
        vec![(REPO_FULL.to_string(), Role::Read)],
        "{:?}",
        h.reduction.calls()
    );
    // The registry already points back at the intended role.
    assert_eq!(h.registry_row().intended_role, "read");
    assert_eq!(h.registry_row().granted_role, "maintain");

    // The success copy matches the actual behavior: the seam fires at DONE
    // (reduction starts NOW) — it must not promise a later trigger.
    let success = h
        .sent_with("temporary grant")
        .into_iter()
        .find(|m| m.text.contains("Read"))
        .expect("temporary-grant success message");
    assert!(
        !success.text.contains("once you confirm"),
        "no promise of a later trigger (seam fires at DONE): {}",
        success.text
    );
    assert!(
        success.text.contains("now"),
        "the copy says the reduction starts now: {}",
        success.text
    );

    // And the retried request succeeds while the temporary grant holds.
    Mock::given(method("PUT"))
        .and(path("/repos/acme/api/topics"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&h.upstream)
        .await;
    let resp = h
        .send("PUT", "/api/v3/repos/acme/api/topics", br#"{"names":["go"]}"#)
        .await;
    assert_eq!(resp.status(), 204, "the temporary grant carries the retry");

    // §9.2 bookkeeping: a successful retried request marks work_done=1 on
    // the temp elevation flow (the reduction still proceeds at DONE — this
    // is a record, not a second trigger).
    wait_for(|| h.payload_of(&id).work_done == Some(1)).await;
}

// ---------------------------------------------------------------------------
// 6. Two concurrent gaps 2 s apart ⇒ ONE combined consent message.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_concurrent_gaps_two_seconds_apart_present_one_message() {
    // Batch window wider than the 2 s gap, so both gaps land inside it.
    let h = spawn_gate(Duration::from_millis(3000), Duration::from_millis(20)).await;
    h.grant_role("read");

    // Gap 1: topics ⇒ Maintain.
    let resp = h
        .send("PUT", "/api/v3/repos/acme/api/topics", br#"{"names":["go"]}"#)
        .await;
    assert_eq!(resp.status(), 403);

    tokio::time::sleep(Duration::from_secs(2)).await;

    // Gap 2: repo settings PATCH ⇒ Admin (raises the needed role).
    let resp = h.send("PATCH", "/api/v3/repos/acme/api", br#"{"description":"x"}"#).await;
    assert_eq!(resp.status(), 403);

    // Exactly ONE flow and ONE consent message, naming both actions and
    // the raised (max) role.
    wait_for(|| !h.sent_with("Role elevation needed").is_empty()).await;
    tokio::time::sleep(Duration::from_millis(100)).await; // let late writes settle
    let consents = h.sent_with("Role elevation needed");
    assert_eq!(consents.len(), 1, "one combined message: {consents:?}");
    let flows = h.flow_states();
    assert_eq!(flows.len(), 1, "one flow for the repo: {flows:?}");
    let text = &consents[0].text;
    assert!(text.contains("/repos/acme/api/topics"), "{}", text);
    assert!(text.contains("/repos/acme/api"), "{}", text);
    assert!(text.contains("Admin"), "raised to the max role: {}", text);
    assert!(text.contains("Read"), "current role shown: {}", text);
    let payload = h.payload_of(&flows[0].0);
    assert_eq!(payload.needed_role, "admin");
    assert_eq!(payload.actions.len(), 2, "{:?}", payload.actions);
}

// ---------------------------------------------------------------------------
// 7. `/elevate owner/repo role` (GATE-006/GATE-010): the operator-side
//    trigger opens/joins the SAME flow the proxies open; the sweep takes
//    it to consent exactly like a detected gap.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elevate_command_opens_the_flow_and_the_sweep_takes_it_to_consent() {
    let h = spawn_gate(Duration::from_millis(30), Duration::from_millis(10)).await;

    // Bad role: usage reply, nothing opened.
    let reply = h.command("/elevate acme/api emperor").await;
    assert!(reply.text.contains("Usage: /elevate"), "{}", reply.text);
    assert!(h.flow_states().is_empty(), "rejections open no flow");

    // Valid: the DETECTED row exists immediately, the reply names its
    // state, and the sweep claims it — consent message out, row at
    // AWAITING_USER_CONSENT (GATE-010 states machine).
    let reply = h.command("/elevate acme/api maintain").await;
    let flows = h.flow_states();
    assert_eq!(flows.len(), 1);
    assert_eq!(flows[0].1, "DETECTED");
    assert!(reply.text.contains("DETECTED"), "reply names the flow state: {}", reply.text);
    assert!(reply.text.contains(&flows[0].0), "reply names the flow id: {}", reply.text);
    let payload = h.payload_of(&flows[0].0);
    assert_eq!(payload.repo, "acme/api");
    assert_eq!(payload.needed_role, "maintain");
    assert_eq!(payload.actions.len(), 1);

    wait_for(|| !h.sent_with("Role elevation needed").is_empty()).await;
    let id = flows[0].0.clone();
    wait_for(|| h.flow_state_of(&id).as_deref() == Some("AWAITING_USER_CONSENT")).await;

    // Re-issued: joins the SAME flow (no duplicate row, no duplicate
    // consent) and the reply names the joined state.
    let consent_count = h.sent_with("Role elevation needed").len();
    let reply = h.command("/elevate acme/api maintain").await;
    assert_eq!(h.flow_states().len(), 1, "join, never duplicate");
    assert!(
        reply.text.contains("AWAITING_USER_CONSENT"),
        "reply names the joined flow state: {}",
        reply.text
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        h.sent_with("Role elevation needed").len(),
        consent_count,
        "no duplicate consent message"
    );
}

// ---------------------------------------------------------------------------
// 7. /proxy/elevations/{id} reflects the state transitions.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elevation_endpoint_reflects_state_transitions() {
    // Slow verify cadence ⇒ AWAITING_ROLE_CHANGE stays observable between
    // the button press and the flow task's next poll.
    let h = spawn_gate(Duration::from_millis(100), Duration::from_millis(250)).await;
    h.grant_role("read");
    // Triage forever ⇒ the flow parks in VERIFYING.
    h.mount_repo_role(permissions_body(Role::Triage)).await;

    let resp = h
        .send("PUT", "/api/v3/repos/acme/api/topics", br#"{"names":["go"]}"#)
        .await;
    let v = H::body_json(resp).await;
    let id = v["elevation_id"].as_str().unwrap().to_string();

    // Endpoint probes (inner fns: no captures, take the harness + id).
    async fn endpoint_json(h: &H, id: &str) -> serde_json::Value {
        let resp = h.send("GET", &format!("/proxy/elevations/{id}"), b"").await;
        assert_eq!(resp.status(), 200);
        let v = H::body_json(resp).await;
        assert_eq!(v["id"], id);
        v
    }
    async fn endpoint_state(h: &H, id: &str) -> String {
        endpoint_json(h, id).await["state"].as_str().unwrap().to_string()
    }

    // Immediately after the fail-fast answer the endpoint reflects an
    // early state (DETECTED; AWAITING_USER_CONSENT if the sweep already
    // presented consent — the batching window in this test is short).
    let first = endpoint_state(&h, &id).await;
    assert!(
        first == "DETECTED" || first == "AWAITING_USER_CONSENT",
        "endpoint must reflect an early flow state, got {first}"
    );

    wait_until(|| async {
        let v = endpoint_json(&h, &id).await;
        v["state"] == "AWAITING_USER_CONSENT"
    })
    .await;

    h.press("Grant it").await;

    // AWAITING_ROLE_CHANGE is observable during the flow task's poll gap.
    let mut seed = vec![first.clone()];
    if seed.last() != Some(&"AWAITING_USER_CONSENT".to_string()) {
        seed.push("AWAITING_USER_CONSENT".to_string());
    }
    let seen = Mutex::new(seed);
    wait_until(|| async {
        let s = endpoint_state(&h, &id).await;
        let mut seen = seen.lock().unwrap();
        if seen.last() != Some(&s) {
            seen.push(s);
        }
        seen.last().map(String::as_str) == Some("VERIFYING")
    })
    .await;
    let seen = seen.into_inner().unwrap();
    assert!(
        seen.contains(&"AWAITING_ROLE_CHANGE".to_string()),
        "transitions observed: {seen:?}"
    );
    let expected: Vec<Vec<String>> = vec![
        // DETECTED observed before consent, or consent beat the first probe.
        vec![
            "DETECTED".to_string(),
            "AWAITING_USER_CONSENT".to_string(),
            "AWAITING_ROLE_CHANGE".to_string(),
            "VERIFYING".to_string(),
        ],
        vec![
            "AWAITING_USER_CONSENT".to_string(),
            "AWAITING_ROLE_CHANGE".to_string(),
            "VERIFYING".to_string(),
        ],
    ];
    assert!(
        expected.iter().any(|e| *e == seen),
        "exact transition order: {seen:?}"
    );

    // The endpoint also carries the gap detail for polling agents.
    let v = endpoint_json(&h, &id).await;
    assert_eq!(v["repo"], REPO_FULL);
    assert_eq!(v["needs_role"], "maintain");
}

// ---------------------------------------------------------------------------
// 8. Git push role gap ⇒ ERR payload with the elevation id; upstream not hit.
// ---------------------------------------------------------------------------

const ZERO_OLD: &str = "0000000000000000000000000000000000000000";
const NEW_OID: &str = "fedcba0987654321fedcba0987654321fedcba09";

fn pkt(payload: &str) -> Vec<u8> {
    let mut raw = format!("{:04x}", 4 + payload.len()).into_bytes();
    raw.extend_from_slice(payload.as_bytes());
    raw
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_push_role_gap_err_carries_elevation_id_without_upstream() {
    let h = spawn_gate(Duration::from_millis(100), Duration::from_millis(20)).await;
    h.grant_role("read");

    // A ref creation (zero old) classifies as Push without any ancestry
    // call, and the allow-all policy forwards it — straight into the
    // route-level role pre-flight.
    let mut body = pkt(&format!(
        "{ZERO_OLD} {NEW_OID} refs/heads/feature\0report-status object-format=sha1 agent=gate-test\n"
    ));
    body.extend_from_slice(b"0000");

    let resp = h
        .send("POST", "/git/acme/api/git-receive-pack", &body)
        .await;
    assert_eq!(resp.status(), 403);
    let bytes = resp.bytes().await.unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    assert!(text.contains("ERR "), "git-friendly ERR payload: {text:?}");

    // The ERR payload names an elevation id backed by a real flow row.
    let flows = h.flow_states();
    assert_eq!(flows.len(), 1, "{flows:?}");
    let id = &flows[0].0;
    // Active flow (DETECTED; the sweep may already have presented consent
    // with this test's short batching window).
    assert!(
        gate::control::elevation::is_active_state(&flows[0].1),
        "flow must be active, got {}",
        flows[0].1
    );
    assert!(
        text.contains(id.as_str()),
        "ERR payload must contain the elevation id {id}: {text:?}"
    );
    let payload = h.payload_of(id);
    assert_eq!(payload.repo, REPO_FULL);
    assert_eq!(payload.needed_role, "write");

    // Nothing reached upstream — no advertisement, no receive-pack (our
    // paths only; sibling strays on recycled ports are documented noise).
    wait_for(|| true).await; // settle
    let requests = h.upstream_requests().await;
    assert!(
        requests
            .iter()
            .all(|r| !r.url.path().contains("/acme/api.git")),
        "the push must not be forwarded: {requests:?}"
    );
    let audit = h.audit_lines();
    let pushes: Vec<_> = audit
        .iter()
        .filter(|l| l["action"] == "git.receive-pack")
        .collect();
    assert!(
        pushes
            .iter()
            .any(|l| l["decision"] == "deny"
                && l["basis"].as_str().unwrap().contains("preflight")),
        "{pushes:?}"
    );
}

// ---------------------------------------------------------------------------
// 9. Contradiction: registry says Admin, GitHub refuses ⇒ rolemap.contradiction.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contradiction_logged_when_registry_role_covers_but_upstream_refuses() {
    let h = spawn_gate(Duration::from_millis(100), Duration::from_millis(20)).await;
    h.grant_role("admin");
    h.mount_repo_role(permissions_body(Role::Admin)).await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/api/hooks"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_string(r#"{"message":"Must have admin rights to Repository."}"#),
        )
        .mount(&h.upstream)
        .await;

    let resp = h
        .send("POST", "/api/v3/repos/acme/api/hooks", br#"{"name":"web"}"#)
        .await;
    assert_eq!(resp.status(), 403);
    let v = H::body_json(resp).await;
    assert_eq!(v["error"], "permission_pending", "{v}");
    let id = v["elevation_id"].as_str().unwrap().to_string();

    // The refusal still opens an elevation flow (the operator decides);
    // with the test's short batching window consent may already be out, so
    // any ACTIVE state backs the id.
    let state = h.flow_state_of(&id).expect("flow row exists");
    assert!(
        gate::control::elevation::is_active_state(&state),
        "flow must be active, got {state}"
    );

    // …and the map-vs-reality contradiction is audited with method/path.
    let audit = h.audit_lines();
    let hits: Vec<_> = audit
        .iter()
        .filter(|l| l["action"] == "rolemap.contradiction")
        .collect();
    assert_eq!(hits.len(), 1, "{audit:?}");
    let basis = hits[0]["basis"].as_str().unwrap();
    assert!(basis.contains("POST"), "{basis}");
    assert!(basis.contains("/repos/acme/api/hooks"), "{basis}");
    assert!(basis.contains("admin"), "{basis}");
}
