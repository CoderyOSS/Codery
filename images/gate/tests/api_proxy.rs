//! Integration: the API proxy (task 8) — EVERY GitHub-API call an agent
//! makes flows through `ANY /api/v3/*path`: reads pass through and are
//! audited, mutations are policy-gated and held for approval, the machine
//! credential is verified at request time before anything is forwarded,
//! and upstream failures map to the GATE-010 agent error shapes.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use reqwest::StatusCode;
use sha2::{Digest, Sha256};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use gate::approval::{ApprovalOutcome, StubBackend};
use gate::apiproxy::ApiDeps;
use gate::audit::Audit;
use gate::config::DEFAULT_MAX_BODY_BYTES;
use gate::github::{GitHubPlatform, Platform};
use gate::policy::{PolicyEngine, PolicyFile};
use gate::routes::{router, GateState, RateLimiter};
use gate::state::State as DbState;
use gate::tokens::TokenStore;

const AGENT: &str = "claude-code";
const MACHINE_TOKEN: &str = "test-machine-token";
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(3);

const DEFAULT_POLICY: &str = "\
[defaults]
push = \"allow\"
force-push = \"approve\"
branch-delete = \"approve\"
tag-delete = \"approve\"
";

struct Gate {
    port: u16,
    agent_token: String,
    upstream: MockServer,
    audit_path: PathBuf,
    dir: PathBuf,
    state: Arc<Mutex<DbState>>,
}

impl Drop for Gate {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Gate {
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    async fn send(&self, method: &str, url_path: &str, body: &[u8]) -> reqwest::Response {
        self.send_with(method, url_path, body, &[]).await
    }

    async fn send_with(
        &self,
        method: &str,
        url_path: &str,
        body: &[u8],
        headers: &[(&str, &str)],
    ) -> reqwest::Response {
        let auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{AGENT}:{}", self.agent_token))
        );
        let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut rb = reqwest::Client::new()
            .request(m, self.url(url_path))
            .header("authorization", auth);
        for (k, v) in headers {
            rb = rb.header(*k, *v);
        }
        if !body.is_empty() {
            rb = rb.body(body.to_vec());
        }
        rb.send().await.unwrap()
    }

    fn audit_lines(&self) -> Vec<serde_json::Value> {
        let contents = std::fs::read_to_string(&self.audit_path).unwrap();
        contents
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn api_audit(&self) -> Vec<serde_json::Value> {
        self.audit_lines()
            .into_iter()
            .filter(|l| l["action"] == "api.request")
            .collect()
    }

    async fn upstream_requests(&self) -> Vec<wiremock::Request> {
        self.upstream.received_requests().await.unwrap()
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.dir.join("gate.db")).unwrap()
    }

    fn pending_rows(&self) -> Vec<(String, String, String)> {
        let db = self.db();
        let mut stmt = db
            .prepare("SELECT id, kind, payload_json FROM pending")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        drop(stmt);
        db.close().ok();
        rows
    }

    fn flows(&self) -> Vec<(String, String, String)> {
        let db = self.db();
        let mut stmt = db.prepare("SELECT id, kind, state FROM flows").unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        drop(stmt);
        db.close().ok();
        rows
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
}

async fn spawn_gate_with(
    auto: ApprovalOutcome,
    policy_src: &str,
    machine_token: Option<&str>,
) -> Gate {
    spawn_gate_full(auto, policy_src, machine_token, DEFAULT_MAX_BODY_BYTES).await
}

/// Variant with a test-chosen body cap (final-review fix 4).
async fn spawn_gate_full(
    auto: ApprovalOutcome,
    policy_src: &str,
    machine_token: Option<&str>,
    max_body_bytes: usize,
) -> Gate {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gate-apiproxy-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let upstream = MockServer::start().await;
    // The credential probe (GET /user) is mounted only when a machine token
    // exists — the no-token test asserts NOTHING reached upstream.
    if machine_token.is_some() {
        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"login":"gate-machine"}"#),
            )
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

