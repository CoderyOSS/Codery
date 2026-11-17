//! Integration: the git smart-HTTP proxy end-to-end — real `git` client
//! through the gate against a wiremock upstream. Covers clone passthrough
//! with machine Basic auth, fast-forward push forwarding, held force-pushes
//! resolved by the stub approval backend, byte-identical forwarding, and
//! fail-closed parse errors.

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use gate::approval::{ApprovalOutcome, StubBackend};
use gate::audit::Audit;
use gate::config::DEFAULT_MAX_BODY_BYTES;
use gate::github::GitHubPlatform;
use gate::gitproxy::GitDeps;
use gate::policy::{PolicyEngine, PolicyFile};
use gate::routes::{router, GateState};
use gate::state::State as DbState;
use gate::tokens::TokenStore;

const MACHINE_LOGIN: &str = "gate-machine";
const MACHINE_TOKEN: &str = "test-machine-token";
const AGENT: &str = "claude-code";
const OWNER: &str = "octo";
const REPO: &str = "hello";
const REPO_FULL: &str = "octo/hello";

const APPROVAL_TIMEOUT: Duration = Duration::from_secs(3);

const POLICY: &str = "\
[defaults]
push = \"allow\"
force-push = \"approve\"
branch-delete = \"approve\"
tag-delete = \"approve\"
";

/// One pkt-line: 4 lowercase-hex length bytes then the payload.
fn pkt(payload: &str) -> Vec<u8> {
    let mut raw = format!("{:04x}", 4 + payload.len()).into_bytes();
    raw.extend_from_slice(payload.as_bytes());
    raw
}

fn flush() -> Vec<u8> {
    b"0000".to_vec()
}

struct Gate {
    port: u16,
    token: String,
    upstream: MockServer,
    audit_path: PathBuf,
    dir: PathBuf,
}

impl Drop for Gate {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
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

    fn push_audit(&self) -> Vec<serde_json::Value> {
        self.audit_lines()
            .into_iter()
            .filter(|l| l["action"] == "git.receive-pack")
            .collect()
    }

    async fn upstream_requests(&self) -> Vec<wiremock::Request> {
        self.upstream.received_requests().await.unwrap()
    }

    fn receive_pack_hits<'a>(
        &self,
        requests: &'a [wiremock::Request],
    ) -> Vec<&'a wiremock::Request> {
        requests
            .iter()
            .filter(|r| {
                r.method == wiremock::http::Method::POST
                    && r.url.path() == &format!("/{OWNER}/{REPO}.git/git-receive-pack")
            })
            .collect()
    }
}

async fn spawn_gate(auto: ApprovalOutcome) -> Gate {
    spawn_gate_with_limit(auto, DEFAULT_MAX_BODY_BYTES).await
}

