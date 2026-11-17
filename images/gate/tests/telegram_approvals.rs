//! Integration: Telegram approval delivery (GATE-005/006) end-to-end.
//! Approval asks land on the control-plane chat with signed Approve/Deny
//! buttons; a validated press forwards or rejects the held push through
//! the same `ApprovalBackend` trait the git/API proxies consume; timed-out
//! holds answer a late press with their state and never touch upstream;
//! /block is the kill switch that denies before any Telegram ask; /pending
//! and /status render the gate's live state.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use gate::approval::{ApprovalBackend, ApprovalDetail, ApprovalRequest};
use gate::audit::Audit;
use gate::github::GitHubPlatform;
use gate::gitproxy::GitDeps;
use gate::policy::{PolicyEngine, PolicyFile};
use gate::routes::{router, GateState};
use gate::state::State as DbState;
use gate::telegram::approvals::{register_approvals, TelegramApprovalBackend, TIMED_OUT_TEXT};
use gate::telegram::commands::{register_commands, CommandDeps};
use gate::telegram::fake::{FakeTelegram, FAKE_CHAT_ID};
use gate::telegram::{
    run_poll_loop, Callback, MsgRef, TelegramApi, TelegramRouter, Update,
};
use gate::tokens::TokenStore;
use gate::types::ActionClass;

const MACHINE_LOGIN: &str = "gate-machine";
const MACHINE_TOKEN: &str = "test-machine-token";
const AGENT: &str = "claude-code";
const OWNER: &str = "octo";
const REPO: &str = "hello";
const REPO_FULL: &str = "octo/hello";
const APPROVER: i64 = 42;

const POLICY: &str = "\
[defaults]
push = \"allow\"
force-push = \"approve\"
branch-delete = \"approve\"
tag-delete = \"approve\"
";

// ---------------------------------------------------------------------------
// Harness: gate + Telegram control plane, wired like main.rs
// ---------------------------------------------------------------------------

struct Gate {
    port: u16,
    token: String,
    upstream: MockServer,
    audit_path: PathBuf,
    dir: PathBuf,
    fake: Arc<FakeTelegram>,
    backend: Arc<TelegramApprovalBackend>,
    state: Arc<Mutex<DbState>>,
    _shutdown: tokio::sync::watch::Sender<bool>,
}