    let token = machine_token.map(str::to_string);
    let token_getter: Arc<dyn Fn() -> Option<String> + Send + Sync> =
        Arc::new(move || token.clone());
    let base = reqwest::Url::parse(&upstream.uri()).unwrap();
    let platform: Arc<dyn Platform> = Arc::new(GitHubPlatform::with_bases(
        base.clone(),
        base,
        token_getter.clone(),
    ));
    let policy = PolicyEngine::new(&PolicyFile::parse(policy_src).unwrap());
    let backend = Arc::new(StubBackend::new(state.clone(), audit.clone(), auto).unwrap());
    let api = ApiDeps::new(
        state.clone(),
        platform,
        policy,
        backend,
        token_getter,
        APPROVAL_TIMEOUT,
    )
    .unwrap();

    let gs = GateState {
        state: state.clone(),
        audit,
        rate: Mutex::new(RateLimiter::default()),
        git: None,
        api: Some(Arc::new(api)),
        onepage: None,
        max_body_bytes,
        telegram_live: std::sync::atomic::AtomicBool::new(false),
        forwarding_enabled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let app = router(gs);
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
        agent_token,
        upstream,
        audit_path,
        dir,
        state: state.clone(),
    }
}

async fn spawn_gate(auto: ApprovalOutcome) -> Gate {
    spawn_gate_with(auto, DEFAULT_POLICY, Some(MACHINE_TOKEN)).await
}

async fn mount_membership(gate: &Gate, org: &str, body: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/user/memberships/orgs/{org}")))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&gate.upstream)
        .await;
}

async fn mount_repo_get(gate: &Gate, owner: &str, repo: &str, status: u16, body: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/repos/{owner}/{repo}")))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&gate.upstream)
        .await;
}

async fn mount_post(gate: &Gate, route: &str, status: u16, body: &str) {
    Mock::given(method("POST"))
        .and(path(route.to_string()))
        .respond_with(ResponseTemplate::new(status).set_body_raw(
            body.as_bytes().to_vec(),
            "application/json",
        ))
        .mount(&gate.upstream)
        .await;
}

async fn mount_get(gate: &Gate, route: &str, status: u16, body: &str) {
    Mock::given(method("GET"))
        .and(path(route.to_string()))
        .respond_with(ResponseTemplate::new(status).set_body_raw(
            body.as_bytes().to_vec(),
            "application/json",
        ))
        .mount(&gate.upstream)
        .await;
}

const MACHINE_BEARER: &str = "Bearer test-machine-token";