/// Variant with a test-chosen body cap (final-review fix 4).
async fn spawn_gate_with_limit(auto: ApprovalOutcome, max_body_bytes: usize) -> Gate {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "gate-gitproxy-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let upstream = MockServer::start().await;
    // The machine account is an active member of the owning org.
    Mock::given(method("GET"))
        .and(path(format!("/user/memberships/orgs/{OWNER}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"state":"active"}"#),
        )
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

    let token_getter: Arc<dyn Fn() -> Option<String> + Send + Sync> =
        Arc::new(|| Some(MACHINE_TOKEN.to_string()));
    let base = reqwest::Url::parse(&upstream.uri()).unwrap();
    let platform = Arc::new(GitHubPlatform::with_bases(
        base.clone(),
        base,
        token_getter.clone(),
    ));
    let policy = PolicyEngine::new(&PolicyFile::parse(POLICY).unwrap());
    let backend = Arc::new(StubBackend::new(state.clone(), audit.clone(), auto).unwrap());
    let git = GitDeps::new(
        state.clone(),
        platform,
        policy,
        backend,
        MACHINE_LOGIN.to_string(),
        token_getter,
        APPROVAL_TIMEOUT,
    )
    .unwrap();

    let mut gate_state = GateState::with_git(state, audit, git);
    gate_state.max_body_bytes = max_body_bytes;
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

    Gate {
        port,
        token,
        upstream,
        audit_path,
        dir,
    }
}

/// Run git with the gate-friendly env: no prompts, no system/global config,
/// protocol v0 so the canned v0 upstream advertisements apply.
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

/// stdout-only variant for commands whose output is parsed.
fn run_git_out(dir: &Path, args: &[&str]) -> (bool, String) {
    let output = Command::new("git")
        .args(["-c", "protocol.version=0", "-c", "init.defaultBranch=main"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(dir)
        .output()
        .expect("git runs");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

/// Fresh repo with one commit on `main`.
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
    let (ok, out) = run_git_out(repo, &["rev-parse", rev]);
    assert!(ok, "rev-parse {rev}");
    out.trim().to_string()
}

/// Two-commit repo: main~1 and main, a genuine fast-forward pair.
fn ff_repo(dir: &Path, name: &str) -> (PathBuf, String, String) {
    let repo = init_repo(dir, name);
    std::fs::write(repo.join("file.txt"), "hello\nworld\n").unwrap();
    let (ok, out) = run_git(&repo, &["add", "."]);
    assert!(ok, "add: {out}");
    let (ok, out) = run_git(&repo, &["commit", "-m", "second commit"]);
    assert!(ok, "commit: {out}");
    let old = rev(&repo, "main~1");
    let new = rev(&repo, "main");
    (repo, old, new)
}

fn pack_bytes(repo: &Path) -> Vec<u8> {
    let mut child = Command::new("git")
        .args(["pack-objects", "--stdout", "--revs"])
        .current_dir(repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("pack-objects spawns");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"HEAD\n")
        .unwrap();
    let out = child.wait_with_output().expect("pack-objects waits");
    assert!(
        out.status.success(),
        "pack-objects: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// Smart-HTTP v0 ref advertisement for receive-pack with `refs` advertised
/// (capabilities ride the first line).
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

/// Minimal valid report-status result — framing computed here: "unpack
/// ok\n" (10 payload bytes) → 000e; "ok refs/heads/main\n" (19 bytes) → 0017.
fn report_status_ok(refname: &str) -> Vec<u8> {
    let mut b = pkt("unpack ok\n");
    b.extend(pkt(&format!("ok {refname}\n")));
    b.extend_from_slice(&flush());
    b
}

async fn mount_receive_flow(gate: &Gate, refs: &[(&str, &str)], compare_path: &str, compare_body: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/{OWNER}/{REPO}.git/info/refs")))
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
        .and(path(format!("/{OWNER}/{REPO}.git/git-receive-pack")))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("content-type", "application/x-git-receive-pack-result")
                .set_body_bytes(report_status_ok("refs/heads/main")),
        )
        .mount(&gate.upstream)
        .await;
}

fn machine_auth_header() -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD
            .encode(format!("{MACHINE_LOGIN}:{MACHINE_TOKEN}"))
    )
}

fn agent_auth_header(token: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{AGENT}:{token}"))
    )
}

/// Raw HTTP/1.1 GET straight on the socket: reqwest would normalize bare
/// `.` path segments away before sending, so the grammar test needs the
/// exact bytes on the wire. Returns the response status code.
fn raw_get_status(port: u16, path: &str, auth: &str) -> u16 {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))
        .expect("gate listens");
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: {auth}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).expect("request written");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).expect("response read");
    let text = String::from_utf8_lossy(&buf);
    text.split_whitespace()
        .nth(1)
        .unwrap_or_else(|| panic!("no status line: {text}"))
        .parse()
        .expect("status code parses")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_via_gate_authenticates_upstream_as_machine_account_only() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;
    let work = gate.dir.join("clone-work");
    std::fs::create_dir_all(&work).unwrap();
    let seed = init_repo(&work, "seed");
    let head = rev(&seed, "main");
    let pack = pack_bytes(&seed);

    let mut adv = pkt("# service=git-upload-pack\n");
    adv.extend_from_slice(&flush());
    adv.extend(pkt(&format!(
        "{head} HEAD\0multi_ack_detailed multi_ack thin-pack ofs-delta object-format=sha1 symref=HEAD:refs/heads/main agent=gate-test\n"
    )));
    adv.extend(pkt(&format!("{head} refs/heads/main\n")));
    adv.extend_from_slice(&flush());

    // A real `upload-pack --stateless-rpc` reply ends after the pack — no
    // trailing flush (verified against the local git binary).
    let mut result = b"0008NAK\n".to_vec();
    result.extend_from_slice(&pack);

    Mock::given(method("GET"))
        .and(path(format!("/{OWNER}/{REPO}.git/info/refs")))
        .and(query_param("service", "git-upload-pack"))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("content-type", "application/x-git-upload-pack-advertisement")
                .set_body_bytes(adv),
        )
        .mount(&gate.upstream)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/{OWNER}/{REPO}.git/git-upload-pack")))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("content-type", "application/x-git-upload-pack-result")
                .set_body_bytes(result),
        )
        .mount(&gate.upstream)
        .await;

    let target = work.join("cloned");
    let (ok, out) = run_git(
        &work,
        &["clone", &gate.credentialed_url(), target.to_str().unwrap()],
    );
    assert!(ok, "clone through gate failed: {out}");
    let cloned = rev(&target, "HEAD");
    assert_eq!(cloned, head, "clone got the advertised commit");

    let requests = gate.upstream_requests().await;
    assert!(!requests.is_empty(), "upstream saw the clone");
    let expected_auth = machine_auth_header();

    let refs = requests
        .iter()
        .find(|r| r.url.path() == &format!("/{OWNER}/{REPO}.git/info/refs"))
        .expect("info/refs request reached upstream");
    assert_eq!(
        refs.headers.get("authorization").map(|v| v.to_str().unwrap()),
        Some(expected_auth.as_str()),
        "upstream must see Basic machine auth, exactly"
    );
    assert_eq!(
        refs.url.path(),
        format!("/{OWNER}/{REPO}.git/info/refs"),
        "no machine PAT in the upstream path"
    );

    // The agent's own token never reached upstream — not in any header,
    // path, or body.
    for r in &requests {
        let headers = format!("{:?}", r.headers);
        let body = String::from_utf8_lossy(&r.body).into_owned();
        let url = r.url.as_str();
        assert!(
            !headers.contains(&gate.token) && !body.contains(&gate.token) && !url.contains(&gate.token),
            "agent token leaked to upstream at {}: {headers}",
            r.url.path()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fast_forward_push_forwards_to_upstream() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;
    let (repo, old, new) = ff_repo(&gate.dir, "ff");

    mount_receive_flow(
        &gate,
        &[("refs/heads/main", old.as_str())],
        &format!("/repos/{REPO_FULL}/compare/{old}...{new}"),
        r#"{"status":"ahead"}"#,
    )
    .await;

    let (ok, out) = run_git(&repo, &["push", &gate.credentialed_url(), "main"]);
    assert!(ok, "fast-forward push failed: {out}");

    let requests = gate.upstream_requests().await;
    assert_eq!(
        gate.receive_pack_hits(&requests).len(),
        1,
        "upstream receive-pack POST hit exactly once"
    );

    let pushes = gate.push_audit();
    assert_eq!(pushes.len(), 1, "one audited push decision: {:?}", pushes);
    assert_eq!(pushes[0]["decision"], "forward");
    assert_eq!(pushes[0]["agent"], AGENT);
    assert_eq!(pushes[0]["repo"], REPO_FULL);
    let basis = pushes[0]["basis"].as_str().unwrap();
    assert!(basis.contains("policy: allow"), "basis: {basis}");
    assert!(basis.contains("push"), "classes in basis: {basis}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_push_denied_by_stub_never_reaches_upstream() {
    let gate = spawn_gate(ApprovalOutcome::Denied).await;
    let (repo, advertised, local_tip) = {
        let repo = init_repo(&gate.dir, "force-deny");
        let tip = rev(&repo, "main");
        // Upstream advertises an oid the client does not have: the push is
        // a forced replacement.
        let advertised = "1234567890abcdef1234567890abcdef12345678";
        (repo, advertised.to_string(), tip)
    };

    mount_receive_flow(
        &gate,
        &[("refs/heads/main", advertised.as_str())],
        &format!("/repos/{REPO_FULL}/compare/{advertised}...{local_tip}"),
        r#"{"status":"diverged","commits":[{"sha":"fedcba","commit":{"message":"rewritten\n"}}]}"#,
    )
    .await;

    let (ok, out) = run_git(&repo, &["push", "--force", &gate.credentialed_url(), "main"]);
    assert!(!ok, "denied force push must fail: {out}");
    assert!(
        out.contains("403") || out.contains("rejected"),
        "client must see the rejection: {out}"
    );

    let requests = gate.upstream_requests().await;
    assert!(
        gate.receive_pack_hits(&requests).is_empty(),
        "denied push must NOT reach upstream receive-pack"
    );

    let pushes = gate.push_audit();
    let decisions: Vec<&str> = pushes
        .iter()
        .map(|p| p["decision"].as_str().unwrap())
        .collect();
    assert_eq!(
        decisions,
        vec!["hold", "deny"],
        "hold recorded, then the denial: {pushes:?}"
    );
    assert!(
        pushes[0]["basis"].as_str().unwrap().contains("policy: approve"),
        "{:?}",
        pushes[0]
    );
    assert!(
        pushes[0]["basis"].as_str().unwrap().contains("force_push"),
        "{:?}",
        pushes[0]
    );
    assert_eq!(pushes[1]["basis"], "approval: denied", "{:?}", pushes[1]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_push_auto_approved_reaches_upstream() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;
    let (repo, advertised, local_tip) = {
        let repo = init_repo(&gate.dir, "force-approve");
        let tip = rev(&repo, "main");
        let advertised = "1234567890abcdef1234567890abcdef12345678";
        (repo, advertised.to_string(), tip)
    };

    mount_receive_flow(
        &gate,
        &[("refs/heads/main", advertised.as_str())],
        &format!("/repos/{REPO_FULL}/compare/{advertised}...{local_tip}"),
        r#"{"status":"diverged","commits":[{"sha":"fedcba","commit":{"message":"rewritten\n"}}]}"#,
    )
    .await;

    let (ok, out) = run_git(&repo, &["push", "--force", &gate.credentialed_url(), "main"]);
    assert!(ok, "approved force push must succeed: {out}");

    let requests = gate.upstream_requests().await;
    assert_eq!(
        gate.receive_pack_hits(&requests).len(),
        1,
        "approved hold forwards to upstream receive-pack"
    );

    let pushes = gate.push_audit();
    let decisions: Vec<&str> = pushes
        .iter()
        .map(|p| p["decision"].as_str().unwrap())
        .collect();
    assert_eq!(
        decisions,
        vec!["hold", "forward"],
        "hold recorded, then the approved forward: {pushes:?}"
    );
    assert_eq!(pushes[1]["basis"], "approval: approved", "{:?}", pushes[1]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approved_hold_forwards_body_byte_identical() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;

    let old = "1234567890abcdef1234567890abcdef12345678";
    let new = "fedcba0987654321fedcba0987654321fedcba09";
    mount_receive_flow(
        &gate,
        &[("refs/heads/main", old)],
        &format!("/repos/{REPO_FULL}/compare/{old}...{new}"),
        r#"{"status":"diverged"}"#,
    )
    .await;

    let mut body = pkt(&format!("{old} {new} refs/heads/main\0report-status"));
    body.extend_from_slice(&flush());
    body.extend_from_slice(b"PACK-fake-packfile-bytes-0123456789");

    let client = reqwest::Client::new();
    let agent_auth = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD
            .encode(format!("{AGENT}:{}", gate.token))
    );
    let resp = client
        .post(format!("{}/{OWNER}/{REPO}.git/git-receive-pack", gate.base_url()))
        .header("authorization", agent_auth)
        .header("content-type", "application/x-git-receive-pack-request")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "approved hold forwards");

    let requests = gate.upstream_requests().await;
    let hits = gate.receive_pack_hits(&requests);
    assert_eq!(hits.len(), 1, "exactly one upstream receive-pack POST");
    let upstream = hits[0];
    assert_eq!(
        upstream.body, body,
        "upstream must receive the exact bytes the gate received"
    );
    assert_eq!(
        upstream.headers.get("authorization").map(|v| v.to_str().unwrap()),
        Some(machine_auth_header().as_str()),
        "forwarded request uses machine Basic auth"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unparseable_receive_body_is_403_without_upstream() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;

    let client = reqwest::Client::new();
    let agent_auth = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD
            .encode(format!("{AGENT}:{}", gate.token))
    );
    let resp = client
        .post(format!("{}/{OWNER}/{REPO}.git/git-receive-pack", gate.base_url()))
        .header("authorization", agent_auth)
        .header("content-type", "application/x-git-receive-pack-request")
        .body(b"garbage that is not pkt-line".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403, "parse failure denies");
    let headers = resp.headers().clone();
    let bytes = resp.bytes().await.unwrap();
    assert_eq!(
        headers.get("content-type").map(|v| v.to_str().unwrap()),
        Some("application/x-git-receive-pack-result"),
        "git-friendly content type"
    );
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("ERR"), "ERR payload for git clients: {text}");
    assert!(bytes.ends_with(b"0000"), "flush-terminated body");
    assert_eq!(&bytes[..4], b"000d", "length header computed: {text}");

    let requests = gate.upstream_requests().await;
    assert!(
        gate.receive_pack_hits(&requests).is_empty(),
        "unparseable body must NOT reach upstream"
    );

    let pushes = gate.push_audit();
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["decision"], "deny");
    assert!(
        pushes[0]["basis"].as_str().unwrap().contains("parse-error"),
        "{:?}",
        pushes[0]
    );
}