impl Gate {
    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/git", self.port)
    }

    fn credentialed_url(&self) -> String {
        format!(
            "http://{AGENT}:{}@127.0.0.1:{}/git/{OWNER}/{REPO}.git",
            self.token, self.port
        )
    }

    fn audit_lines(&self) -> Vec<serde_json::Value> {
        let contents = std::fs::read_to_string(&self.audit_path).unwrap();
        contents
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn resolve_audit(&self) -> Vec<serde_json::Value> {
        self.audit_lines()
            .into_iter()
            .filter(|l| l["action"] == "approval.resolve")
            .collect()
    }

    async fn upstream_requests(&self) -> Vec<wiremock::Request> {
        self.upstream.received_requests().await.unwrap()
    }

    fn receive_pack_hits<'a>(
        &self,
        requests: &'a [wiremock::Request],
        owner: &str,
        repo: &str,
    ) -> Vec<&'a wiremock::Request> {
        requests
            .iter()
            .filter(|r| {
                r.method == wiremock::http::Method::POST
                    && r.url.path() == &format!("/{owner}/{repo}.git/git-receive-pack")
            })
            .collect()
    }

    fn row_outcome(&self, id: &str) -> Option<gate::approval::ApprovalOutcome> {
        let guard = self.state.lock().unwrap();
        gate::approval::row_state(&guard, id).2
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn spawn_gate(approval_timeout: Duration) -> Gate {
    spawn_gate_with_chat(approval_timeout, Some(FAKE_CHAT_ID)).await
}

/// Variant with a test-chosen ask chat (`None` = no chat configured ⇒
/// asks are undeliverable, present must fail closed without persisting).
async fn spawn_gate_with_chat(approval_timeout: Duration, ask_chat: Option<i64>) -> Gate {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gate-tg-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/user/memberships/orgs/{OWNER}")))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"state":"active"}"#))
        .mount(&upstream)
        .await;

    let db = DbState::open(":memory:").unwrap();
    let audit_path = dir.join("audit.jsonl");
    let audit = Arc::new(Audit::open(&audit_path));
    let token = {
        let store = TokenStore::new(&db, &audit);
        store.mint(AGENT).unwrap()
    };
    let state = Arc::new(Mutex::new(db));

    let fake = Arc::new(FakeTelegram::new());
    let api: Arc<dyn TelegramApi> = fake.clone();
    let backend = Arc::new(
        TelegramApprovalBackend::new(state.clone(), audit.clone(), api.clone(), ask_chat).unwrap(),
    );

    let machine_token: Arc<dyn Fn() -> Option<String> + Send + Sync> =
        Arc::new(|| Some(MACHINE_TOKEN.to_string()));
    let base = reqwest::Url::parse(&upstream.uri()).unwrap();
    let platform = Arc::new(GitHubPlatform::with_bases(
        base.clone(),
        base,
        machine_token.clone(),
    ));
    let policy = PolicyEngine::new(&PolicyFile::parse(POLICY).unwrap());
    let git = GitDeps::new(
        state.clone(),
        platform,
        policy,
        backend.clone(),
        MACHINE_LOGIN.to_string(),
        machine_token.clone(),
        approval_timeout,
    )
    .unwrap();

    let gate_state = GateState::with_git(state.clone(), audit.clone(), git);
    // The harness models a working gate: live machine credential, startup
    // verdict "on" — approval holds resolve into real upstream forwards.
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

    // Control plane wired exactly as the binary does: poll loop -> router
    // with the approval callback handler + command handlers registered.
    let mut tg_router = TelegramRouter::new();
    register_approvals(&mut tg_router, backend.clone());
    register_commands(
        &mut tg_router,
        Arc::new(CommandDeps {
            state: state.clone(),
            audit: audit.clone(),
            machine_token,
            lifecycle: None,
            grant: None,
            reduction: None,
        }),
    );
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let api_for_loop: Arc<dyn TelegramApi> = fake.clone();
    tokio::spawn(run_poll_loop(
        api_for_loop.clone(),
        vec![APPROVER],
        tx,
        shutdown_rx,
        Duration::from_millis(10),
    ));
    tokio::spawn(tg_router.run(rx, api_for_loop));

    Gate {
        port,
        token,
        upstream,
        audit_path,
        dir,
        fake,
        backend,
        state,
        _shutdown: shutdown_tx,
    }
}

// ---------------------------------------------------------------------------
// Git client harness (mirrors tests/git_proxy.rs)
// ---------------------------------------------------------------------------

fn pkt(payload: &str) -> Vec<u8> {
    let mut raw = format!("{:04x}", 4 + payload.len()).into_bytes();
    raw.extend_from_slice(payload.as_bytes());
    raw
}

fn flush() -> Vec<u8> {
    b"0000".to_vec()
}

fn run_git(dir: &Path, args: &[&str]) -> (bool, String) {
    let output = Command::new("git")
        .args(["-c", "protocol.version=0", "-c", "init.defaultBranch=main"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(dir)
        .output()
        .expect("git runs");
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

fn init_repo(dir: &Path, name: &str) -> PathBuf {
    let repo = dir.join(name);
    std::fs::create_dir_all(&repo).unwrap();
    let (ok, out) = run_git(&repo, &["init", "-b", "main"]);
    assert!(ok, "init: {out}");
    let (ok, out) = run_git(&repo, &["config", "user.email", "gate@example.invalid"]);
    assert!(ok, "config: {out}");
    let (ok, out) = run_git(&repo, &["config", "user.name", "Gate Test"]);
    assert!(ok, "config: {out}");
    std::fs::write(repo.join("file.txt"), "hello\n").unwrap();
    let (ok, out) = run_git(&repo, &["add", "."]);
    assert!(ok, "add: {out}");
    let (ok, out) = run_git(&repo, &["commit", "-m", "seed commit"]);
    assert!(ok, "commit: {out}");
    repo
}

fn rev(repo: &Path, rev: &str) -> String {
    let output = Command::new("git")
        .args(["rev-parse", rev])
        .current_dir(repo)
        .output()
        .expect("rev-parse runs");
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn receive_advertisement(refs: &[(&str, &str)]) -> Vec<u8> {
    let mut b = pkt("# service=git-receive-pack\n");
    b.extend_from_slice(&flush());
    for (i, (name, sha)) in refs.iter().enumerate() {
        if i == 0 {
            b.extend(pkt(&format!(
                "{sha} {name}\0report-status delete-refs ofs-delta object-format=sha1 agent=gate-test\n"
            )));
        } else {
            b.extend(pkt(&format!("{sha} {name}\n")));
        }
    }
    b.extend_from_slice(&flush());
    b
}

fn report_status_ok(refname: &str) -> Vec<u8> {
    let mut b = pkt("unpack ok\n");
    b.extend(pkt(&format!("ok {refname}\n")));
    b.extend_from_slice(&flush());
    b
}

/// Mount the upstream receive flow: ref advertisement, compare answer and
/// a receive-pack POST that accepts everything.
async fn mount_receive_flow(
    gate: &Gate,
    repo_full: &str,
    refs: &[(&str, &str)],
    compare_path: &str,
    compare_body: &str,
) {
    Mock::given(method("GET"))
        .and(path(format!("/{repo_full}.git/info/refs")))
        .and(query_param("service", "git-receive-pack"))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("content-type", "application/x-git-receive-pack-advertisement")
                .set_body_bytes(receive_advertisement(refs)),
        )
        .mount(&gate.upstream)
        .await;
    Mock::given(method("GET"))
        .and(path(compare_path))
        .respond_with(ResponseTemplate::new(200).set_body_string(compare_body))
        .mount(&gate.upstream)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/{repo_full}.git/git-receive-pack")))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("content-type", "application/x-git-receive-pack-result")
                .set_body_bytes(report_status_ok("refs/heads/main")),
        )
        .mount(&gate.upstream)
        .await;
}

/// A receive-pack POST body whose ref update (old != new, both non-zero)
/// needs ancestry — with a diverged compare it classifies force-push and
/// holds.
fn force_push_body(old: &str, new: &str) -> Vec<u8> {
    let mut body = pkt(&format!("{old} {new} refs/heads/main\0report-status"));
    body.extend_from_slice(&flush());
    body.extend_from_slice(b"PACK-fake-packfile-bytes-0123456789");
    body
}

fn agent_auth(token: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{AGENT}:{token}"))
    )
}

/// POST a receive-pack push through the gate; returns the status code.
async fn post_push(gate: &Gate, owner: &str, repo: &str, body: Vec<u8>) -> u16 {
    reqwest::Client::new()
        .post(format!(
            "{base}/{owner}/{repo}.git/git-receive-pack",
            base = gate.base_url()
        ))
        .header("authorization", agent_auth(&gate.token))
        .header("content-type", "application/x-git-receive-pack-request")
        .body(body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

// ---------------------------------------------------------------------------
// Telegram update helpers
// ---------------------------------------------------------------------------

/// Poll a condition for up to ~6s (10ms steps).
async fn wait_for(mut cond: impl FnMut() -> bool) {
    for _ in 0..600 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not met within 6s");
}

/// The single outbound ask: (text, buttons).
fn ask_message(fake: &FakeTelegram) -> (String, Vec<(String, String)>) {
    let sent = fake.sent();
    assert_eq!(sent.len(), 1, "exactly one outbound message: {:?}", sent);
    (sent[0].text.clone(), sent[0].inline_buttons.clone())
}

/// Extract (pending id, nonce) from the Approve button's signed triple.
fn ask_triple(fake: &FakeTelegram) -> (String, String) {
    let (_, buttons) = ask_message(fake);
    let (_, data) = buttons
        .iter()
        .find(|(label, _)| label == "Approve")
        .expect("Approve button present");
    let mut parts = data.split(':');
    let id = parts.next().unwrap().to_string();
    let nonce = parts.next().unwrap().to_string();
    assert_eq!(parts.next(), Some("approve"));
    (id, nonce)
}

fn callback_update(from: i64, data: &str, ask_ref: MsgRef) -> Update {
    Update {
        callback: Some(Callback {
            id: "cbq-1".to_string(),
            data: data.to_string(),
            from_id: from,
        }),
        message_ref: Some(ask_ref),
        ..Default::default()
    }
}

fn text_update(from: i64, text: &str, msg: MsgRef) -> Update {
    Update {
        sender_id: Some(from),
        text: Some(text.to_string()),
        message_ref: Some(msg),
        ..Default::default()
    }
}

fn msg_ref(message_id: i32) -> MsgRef {
    MsgRef {
        chat_id: FAKE_CHAT_ID,
        message_id,
    }
}

fn row_nonce(gate: &Gate, id: &str) -> Option<String> {
    let guard = gate.state.lock().unwrap();
    gate::approval::row_nonce(&guard, id)
}

/// Every pending row as `(id, outcome)` — the DB truth.
fn pending_outcomes(gate: &Gate) -> Vec<(String, Option<gate::approval::ApprovalOutcome>)> {
    let guard = gate.state.lock().unwrap();
    gate::approval::all_pending_outcomes(&guard).unwrap()
}

const OLD: &str = "1234567890abcdef1234567890abcdef12345678";
const NEW: &str = "fedcba0987654321fedcba0987654321fedcba09";

// ---------------------------------------------------------------------------
// Approval ask delivery
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approval_request_sends_ask_with_repo_detail_and_signed_buttons() {
    let gate = spawn_gate(Duration::from_secs(30)).await;
    mount_receive_flow(
        &gate,
        REPO_FULL,
        &[("refs/heads/main", OLD)],
        &format!("/repos/{REPO_FULL}/compare/{OLD}...{NEW}"),
        r#"{"status":"diverged","commits":[{"sha":"fedcba","commit":{"message":"rewritten\n"}}]}"#,
    )
    .await;

    // The held POST blocks until the approval window passes; the ask must
    // appear long before that.
    let gate_port = gate.port;
    let gate_token = gate.token.clone();
    let body = force_push_body(OLD, NEW);
    let holder = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!(
                "http://127.0.0.1:{gate_port}/git/{OWNER}/{REPO}.git/git-receive-pack"
            ))
            .header("authorization", agent_auth(&gate_token))
            .header("content-type", "application/x-git-receive-pack-request")
            .body(body)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    });

    wait_for(|| !gate.fake.sent().is_empty()).await;
    let (text, buttons) = ask_message(&gate.fake);

    // GATE-006 style: what's happening (repo, class, refs) and what to do.
    assert!(
        text.contains(REPO_FULL) && text.contains("force_push"),
        "ask names repo and class: {text}"
    );
    assert!(text.contains("refs/heads/main"), "ask names the ref: {text}");
    assert!(
        text.contains(&OLD[..7]) && text.contains(&NEW[..7]),
        "ask shows old→new short shas: {text}"
    );
    assert!(
        text.contains("rewritten"),
        "ask shows the commit subject: {text}"
    );

    // Buttons: [Approve]/[Deny] bound to (pending id, nonce, action).
    let labels: Vec<&str> = buttons.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(labels, vec!["Approve", "Deny"]);
    let (id, nonce) = {
        let (_, data) = &buttons[0];
        let mut parts = data.split(':');
        let id = parts.next().unwrap().to_string();
        let nonce = parts.next().unwrap().to_string();
        assert_eq!(parts.next(), Some("approve"));
        (id, nonce)
    };
    assert_eq!(id.len(), 64, "hold id is the sha256 scope key: {id}");
    assert_eq!(nonce.len(), 16, "nonce is 16 hex chars: {nonce}");
    let (_, deny_data) = &buttons[1];
    assert_eq!(deny_data, &format!("{id}:{nonce}:deny"));

    // The nonce on the button matches the one stored in the pending row.
    assert_eq!(row_nonce(&gate, &id).as_deref(), Some(nonce.as_str()));

    // The ask is audited before any decision exists.
    let requests = gate.audit_lines();
    let ask_event = requests
        .iter()
        .find(|l| l["action"] == "approval.request")
        .expect("approval.request audited");
    assert_eq!(ask_event["repo"], REPO_FULL);
    assert_eq!(ask_event["decision"], "pending");

    holder.abort();
}

// ---------------------------------------------------------------------------
// Approve / deny through the real git push harness
// ---------------------------------------------------------------------------

/// Shared setup for the git e2e tests: a local repo whose forced main is
/// advertised upstream at a diverged oid, plus the mounted receive flow.
async fn force_scenario(gate: &Gate, name: &str) -> (PathBuf, String, String) {
    let repo = init_repo(&gate.dir, name);
    let local_tip = rev(&repo, "main");
    mount_receive_flow(
        gate,
        REPO_FULL,
        &[("refs/heads/main", OLD)],
        &format!("/repos/{REPO_FULL}/compare/{OLD}...{local_tip}"),
        r#"{"status":"diverged","commits":[{"sha":"fedcba","commit":{"message":"rewritten\n"}}]}"#,
    )
    .await;
    (repo, OLD.to_string(), local_tip)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approve_button_forwards_held_push_end_to_end() {
    let gate = spawn_gate(Duration::from_secs(20)).await;
    let (repo, _advertised, _tip) = force_scenario(&gate, "approve-e2e").await;

    let url = gate.credentialed_url();
    let push = tokio::task::spawn_blocking(move || run_git(&repo, &["push", "--force", &url, "main"]));

    wait_for(|| !gate.fake.sent().is_empty()).await;
    let (id, nonce) = ask_triple(&gate.fake);
    let ask_ref = msg_ref(1); // first FakeTelegram send

    // The allowlisted approver presses Approve on the ask message itself.
    gate.fake.push_update(callback_update(
        APPROVER,
        &format!("{id}:{nonce}:approve"),
        ask_ref,
    ));

    let (ok, out) = push.await.unwrap();
    assert!(ok, "approved force push must succeed: {out}");

    let requests = gate.upstream_requests().await;
    assert_eq!(
        gate.receive_pack_hits(&requests, OWNER, REPO).len(),
        1,
        "upstream receive-pack POST hit exactly once"
    );

    // Audit: the resolution names the Telegram approver and the outcome.
    let resolved = gate.resolve_audit();
    assert_eq!(resolved.len(), 1, "{:?}", gate.audit_lines());
    assert_eq!(resolved[0]["decision"], "approved");
    assert_eq!(resolved[0]["basis"], "approved");
    assert_eq!(resolved[0]["approver"], "42");
    assert_eq!(resolved[0]["repo"], REPO_FULL);

    // The ask message was edited in place to show decision + approver.
    let edits = gate.fake.edits();
    assert!(
        edits.iter().any(|(to, req)| *to == ask_ref
            && req.text.contains("Approved")
            && req.text.contains("42")),
        "ask edited with decision + approver: {edits:?}"
    );
    assert_eq!(gate.row_outcome(&id), Some(gate::approval::ApprovalOutcome::Approved));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deny_button_rejects_held_push() {
    let gate = spawn_gate(Duration::from_secs(20)).await;
    let (repo, _advertised, _tip) = force_scenario(&gate, "deny-e2e").await;

    let url = gate.credentialed_url();
    let push = tokio::task::spawn_blocking(move || run_git(&repo, &["push", "--force", &url, "main"]));

    wait_for(|| !gate.fake.sent().is_empty()).await;
    let (id, nonce) = ask_triple(&gate.fake);
    let ask_ref = msg_ref(1);

    gate.fake.push_update(callback_update(
        APPROVER,
        &format!("{id}:{nonce}:deny"),
        ask_ref,
    ));

    let (ok, out) = push.await.unwrap();
    assert!(!ok, "denied force push must fail: {out}");
    assert!(
        out.contains("403") || out.contains("rejected"),
        "client must see the rejection: {out}"
    );

    let requests = gate.upstream_requests().await;
    assert!(
        gate.receive_pack_hits(&requests, OWNER, REPO).is_empty(),
        "denied push must NOT reach upstream"
    );

    let resolved = gate.resolve_audit();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0]["decision"], "denied");
    assert_eq!(resolved[0]["basis"], "denied");
    assert_eq!(resolved[0]["approver"], "42");

    let edits = gate.fake.edits();
    assert!(
        edits
            .iter()
            .any(|(to, req)| *to == ask_ref && req.text.contains("Denied") && req.text.contains("42")),
        "ask edited with denial + approver: {edits:?}"
    );
}

// ---------------------------------------------------------------------------
// Timed-out holds never resurrect
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn double_approve_after_timeout_replies_state_without_upstream_hit() {
    let gate = spawn_gate(Duration::from_millis(400)).await;
    mount_receive_flow(
        &gate,
        REPO_FULL,
        &[("refs/heads/main", OLD)],
        &format!("/repos/{REPO_FULL}/compare/{OLD}...{NEW}"),
        r#"{"status":"diverged"}"#,
    )
    .await;

    let status = post_push(&gate, OWNER, REPO, force_push_body(OLD, NEW)).await;
    assert_eq!(status, 403, "timed-out hold rejects the push");

    wait_for(|| !gate.fake.sent().is_empty()).await;
    let (id, nonce) = ask_triple(&gate.fake);
    let ask_ref = msg_ref(1);

    wait_for(|| gate.row_outcome(&id).is_some()).await;
    assert_eq!(
        gate.row_outcome(&id),
        Some(gate::approval::ApprovalOutcome::TimedOut)
    );

    // Late approve press: the button carries the right nonce, but the hold
    // is dead — reply with state, change nothing, hit nothing.
    gate.fake.push_update(callback_update(
        APPROVER,
        &format!("{id}:{nonce}:approve"),
        ask_ref,
    ));
    wait_for(|| {
        gate.fake
            .edits()
            .last()
            .is_some_and(|(_, req)| req.text == TIMED_OUT_TEXT)
    })
    .await;
    assert_eq!(
        gate.row_outcome(&id),
        Some(gate::approval::ApprovalOutcome::TimedOut),
        "late approve must not change the row"
    );

    let requests = gate.upstream_requests().await;
    assert!(
        gate.receive_pack_hits(&requests, OWNER, REPO).is_empty(),
        "no upstream receive-pack hit at any point"
    );
    let resolved = gate.resolve_audit();
    assert!(
        resolved.iter().all(|l| l["decision"] == "timed_out"),
        "no approved/denied resolution was recorded: {resolved:?}"
    );
}

// ---------------------------------------------------------------------------
// Undeliverable asks never leave a resolvable row (review round 1, F1)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_failure_resolves_hold_denied_without_a_waiter() {
    let gate = spawn_gate(Duration::from_secs(30)).await;
    mount_receive_flow(
        &gate,
        REPO_FULL,
        &[("refs/heads/main", OLD)],
        &format!("/repos/{REPO_FULL}/compare/{OLD}...{NEW}"),
        r#"{"status":"diverged"}"#,
    )
    .await;
    gate.fake.fail_next_sends(1);

    let status = post_push(&gate, OWNER, REPO, force_push_body(OLD, NEW)).await;
    assert_eq!(status, 403, "undeliverable ask ⇒ hold denied");

    // The row was rolled back: exactly one row exists and it is already
    // Denied — NOT pending, so /pending Resume cannot resurrect it.
    let rows = pending_outcomes(&gate);
    assert_eq!(rows.len(), 1, "the persisted row: {rows:?}");
    assert_eq!(
        rows[0].1,
        Some(gate::approval::ApprovalOutcome::Denied),
        "send failure must resolve the row Denied, never leave it pending"
    );
    assert!(
        gate.fake.sent().is_empty(),
        "failed send records no outbound message"
    );

    // The rollback and the delivery failure itself are audited.
    let lines = gate.audit_lines();
    let resolve_event = lines
        .iter()
        .find(|l| l["action"] == "approval.resolve")
        .expect("rollback resolution audited");
    assert_eq!(resolve_event["decision"], "denied");
    assert_eq!(resolve_event["basis"], "delivery-failed");
    let delivery = lines
        .iter()
        .find(|l| l["action"] == "approval.delivery")
        .expect("delivery failure audited");
    assert_eq!(delivery["decision"], "deny");
    assert!(
        delivery["basis"]
            .as_str()
            .unwrap()
            .contains("simulated telegram outage"),
        "basis names the (already token-free) transport error: {delivery:?}"
    );

    // The git proxy saw present Err ⇒ deny.
    let pushes: Vec<_> = lines
        .iter()
        .filter(|l| l["action"] == "git.receive-pack")
        .collect();
    let decisions: Vec<&str> = pushes
        .iter()
        .map(|p| p["decision"].as_str().unwrap())
        .collect();
    assert_eq!(decisions, vec!["hold", "deny"]);
    assert_eq!(pushes[1]["basis"], "approval backend unavailable");

    let requests = gate.upstream_requests().await;
    assert!(
        gate.receive_pack_hits(&requests, OWNER, REPO).is_empty(),
        "nothing forwards upstream"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_chat_configured_persists_nothing_and_denies() {
    let gate = spawn_gate_with_chat(Duration::from_secs(30), None).await;
    mount_receive_flow(
        &gate,
        REPO_FULL,
        &[("refs/heads/main", OLD)],
        &format!("/repos/{REPO_FULL}/compare/{OLD}...{NEW}"),
        r#"{"status":"diverged"}"#,
    )
    .await;

    let status = post_push(&gate, OWNER, REPO, force_push_body(OLD, NEW)).await;
    assert_eq!(status, 403, "undeliverable ask ⇒ hold denied");

    // Nothing was persisted at all: the chat check runs BEFORE the insert.
    assert!(
        pending_outcomes(&gate).is_empty(),
        "no chat ⇒ zero pending rows: {:?}",
        pending_outcomes(&gate)
    );
    assert!(gate.fake.sent().is_empty());

    // No approval lifecycle events either — the hold never existed.
    let lines = gate.audit_lines();
    assert!(
        !lines
            .iter()
            .any(|l| l["action"].as_str().unwrap().starts_with("approval.")),
        "no approval rows ⇒ no approval audit events: {lines:?}"
    );
    let pushes: Vec<_> = lines
        .iter()
        .filter(|l| l["action"] == "git.receive-pack")
        .collect();
    assert_eq!(pushes[0]["decision"], "hold");
    assert_eq!(pushes[1]["decision"], "deny");
    assert_eq!(pushes[1]["basis"], "approval backend unavailable");

    let requests = gate.upstream_requests().await;
    assert!(
        gate.receive_pack_hits(&requests, OWNER, REPO).is_empty(),
        "zero upstream receive-pack calls"
    );
}

// ---------------------------------------------------------------------------
// Kill switch short-circuits before any Telegram ask
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn block_command_denies_holds_without_telegram_ask() {
    let gate = spawn_gate(Duration::from_secs(30)).await;

    // /block acme/api via the control plane.
    gate.fake
        .push_update(text_update(APPROVER, "/block acme/api", msg_ref(55)));
    wait_for(|| {
        gate.state
            .lock()
            .unwrap()
            .is_blocked("acme/api")
            .unwrap_or(false)
    })
    .await;
    let edits = gate.fake.edits();
    assert!(
        edits
            .iter()
            .any(|(to, req)| *to == msg_ref(55) && req.text.contains("Blocked acme/api")),
        "confirmation edit: {edits:?}"
    );
    let switched = gate.audit_lines();
    assert!(
        switched
            .iter()
            .any(|l| l["action"] == "killswitch.set" && l["decision"] == "blocked"),
        "kill switch flip audited: {switched:?}"
    );

    // A push that WOULD hold on acme/api: denied before any ancestry call
    // and before any Telegram ask.
    Mock::given(method("GET"))
        .and(path("/acme/api.git/info/refs"))
        .and(query_param("service", "git-receive-pack"))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("content-type", "application/x-git-receive-pack-advertisement")
                .set_body_bytes(receive_advertisement(&[("refs/heads/main", OLD)])),
        )
        .mount(&gate.upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/user/memberships/orgs/acme"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"state":"active"}"#))
        .mount(&gate.upstream)
        .await;

    let status = post_push(&gate, "acme", "api", force_push_body(OLD, NEW)).await;
    assert_eq!(status, 403, "kill-switched repo denies the push");

    assert!(
        gate.fake.sent().is_empty(),
        "kill switch short-circuits BEFORE the approval ask: {:?}",
        gate.fake.sent()
    );
    let requests = gate.upstream_requests().await;
    assert!(
        gate.receive_pack_hits(&requests, "acme", "api").is_empty(),
        "nothing reaches upstream receive-pack"
    );
    let pushes = gate
        .audit_lines()
        .into_iter()
        .filter(|l| l["action"] == "git.receive-pack" && l["repo"] == "acme/api")
        .collect::<Vec<_>>();
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["decision"], "deny");
    assert_eq!(pushes[0]["basis"], "kill-switch");
}

// ---------------------------------------------------------------------------
// /pending and /status
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_command_lists_one_open_hold_with_resume_cancel() {
    let gate = spawn_gate(Duration::from_secs(30)).await;

    let req = ApprovalRequest {
        id: "hold-pending-1".to_string(),
        agent: AGENT.to_string(),
        repo: REPO_FULL.to_string(),
        class: ActionClass::ForcePush,
        detail: ApprovalDetail::Api {
            method: "DELETE".to_string(),
            path: "/repos/octo/hello/hooks/1".to_string(),
            body_snippet: "{}".to_string(),
            body_sha: "ab12cd34".to_string(),
        },
        timeout: Duration::from_secs(300),
    };
    gate.backend.present(&req).await.unwrap();

    gate.fake
        .push_update(text_update(APPROVER, "/pending", msg_ref(77)));
    wait_for(|| {
        gate.fake
            .edits()
            .iter()
            .any(|(to, _)| *to == msg_ref(77))
    })
    .await;

    let (_, reply) = gate
        .fake
        .edits()
        .into_iter()
        .find(|(to, _)| *to == msg_ref(77))
        .unwrap();
    assert!(
        reply.text.contains("hold-pending-1")
            && reply.text.contains(REPO_FULL)
            && reply.text.contains("force_push"),
        "/pending lists id, repo, class: {}",
        reply.text
    );
    let (resume, cancel) = {
        let find = |label: &str| {
            reply
                .inline_buttons
                .iter()
                .find(|(l, _)| l == label)
                .map(|(_, d)| d.clone())
                .expect(label)
        };
        (find("Resume"), find("Cancel"))
    };
    let nonce = row_nonce(&gate, "hold-pending-1").unwrap();
    assert_eq!(resume, format!("hold-pending-1:{nonce}:resume"));
    assert_eq!(cancel, format!("hold-pending-1:{nonce}:cancel"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_resume_and_cancel_manage_holds() {
    let gate = spawn_gate(Duration::from_secs(30)).await;
    let req = ApprovalRequest {
        id: "hold-resume-1".to_string(),
        agent: AGENT.to_string(),
        repo: REPO_FULL.to_string(),
        class: ActionClass::Push,
        detail: ApprovalDetail::Push {
            updates: vec![],
            commits: vec![],
        },
        timeout: Duration::from_secs(300),
    };
    gate.backend.present(&req).await.unwrap();
    let sent_before = gate.fake.sent().len();
    let nonce = row_nonce(&gate, "hold-resume-1").unwrap();

    // Resume re-delivers the ask with the same signed buttons.
    gate.fake.push_update(callback_update(
        APPROVER,
        &format!("hold-resume-1:{nonce}:resume"),
        msg_ref(99),
    ));
    wait_for(|| gate.fake.sent().len() > sent_before).await;
    let buttons = gate.fake.sent()[sent_before].inline_buttons.clone();
    assert!(
        buttons.iter().any(|(label, data)| label == "Approve"
            && data == &format!("hold-resume-1:{nonce}:approve")),
        "resume re-sends the signed Approve button: {buttons:?}"
    );

    // Cancel resolves the hold as denied (with the Telegram approver).
    gate.fake.push_update(callback_update(
        APPROVER,
        &format!("hold-resume-1:{nonce}:cancel"),
        msg_ref(99),
    ));
    wait_for(|| gate.row_outcome("hold-resume-1").is_some()).await;
    assert_eq!(
        gate.row_outcome("hold-resume-1"),
        Some(gate::approval::ApprovalOutcome::Denied)
    );
    let resolved = gate.resolve_audit();
    assert_eq!(resolved[0]["decision"], "denied");
    assert_eq!(resolved[0]["approver"], "42");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_command_renders_registry_forwarding_and_flows() {
    let gate = spawn_gate(Duration::from_secs(30)).await;
    gate.state
        .lock()
        .unwrap()
        .registry_upsert(REPO_FULL, "admin", "admin", "device-flow")
        .unwrap();
    gate.state
        .lock()
        .unwrap()
        .flow_insert("flow-1", "elevation", "DETECTED", r#"{"repo":"octo/hello"}"#)
        .unwrap();

    gate.fake
        .push_update(text_update(APPROVER, "/status", msg_ref(66)));
    wait_for(|| gate.fake.edits().iter().any(|(to, _)| *to == msg_ref(66))).await;

    let (_, reply) = gate
        .fake
        .edits()
        .into_iter()
        .find(|(to, _)| *to == msg_ref(66))
        .unwrap();
    let text = reply.text;
    assert!(text.contains("forwarding: enabled"), "status text: {text}");
    assert!(text.contains("agent tokens: 1"), "status text: {text}");
    assert!(
        text.contains(REPO_FULL) && text.contains("admin") && text.contains("device-flow"),
        "registry row rendered: {text}"
    );
    assert!(text.contains("open flows: 1"), "status text: {text}");
    assert!(
        !text.contains(MACHINE_TOKEN),
        "no machine token ever appears in a reply: {text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_or_argless_commands_get_usage_replies() {
    let gate = spawn_gate(Duration::from_secs(30)).await;

    gate.fake
        .push_update(text_update(APPROVER, "/block", msg_ref(11)));
    wait_for(|| gate.fake.edits().iter().any(|(to, _)| *to == msg_ref(11))).await;
    let (_, reply) = gate
        .fake
        .edits()
        .into_iter()
        .find(|(to, _)| *to == msg_ref(11))
        .unwrap();
    assert!(
        reply.text.contains("Usage: /block owner/repo"),
        "missing arg ⇒ usage: {}",
        reply.text
    );
    assert!(
        !gate.state.lock().unwrap().is_blocked("anything").unwrap(),
        "no repo was blocked by the argless command"
    );

    gate.fake
        .push_update(text_update(APPROVER, "/frobnicate", msg_ref(12)));
    wait_for(|| gate.fake.edits().iter().any(|(to, _)| *to == msg_ref(12))).await;
    let (_, reply) = gate
        .fake
        .edits()
        .into_iter()
        .find(|(to, _)| *to == msg_ref(12))
        .unwrap();
    assert!(
        reply.text.contains("Unknown command"),
        "unknown command ⇒ short usage: {}",
        reply.text
    );
}