// ---------------------------------------------------------------------------
// GraphQL special case
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graphql_query_passthroughs_without_hold() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;
    mount_post(&gate, "/graphql", 200, r#"{"data":{"viewer":{"login":"gate-machine"}}}"#).await;

    let body = br#"{"query":"{ viewer { login } }"}"#;
    let resp = gate
        .send_with(
            "POST",
            "/api/v3/graphql",
            body,
            &[("content-type", "application/json")],
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let text = resp.text().await.unwrap();
    assert_eq!(text, r#"{"data":{"viewer":{"login":"gate-machine"}}}"#);

    // Upstream saw the query verbatim, exactly once.
    let requests = gate.upstream_requests().await;
    let hits: Vec<_> = requests.iter().filter(|r| r.url.path() == "/graphql").collect();
    assert_eq!(hits.len(), 1, "graphql POST forwarded once");
    assert_eq!(&hits[0].body, &body.to_vec());
    assert_eq!(
        hits[0]
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap()),
        Some(MACHINE_BEARER),
        "upstream sees the machine bearer token"
    );

    // A read holds nothing and is audited with method/path/status.
    assert_eq!(gate.pending_rows().len(), 0, "reads never hold");
    let audit = gate.api_audit();
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0]["decision"], "forward");
    let basis = audit[0]["basis"].as_str().unwrap();
    assert!(basis.contains("read"), "basis: {basis}");
    assert!(basis.contains("POST"), "basis: {basis}");
    assert!(basis.contains("/graphql"), "basis: {basis}");
    assert!(basis.contains("200"), "basis: {basis}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graphql_mutation_held_then_approved_forwards() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;
    mount_post(&gate, "/graphql", 200, r#"{"data":{"ok":true}}"#).await;

    let body = br#"{"query":"mutation { createIssue(input:{}) { issue { id } } }"}"#;
    let resp = gate
        .send_with(
            "POST",
            "/api/v3/graphql",
            body,
            &[("content-type", "application/json")],
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "approved mutation forwards");
    assert_eq!(resp.text().await.unwrap(), r#"{"data":{"ok":true}}"#);

    let requests = gate.upstream_requests().await;
    let hits: Vec<_> = requests.iter().filter(|r| r.url.path() == "/graphql").collect();
    assert_eq!(hits.len(), 1, "approved graphql mutation hits upstream");
    assert_eq!(&hits[0].body, &body.to_vec(), "byte-identical forward");

    let rows = gate.pending_rows();
    assert_eq!(rows.len(), 1, "the mutation was held");
    assert_eq!(rows[0].1, "approval.api");

    let audit = gate.api_audit();
    let decisions: Vec<&str> = audit.iter().map(|l| l["decision"].as_str().unwrap()).collect();
    assert_eq!(decisions, vec!["hold", "forward"], "{audit:?}");
    assert!(
        audit[0]["basis"].as_str().unwrap().contains("third-party"),
        "graphql is non-repo-scoped: fail closed to approval: {audit:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graphql_mutation_denied_is_403_policy_json() {
    let gate = spawn_gate(ApprovalOutcome::Denied).await;

    let body = br#"{"query":"mutation { deleteBranch }"}"#;
    let resp = gate.send("POST", "/api/v3/graphql", body).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let v = Gate::body_json(resp).await;
    assert_eq!(v["message"], "blocked by gate policy");

    let requests = gate.upstream_requests().await;
    assert!(
        requests.iter().all(|r| r.url.path() != "/graphql"),
        "denied mutation must NOT reach upstream"
    );

    let audit = gate.api_audit();
    let decisions: Vec<&str> = audit.iter().map(|l| l["decision"].as_str().unwrap()).collect();
    assert_eq!(decisions, vec!["hold", "deny"], "{audit:?}");
    assert_eq!(audit[1]["basis"], "approval: denied");
}

// ---------------------------------------------------------------------------
// REST mutations: hold + byte-identical forward
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_issue_held_then_forwarded_byte_identical() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;
    mount_membership(&gate, "octo", r#"{"state":"active"}"#).await;
    mount_post(&gate, "/repos/octo/hello/issues", 201, r#"{"number":1}"#).await;

    let body = br#"{"title":"from the agent","body":"made via the gate"}"#;
    let resp = gate
        .send_with(
            "POST",
            "/api/v3/repos/octo/hello/issues",
            body,
            &[
                ("content-type", "application/json"),
                ("x-test-header", "survives"),
            ],
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(resp.text().await.unwrap(), r#"{"number":1}"#);

    let requests = gate.upstream_requests().await;
    let hits: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path() == "/repos/octo/hello/issues")
        .collect();
    assert_eq!(hits.len(), 1, "held mutation forwarded after approval");
    assert_eq!(&hits[0].body, &body.to_vec(), "byte-identical body");
    assert_eq!(
        hits[0]
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap()),
        Some(MACHINE_BEARER),
        "machine bearer token upstream"
    );
    assert_eq!(
        hits[0].headers.get("x-test-header").map(|v| v.to_str().unwrap()),
        Some("survives"),
        "agent headers ride along (minus hop-by-hop)"
    );
    assert!(
        !hits[0]
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap().contains(&gate.agent_token))
            .unwrap_or(false),
        "the agent's gate token must never reach upstream"
    );

    let audit = gate.api_audit();
    let decisions: Vec<&str> = audit.iter().map(|l| l["decision"].as_str().unwrap()).collect();
    assert_eq!(decisions, vec!["hold", "forward"], "{audit:?}");
}

// ---------------------------------------------------------------------------
// Reads: passthrough + audit
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_passthrough_is_audited_with_method_path_status() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;
    mount_get(&gate, "/repos/octo/hello/issues", 200, r#"[{"number":1}]"#).await;

    let resp = gate
        .send_with(
            "GET",
            "/api/v3/repos/octo/hello/issues?state=open&per_page=5",
            b"",
            &[("accept", "application/vnd.github+json")],
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").map(|v| v.to_str().unwrap()),
        Some("application/json"),
        "content type passes back"
    );
    assert_eq!(resp.text().await.unwrap(), r#"[{"number":1}]"#);

    let requests = gate.upstream_requests().await;
    let hits: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path() == "/repos/octo/hello/issues")
        .collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].url.query(),
        Some("state=open&per_page=5"),
        "query string preserved verbatim"
    );
    assert_eq!(
        hits[0].headers.get("accept").map(|v| v.to_str().unwrap()),
        Some("application/vnd.github+json"),
        "request headers preserved"
    );
    assert_eq!(
        hits[0]
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap()),
        Some(MACHINE_BEARER)
    );

    assert_eq!(gate.pending_rows().len(), 0, "reads never hold");
    let audit = gate.api_audit();
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0]["decision"], "forward");
    let basis = audit[0]["basis"].as_str().unwrap();
    assert!(basis.contains("GET"), "basis: {basis}");
    assert!(basis.contains("/repos/octo/hello/issues"), "basis: {basis}");
    assert!(basis.contains("200"), "basis: {basis}");
}