// ---------------------------------------------------------------------------
// Git reads are audited (final-review fix 2, GATE-005)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_reads_are_audited_forward_per_request() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;
    let head = "1234567890abcdef1234567890abcdef12345678";

    let mut adv = pkt("# service=git-upload-pack\n");
    adv.extend_from_slice(&flush());
    adv.extend(pkt(&format!(
        "{head} HEAD\0multi_ack_detailed ofs-delta object-format=sha1 symref=HEAD:refs/heads/main agent=gate-test\n"
    )));
    adv.extend(pkt(&format!("{head} refs/heads/main\n")));
    adv.extend_from_slice(&flush());

    Mock::given(method("GET"))
        .and(path(format!("/{OWNER}/{REPO}.git/info/refs")))
        .and(query_param("service", "git-upload-pack"))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("content-type", "application/x-git-upload-pack-advertisement")
                .set_body_bytes(adv),
        )
        .mount(&gate.upstream)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/{OWNER}/{REPO}.git/git-upload-pack")))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("content-type", "application/x-git-upload-pack-result")
                .set_body_bytes(b"0008NAK\n".to_vec()),
        )
        .mount(&gate.upstream)
        .await;

    let auth = agent_auth_header(&gate.token);
    let client = reqwest::Client::new();

    let refs = client
        .get(format!(
            "{}/{OWNER}/{REPO}.git/info/refs?service=git-upload-pack",
            gate.base_url()
        ))
        .header("authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(refs.status(), 200);

    let up = client
        .post(format!(
            "{}/{OWNER}/{REPO}.git/git-upload-pack",
            gate.base_url()
        ))
        .header("authorization", &auth)
        .header("content-type", "application/x-git-upload-pack-request")
        .body(flush())
        .send()
        .await
        .unwrap();
    assert_eq!(up.status(), 200);

    // One audit record per read request, matching the audit_push shape.
    let lines = gate.audit_lines();
    let refs_audit = lines
        .iter()
        .find(|l| l["action"] == "git.info-refs")
        .expect("info/refs request is audited");
    assert_eq!(refs_audit["decision"], "forward");
    assert_eq!(refs_audit["agent"], AGENT);
    assert_eq!(refs_audit["repo"], REPO_FULL);
    assert!(
        refs_audit["basis"]
            .as_str()
            .unwrap()
            .contains("service=git-upload-pack"),
        "{refs_audit:?}"
    );

    let up_audit = lines
        .iter()
        .find(|l| l["action"] == "git.upload-pack")
        .expect("upload-pack request is audited");
    assert_eq!(up_audit["decision"], "forward");
    assert_eq!(up_audit["agent"], AGENT);
    assert_eq!(up_audit["repo"], REPO_FULL);
    assert_eq!(
        up_audit["basis"], "service=git-upload-pack",
        "{up_audit:?}"
    );

    assert_eq!(
        lines
            .iter()
            .filter(|l| l["action"] == "git.info-refs" || l["action"] == "git.upload-pack")
            .count(),
        2,
        "exactly one record per read request"
    );
}