// ---------------------------------------------------------------------------
// Credential gate (Review Focus 5)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_machine_token_is_credential_unavailable_and_nothing_forwarded() {
    // No machine token, and NO upstream mocks at all: any forwarded byte
    // would show up as a received request.
    let gate = spawn_gate_with(
        ApprovalOutcome::Approved,
        DEFAULT_POLICY,
        None,
    )
    .await;

    let resp = gate
        .send("POST", "/api/v3/repos/octo/hello/issues", br#"{"title":"x"}"#)
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let v = Gate::body_json(resp).await;
    assert_eq!(v["error"], "credential_unavailable");
    assert!(v["message"].is_string(), "shape: {v}");

    let requests = gate.upstream_requests().await;
    assert!(
        requests.is_empty(),
        "nothing may be forwarded without a verified machine token: {requests:?}"
    );
    assert!(
        !gate.api_audit().is_empty(),
        "the credential denial is audited"
    );
}

// ---------------------------------------------------------------------------
// Upstream error mapping (GATE-010)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_404_maps_to_access_pending() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;
    mount_get(
        &gate,
        "/repos/octo/ghost/issues",
        404,
        r#"{"message":"Not Found"}"#,
    )
    .await;

    let resp = gate
        .send("GET", "/api/v3/repos/octo/ghost/issues", b"")
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let v = Gate::body_json(resp).await;
    assert_eq!(v["error"], "access_pending");
    assert_eq!(v["repo"], "octo/ghost");
    assert!(v["message"].is_string(), "shape: {v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_403_admin_rights_maps_to_permission_pending_with_flow_row() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;
    mount_membership(&gate, "octo", r#"{"state":"active"}"#).await;
    mount_post(
        &gate,
        "/repos/octo/priv/hooks",
        403,
        r#"{"message":"Must have admin rights to Repository."}"#,
    )
    .await;

    let resp = gate
        .send("POST", "/api/v3/repos/octo/priv/hooks", br#"{"name":"web"}"#)
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let v = Gate::body_json(resp).await;
    assert_eq!(v["error"], "permission_pending", "{v}");
    let elevation_id = v["elevation_id"].as_str().expect("elevation_id").to_string();
    assert!(!elevation_id.is_empty());
    assert_eq!(v["needs_role"], "admin");
    assert!(v["message"].is_string());

    // A real flows row backs the id, so the endpoint and Plan B have
    // something to read.
    let flows = gate.flows();
    assert_eq!(flows.len(), 1, "{flows:?}");
    assert_eq!(flows[0].0, elevation_id);
    assert_eq!(flows[0].1, "elevation");
    assert_eq!(flows[0].2, "DETECTED");

    // The elevation endpoint serves the row back.
    let resp = gate
        .send("GET", &format!("/proxy/elevations/{elevation_id}"), b"")
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = Gate::body_json(resp).await;
    assert_eq!(v["id"], elevation_id);
    assert_eq!(v["state"], "DETECTED");

    let audit = gate.api_audit();
    let last = audit.last().unwrap();
    assert_eq!(last["decision"], "deny");
    assert!(
        last["basis"].as_str().unwrap().contains("permission_pending"),
        "{audit:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_403_missing_scope_maps_to_role_unavailable() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;
    mount_membership(&gate, "octo", r#"{"state":"active"}"#).await;
    Mock::given(method("POST"))
        .and(path("/repos/octo/hello/hooks"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_string(r#"{"message":"Forbidden"}"#)
                .append_header("X-Accepted-OAuth-Scopes", "repo, admin:org")
                .append_header("X-OAuth-Scopes", "public_repo"),
        )
        .mount(&gate.upstream)
        .await;

    let resp = gate
        .send("POST", "/api/v3/repos/octo/hello/hooks", br#"{}"#)
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let v = Gate::body_json(resp).await;
    assert_eq!(v["error"], "role_unavailable", "{v}");
    let message = v["message"].as_str().unwrap();
    assert!(
        message.contains("'repo'"),
        "must name the missing scope: {message}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_401_maps_to_credential_unavailable() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;
    mount_get(
        &gate,
        "/repos/octo/ghost/issues",
        401,
        r#"{"message":"Bad credentials"}"#,
    )
    .await;

    let resp = gate
        .send("GET", "/api/v3/repos/octo/ghost/issues", b"")
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let v = Gate::body_json(resp).await;
    assert_eq!(v["error"], "credential_unavailable", "{v}");
    assert!(v["message"].is_string());
}

// ---------------------------------------------------------------------------
// Approval payload hygiene
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_body_snippet_is_capped_and_sha_covers_full_body() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;
    mount_membership(&gate, "octo", r#"{"state":"active"}"#).await;
    mount_post(&gate, "/repos/octo/big/issues", 201, r#"{"number":7}"#).await;

    let body = vec![b'a'; 5120]; // 5 KiB
    let resp = gate
        .send("POST", "/api/v3/repos/octo/big/issues", &body)
        .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Upstream still received the FULL body.
    let requests = gate.upstream_requests().await;
    let hits: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path() == "/repos/octo/big/issues")
        .collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(&hits[0].body, &body, "full body forwarded");

    // The approval payload carries a capped snippet plus the full sha.
    let rows = gate.pending_rows();
    assert_eq!(rows.len(), 1);
    let payload: serde_json::Value = serde_json::from_str(&rows[0].2).unwrap();
    let snippet = payload["detail"]["body_snippet"].as_str().unwrap();
    assert!(
        snippet.len() <= 4096,
        "snippet capped at 4 KiB: {} bytes",
        snippet.len()
    );
    assert!(snippet.contains("truncated"), "truncation noted: {snippet}");
    let sha = payload["detail"]["body_sha"].as_str().unwrap();
    let mut hasher = Sha256::new();
    hasher.update(&body);
    let expected: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(sha, expected, "body_sha is the sha-256 of the FULL body");
    assert_eq!(sha.len(), 64);
}

// ---------------------------------------------------------------------------
// Elevation endpoint
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elevation_endpoint_known_and_unknown() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;
    gate.state
        .lock()
        .unwrap()
        .flow_insert("flow-123", "elevation", "DETECTED", r#"{"repo":"octo/x"}"#)
        .unwrap();

    let resp = gate.send("GET", "/proxy/elevations/flow-123", b"").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = Gate::body_json(resp).await;
    assert_eq!(v["id"], "flow-123");
    assert_eq!(v["state"], "DETECTED");

    let resp = gate.send("GET", "/proxy/elevations/absent", b"").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let v = Gate::body_json(resp).await;
    assert!(v["message"].is_string(), "github-shaped 404: {v}");
}

// ---------------------------------------------------------------------------
// Pre-flight role gap (P0 heuristic)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_registry_role_below_write_holds_before_upstream() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;
    gate.state
        .lock()
        .unwrap()
        .registry_upsert("octo/reg", "read", "read", "device-flow")
        .unwrap();

    let resp = gate
        .send("POST", "/api/v3/repos/octo/reg/issues", br#"{"title":"x"}"#)
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let v = Gate::body_json(resp).await;
    assert_eq!(v["error"], "permission_pending", "{v}");
    assert_eq!(v["needs_role"], "write");
    let elevation_id = v["elevation_id"].as_str().unwrap().to_string();

    // The placeholder flow row exists.
    let flows = gate.flows();
    assert_eq!(flows.len(), 1);
    assert_eq!(flows[0].0, elevation_id);
    assert_eq!(flows[0].2, "DETECTED");

    // Upstream saw only the credential probe — the mutation never left.
    let requests = gate.upstream_requests().await;
    assert!(
        requests.iter().all(|r| r.url.path() == "/user"),
        "pre-flight must stop the request before forwarding: {requests:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_live_noaccess_maps_to_access_pending() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;
    gate.state
        .lock()
        .unwrap()
        .registry_upsert("octo/reg", "write", "write", "device-flow")
        .unwrap();
    mount_repo_get(&gate, "octo", "reg", 404, r#"{"message":"Not Found"}"#).await;

    let resp = gate
        .send("POST", "/api/v3/repos/octo/reg/issues", br#"{"title":"x"}"#)
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let v = Gate::body_json(resp).await;
    assert_eq!(v["error"], "access_pending", "{v}");
    assert_eq!(v["repo"], "octo/reg");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_registered_role_ok_forwards_after_approval() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;
    gate.state
        .lock()
        .unwrap()
        .registry_upsert("octo/reg", "admin", "admin", "device-flow")
        .unwrap();
    mount_repo_get(
        &gate,
        "octo",
        "reg",
        200,
        r#"{"private":false,"permissions":{"admin":true,"pull":true}}"#,
    )
    .await;
    mount_post(&gate, "/repos/octo/reg/issues", 201, r#"{"number":3}"#).await;

    let resp = gate
        .send("POST", "/api/v3/repos/octo/reg/issues", br#"{"title":"x"}"#)
        .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "role gap cleared");

    let audit = gate.api_audit();
    let decisions: Vec<&str> = audit.iter().map(|l| l["decision"].as_str().unwrap()).collect();
    assert_eq!(decisions, vec!["hold", "forward"], "{audit:?}");
}

// ---------------------------------------------------------------------------
// [api] policy section
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_policy_deny_blocks_mutation() {
    let policy = "\
[defaults]
push = \"allow\"
[api]
mutations = \"deny\"
";
    let gate = spawn_gate_with(ApprovalOutcome::Approved, policy, Some(MACHINE_TOKEN)).await;
    mount_membership(&gate, "octo", r#"{"state":"active"}"#).await;

    let resp = gate
        .send("POST", "/api/v3/repos/octo/hello/issues", br#"{"title":"x"}"#)
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let v = Gate::body_json(resp).await;
    assert_eq!(v["message"], "blocked by gate policy");

    let requests = gate.upstream_requests().await;
    assert!(
        requests.iter().all(|r| r.url.path() != "/repos/octo/hello/issues"),
        "denied mutation never forwards (membership probe aside)"
    );
    let audit = gate.api_audit();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["decision"], "deny");
    assert!(
        audit[0]["basis"].as_str().unwrap().contains("policy: deny"),
        "{audit:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_policy_allow_forwards_without_hold() {
    let policy = "\
[defaults]
push = \"allow\"
[api]
mutations = \"allow\"
";
    let gate = spawn_gate_with(ApprovalOutcome::Denied, policy, Some(MACHINE_TOKEN)).await;
    mount_membership(&gate, "octo", r#"{"state":"active"}"#).await;
    mount_post(&gate, "/repos/octo/hello/issues", 201, r#"{"number":9}"#).await;

    let resp = gate
        .send("POST", "/api/v3/repos/octo/hello/issues", br#"{"title":"x"}"#)
        .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    assert_eq!(gate.pending_rows().len(), 0, "allow never holds");
    let audit = gate.api_audit();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["decision"], "forward");
    assert!(
        audit[0]["basis"].as_str().unwrap().contains("policy: allow"),
        "{audit:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn third_party_mutation_holds_even_when_policy_allows() {
    let policy = "\
[defaults]
push = \"allow\"
[api]
mutations = \"allow\"
";
    let gate = spawn_gate_with(ApprovalOutcome::Approved, policy, Some(MACHINE_TOKEN)).await;
    // Not a member ⇒ not own sphere ⇒ GATE-005: approval regardless.
    mount_membership(&gate, "octo", r#"{"state":"pending"}"#).await;
    mount_post(&gate, "/repos/octo/other/issues", 201, r#"{"number":2}"#).await;

    let resp = gate
        .send("POST", "/api/v3/repos/octo/other/issues", br#"{"title":"x"}"#)
        .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "approved third-party");

    let rows = gate.pending_rows();
    assert_eq!(rows.len(), 1, "third-party mutations hold");
    let audit = gate.api_audit();
    assert!(
        audit[0]["basis"].as_str().unwrap().contains("third-party"),
        "{audit:?}"
    );
}

// ---------------------------------------------------------------------------
// Response-header allowlist (final-review fix 3)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn response_header_allowlist_survives_to_agents() {
    let gate = spawn_gate(ApprovalOutcome::Approved).await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/hello/issues"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(r#"[{"number":1}]"#.as_bytes().to_vec(), "application/json")
                .append_header(
                    "Link",
                    r#"<https://api.github.com/repos/octo/hello/issues?page=2>; rel="next""#,
                )
                .append_header("Location", "https://api.github.com/elsewhere")
                .append_header("ETag", r#"W/"abc123""#)
                .append_header("Retry-After", "30")
                .append_header("X-RateLimit-Limit", "5000")
                .append_header("X-RateLimit-Remaining", "4998")
                .append_header("X-RateLimit-Reset", "1700000000")
                .append_header("X-RateLimit-Used", "2")
                .append_header("X-Internal-Only", "drop-me"),
        )
        .mount(&gate.upstream)
        .await;

    let resp = gate
        .send("GET", "/api/v3/repos/octo/hello/issues", b"")
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let h = resp.headers();

    // The allowlisted set survives: pagination, redirects, caching,
    // backoff and the rate-limit budget agents (`gh api --paginate` in
    // particular) depend on.
    assert!(
        h.get("link")
            .map(|v| v.to_str().unwrap())
            .unwrap()
            .contains("rel=\"next\""),
        "pagination Link survives: {:?}",
        h.get("link")
    );
    assert_eq!(
        h.get("x-ratelimit-remaining").map(|v| v.to_str().unwrap()),
        Some("4998"),
        "rate-limit budget survives"
    );
    assert_eq!(
        h.get("x-ratelimit-limit").map(|v| v.to_str().unwrap()),
        Some("5000")
    );
    assert_eq!(
        h.get("x-ratelimit-reset").map(|v| v.to_str().unwrap()),
        Some("1700000000")
    );
    assert_eq!(
        h.get("x-ratelimit-used").map(|v| v.to_str().unwrap()),
        Some("2")
    );
    assert_eq!(
        h.get("location").map(|v| v.to_str().unwrap()),
        Some("https://api.github.com/elsewhere")
    );
    assert_eq!(h.get("etag").map(|v| v.to_str().unwrap()), Some(r#"W/"abc123""#));
    assert_eq!(h.get("retry-after").map(|v| v.to_str().unwrap()), Some("30"));
    assert_eq!(
        h.get("content-type").map(|v| v.to_str().unwrap()),
        Some("application/json"),
        "content-type still passes back"
    );
    // Non-allowlisted headers stay dropped.
    assert!(
        h.get("x-internal-only").is_none(),
        "headers outside the allowlist must not leak back"
    );
}

// ---------------------------------------------------------------------------
// Body cap (final-review fix 4)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn over_limit_body_is_413_github_shaped_without_upstream() {
    let gate = spawn_gate_full(
        ApprovalOutcome::Approved,
        DEFAULT_POLICY,
        Some(MACHINE_TOKEN),
        1024,
    )
    .await;
    mount_membership(&gate, "octo", r#"{"state":"active"}"#).await;
    mount_post(&gate, "/repos/octo/hello/issues", 201, r#"{"number":1}"#).await;

    let body = vec![b'a'; 2048]; // over the 1 KiB test cap
    let resp = gate
        .send("POST", "/api/v3/repos/octo/hello/issues", &body)
        .await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let v = Gate::body_json(resp).await;
    assert_eq!(v["message"], "Payload too large");

    // Nothing reached upstream — the cap fires before any upstream work
    // (not even the credential probe).
    let requests = gate.upstream_requests().await;
    assert!(
        requests.is_empty(),
        "over-limit body must not be forwarded: {requests:?}"
    );

    let audit = gate.api_audit();
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0]["decision"], "deny");
    assert_eq!(audit[0]["basis"], "payload too large");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn under_limit_body_still_forwards() {
    let gate = spawn_gate_full(
        ApprovalOutcome::Approved,
        DEFAULT_POLICY,
        Some(MACHINE_TOKEN),
        64 * 1024,
    )
    .await;
    mount_membership(&gate, "octo", r#"{"state":"active"}"#).await;
    mount_post(&gate, "/repos/octo/hello/issues", 201, r#"{"number":5}"#).await;

    let body = vec![b'a'; 2048]; // well under the 64 KiB test cap
    let resp = gate
        .send("POST", "/api/v3/repos/octo/hello/issues", &body)
        .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "under-limit body flows");
    let requests = gate.upstream_requests().await;
    let hits: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path() == "/repos/octo/hello/issues")
        .collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(&hits[0].body, &body, "full body forwarded");
}