// ---------------------------------------------------------------------------
// Body cap (final-review fix 4)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn over_limit_receive_pack_body_is_413_github_shaped_without_upstream() {
    let gate = spawn_gate_with_limit(ApprovalOutcome::Approved, 1024).await;

    let body = vec![b'x'; 2048]; // over the 1 KiB test cap
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "{}/{OWNER}/{REPO}.git/git-receive-pack",
            gate.base_url()
        ))
        .header("authorization", agent_auth_header(&gate.token))
        .header("content-type", "application/x-git-receive-pack-request")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
    let v: serde_json::Value = serde_json::from_slice(&resp.bytes().await.unwrap()).unwrap();
    assert_eq!(v["message"], "Payload too large");

    let requests = gate.upstream_requests().await;
    assert!(
        requests.is_empty(),
        "over-limit push must not reach upstream: {requests:?}"
    );

    let pushes = gate.push_audit();
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0]["decision"], "deny");
    assert_eq!(pushes[0]["basis"], "payload too large");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn under_limit_receive_pack_body_still_forwards() {
    let gate = spawn_gate_with_limit(ApprovalOutcome::Approved, 64 * 1024).await;

    let old = "1234567890abcdef1234567890abcdef12345678";
    let new = "fedcba0987654321fedcba0987654321fedcba09";
    mount_receive_flow(
        &gate,
        &[("refs/heads/main", old)],
        &format!("/repos/{REPO_FULL}/compare/{old}...{new}"),
        r#"{"status":"diverged"}"#,
    )
    .await;

    let mut body = pkt(&format!("{old} {new} refs/heads/main\0report-status"));
    body.extend_from_slice(&flush());

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "{}/{OWNER}/{REPO}.git/git-receive-pack",
            gate.base_url()
        ))
        .header("authorization", agent_auth_header(&gate.token))
        .header("content-type", "application/x-git-receive-pack-request")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "under-limit push still flows");
    assert_eq!(gate.receive_pack_hits(&gate.upstream_requests().await).len(), 1);
}

// ---------------------------------------------------------------------------
// Path grammar: bare `.` segments (final-review fix 5)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_dot_segments_are_rejected_with_400() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;
    let auth = agent_auth_header(&gate.token);
    // A bare `.` segment survives the old grammar but URL
    // normalization desyncs the gated repo key from the upstream
    // path — it must 400 like `..` does. `.git`-as-repo (empty after
    // the suffix strip) likewise.
    for path in [
        format!("/git/./{REPO}.git/info/refs?service=git-upload-pack"),
        format!("/git/{OWNER}/./info/refs?service=git-upload-pack"),
        format!("/git/{OWNER}/.git/info/refs?service=git-upload-pack"),
    ] {
        let status = raw_get_status(gate.port, &path, &auth);
        assert_eq!(status, 400, "path: {path}");
    }
    // Nothing reached upstream.
    let requests = gate.upstream_requests().await;
    assert!(requests.is_empty(), "{requests:?}");
}
