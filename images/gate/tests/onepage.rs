//! Integration: the instruction page + one-time machine-token form
//! (task 9, GATE-002/004). `GET /` and `/setup/token` are mounted OUTSIDE
//! the agent auth middleware — an agent has no gate token at intake time;
//! the form is gated by a single-use setup nonce (kv, 15-min TTL) instead.
//! The submitted PAT is verified against GitHub (login, 2FA, scopes,
//! expiry header) before encrypted custody; verification failure stores
//! nothing and keeps the nonce alive for retry.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use reqwest::StatusCode;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use gate::audit::Audit;
use gate::custody::MachineTokenStore;
use gate::github::{GitHubPlatform, Platform};
use gate::onepage::{mint_setup_nonce, OnePageDeps, KV_NONCE};
use gate::routes::{router, GateState, RateLimiter};
use gate::state::State as DbState;

const HOST: &str = "https://gag.example.com";
const MACHINE_LOGIN: &str = "gate-machine";
const GOOD_TOKEN: &str = "ghp_goodEXAMPLEtoken0000000000";
const ENC_KEY: &str = "integration-test-enc-key";

struct Gate {
    port: u16,
    dir: PathBuf,
    state: Arc<Mutex<DbState>>,
    upstream: MockServer,
}

impl Drop for Gate {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Gate {
    fn url(&self, path_and_query: &str) -> String {
        format!("http://127.0.0.1:{}{path_and_query}", self.port)
    }

    /// Requests carry NO credentials — these routes must work without the
    /// agent auth middleware.
    async fn get(&self, path_and_query: &str) -> reqwest::Response {
        reqwest::Client::new()
            .get(self.url(path_and_query))
            .send()
            .await
            .unwrap()
    }

    async fn post_token(&self, path_and_query: &str, token: &str) -> reqwest::Response {
        reqwest::Client::new()
            .post(self.url(path_and_query))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("token={token}"))
            .send()
            .await
            .unwrap()
    }

    async fn body(resp: reqwest::Response) -> String {
        let bytes = resp.bytes().await.unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn custody_load(&self) -> anyhow::Result<Option<String>> {
        let guard = self.state.lock().unwrap();
        MachineTokenStore::new(&guard, ENC_KEY).load()
    }

    fn custody_has_token(&self) -> bool {
        let guard = self.state.lock().unwrap();
        MachineTokenStore::new(&guard, ENC_KEY).has_token()
    }

    fn mint_nonce(&self) -> String {
        mint_setup_nonce(&self.state.lock().unwrap()).unwrap()
    }

    fn kv(&self, key: &str) -> Option<String> {
        self.state.lock().unwrap().kv_get(key).unwrap()
    }
}

async fn spawn() -> Gate {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gate-onepage-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let upstream = MockServer::start().await;
    let db_path = dir.join("gate.db");
    let state = Arc::new(Mutex::new(DbState::open(db_path.to_str().unwrap()).unwrap()));
    let audit_path = dir.join("audit.jsonl");

    let base = reqwest::Url::parse(&upstream.uri()).unwrap();
    let platform: Arc<dyn Platform> = Arc::new(GitHubPlatform::with_bases(
        base.clone(),
        base,
        Arc::new(|| None),
    ));

    let gs = GateState {
        state: state.clone(),
        audit: Arc::new(Audit::open(&audit_path)),
        rate: Mutex::new(RateLimiter::default()),
        git: None,
        api: None,
        onepage: Some(Arc::new(OnePageDeps {
            gate_host: HOST.to_string(),
            machine_login: MACHINE_LOGIN.to_string(),
            platform,
            enc_key: ENC_KEY.to_string(),
            notify: None,
        })),
        max_body_bytes: gate::config::DEFAULT_MAX_BODY_BYTES,
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
        dir,
        state,
        upstream,
    }
}

/// Mock `GET /user` on the wiremock upstream. When `require_bearer` is
/// given, only a request carrying exactly that Authorization header
/// matches (a wrong token would fall through to wiremock's default 404).
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

async fn mount_good_user(server: &MockServer) {
    mount_user(
        server,
        MACHINE_LOGIN,
        true,
        "repo, workflow, read:org",
        Some("2027-05-01T00:00:00Z"),
        Some(GOOD_TOKEN),
    )
    .await;
}

// ---------------------------------------------------------------------------
// Instruction page (GET /)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn instruction_page_needs_no_auth_and_carries_all_four_blocks() {
    let gate = spawn().await;
    let resp = gate.get("/").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ctype.starts_with("text/html"),
        "instruction page is text/html, got {ctype}"
    );
    let page = Gate::body(resp).await;

    // Block 1 — public repo: clone from github, add gag remote, pushDefault.
    assert!(
        page.contains("git clone https://github.com/{owner}/{repo}.git"),
        "public clone path missing: {page}"
    );
    assert!(
        page.contains(&format!(
            "git remote add gag {HOST}/git/{{owner}}/{{repo}}.git"
        )),
        "gag remote missing: {page}"
    );
    assert!(
        page.contains("git config remote.pushDefault gag"),
        "pushDefault missing"
    );

    // Block 2 — private repo: clone THROUGH the gate, rename, converge.
    assert!(
        page.contains(
            "git clone https://agent:{agent_token}@gag.example.com/git/{owner}/{repo}.git"
        ),
        "private clone through the gate missing"
    );
    assert!(page.contains("git remote rename origin gag"));
    assert!(
        page.contains("git remote add origin https://github.com/{owner}/{repo}.git"),
        "canonical origin re-add missing"
    );

    // Block 3 — commit identity (id placeholder: gate fills it post-setup).
    assert!(
        page.contains(&format!("git config user.name \"{MACHINE_LOGIN}\"")),
        "commit identity name missing"
    );
    assert!(
        page.contains(&format!(
            "{{id}}+{MACHINE_LOGIN}@users.noreply.github.com"
        )),
        "noreply commit email missing"
    );

    // Block 4 — API env.
    assert!(page.contains("export GH_HOST=gag.example.com"));
    assert!(page.contains("export GITHUB_ENTERPRISE_TOKEN={agent_token}"));
}

#[tokio::test]
async fn instruction_page_embeds_agent_token_when_query_param_given() {
    let gate = spawn().await;
    let page = Gate::body(gate.get("/?token=abc123").await).await;
    assert!(
        page.contains("https://agent:abc123@gag.example.com/git/{owner}/{repo}.git"),
        "agent token must be embedded in the gag URLs: {page}"
    );
    assert!(page.contains("export GITHUB_ENTERPRISE_TOKEN=abc123"));
    assert!(
        !page.contains("{agent_token}"),
        "no placeholder may remain when a token was supplied"
    );
}

#[tokio::test]
async fn whoami_still_requires_agent_auth() {
    let gate = spawn().await;
    let resp = gate.get("/whoami").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// One-time token form (GET / POST /setup/token)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn form_get_with_valid_nonce_is_200_password_page() {
    let gate = spawn().await;
    let nonce = gate.mint_nonce();
    let resp = gate.get(&format!("/setup/token?nonce={nonce}")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(ctype.starts_with("text/html"), "got {ctype}");
    let page = Gate::body(resp).await;
    assert!(page.contains("type=\"password\""), "single password field");
    assert!(page.contains("name=\"token\""));
    assert!(
        page.contains(&format!("action=\"/setup/token?nonce={nonce}\"")),
        "form must post back to the same path: {page}"
    );
}

#[tokio::test]
async fn form_get_with_unknown_nonce_is_410() {
    let gate = spawn().await;
    let resp = gate.get("/setup/token?nonce=deadbeef").await;
    assert_eq!(resp.status(), StatusCode::GONE);
}

#[tokio::test]
async fn form_get_without_nonce_param_is_410() {
    let gate = spawn().await;
    let resp = gate.get("/setup/token").await;
    assert_eq!(resp.status(), StatusCode::GONE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn form_get_with_expired_nonce_is_410() {
    let gate = spawn().await;
    let nonce = gate.mint_nonce();
    // Force the expiry into the past (TTL itself is covered by unit tests).
    gate.state
        .lock()
        .unwrap()
        .kv_set("setup_nonce_expires", &(-1).to_string())
        .unwrap();
    let resp = gate.get(&format!("/setup/token?nonce={nonce}")).await;
    assert_eq!(resp.status(), StatusCode::GONE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verified_token_is_stored_encrypted_and_nonce_consumed() {
    let gate = spawn().await;
    mount_good_user(&gate.upstream).await;
    let nonce = gate.mint_nonce();

    let resp = gate
        .post_token(&format!("/setup/token?nonce={nonce}"), GOOD_TOKEN)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let page = Gate::body(resp).await;
    assert!(page.contains(MACHINE_LOGIN), "success page names login");
    assert!(
        page.contains("2027-05-01T00:00:00Z"),
        "success page shows captured expiry: {page}"
    );
    assert!(
        !page.contains(GOOD_TOKEN),
        "the token must never be echoed back"
    );

    // Custody: encrypted at rest, decryptable with the right key.
    assert!(gate.custody_has_token());
    assert_eq!(
        gate.custody_load().unwrap().as_deref(),
        Some(GOOD_TOKEN)
    );
    let b64 = gate.kv("machine_token_ct").expect("ciphertext stored");
    assert!(!b64.contains(GOOD_TOKEN), "stored value is ciphertext");

    // Single use: nonce consumed.
    assert!(gate.kv(KV_NONCE).is_none(), "nonce deleted after success");
    let resp = gate.get(&format!("/setup/token?nonce={nonce}")).await;
    assert_eq!(resp.status(), StatusCode::GONE, "reused nonce is 410");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_login_failure_names_the_found_login_and_keeps_nonce() {
    let gate = spawn().await;
    // Machine login expected, but the token belongs to the main account.
    mount_user(
        &gate.upstream,
        "robsmith",
        true,
        "repo, workflow, read:org",
        None,
        None,
    )
    .await;
    let nonce = gate.mint_nonce();

    let resp = gate
        .post_token(&format!("/setup/token?nonce={nonce}"), "ghp_main_account")
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let page = Gate::body(resp).await;
    assert!(
        page.contains("robsmith"),
        "wrong-login page must name the login found: {page}"
    );
    assert!(
        page.contains("name=\"token\""),
        "retry form offered after failure"
    );

    // Nothing stored, nonce alive for retry.
    assert!(gate.custody_load().unwrap().is_none(), "never stored");
    assert!(gate.kv(KV_NONCE).is_some(), "nonce NOT consumed");
    let resp = gate.get(&format!("/setup/token?nonce={nonce}")).await;
    assert_eq!(resp.status(), StatusCode::OK, "retry still possible");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_scope_failure_names_the_scope_and_stores_nothing() {
    let gate = spawn().await;
    mount_user(
        &gate.upstream,
        MACHINE_LOGIN,
        true,
        "repo, read:org",
        None,
        None,
    )
    .await;
    let nonce = gate.mint_nonce();

    let resp = gate
        .post_token(&format!("/setup/token?nonce={nonce}"), GOOD_TOKEN)
        .await;
    let page = Gate::body(resp).await;
    assert!(
        page.contains("workflow"),
        "first missing scope must be named: {page}"
    );
    assert!(gate.custody_load().unwrap().is_none());
    assert!(gate.kv(KV_NONCE).is_some(), "nonce NOT consumed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_factor_off_failure_names_the_check() {
    let gate = spawn().await;
    mount_user(
        &gate.upstream,
        MACHINE_LOGIN,
        false,
        "repo, workflow, read:org",
        None,
        None,
    )
    .await;
    let nonce = gate.mint_nonce();

    let resp = gate
        .post_token(&format!("/setup/token?nonce={nonce}"), GOOD_TOKEN)
        .await;
    let page = Gate::body(resp).await;
    assert!(
        page.to_lowercase().contains("two-factor"),
        "2FA check failure must be named: {page}"
    );
    assert!(gate.custody_load().unwrap().is_none());
    assert!(gate.kv(KV_NONCE).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_expiry_token_is_accepted_with_warning() {
    let gate = spawn().await;
    mount_user(
        &gate.upstream,
        MACHINE_LOGIN,
        true,
        "repo, workflow, read:org",
        None,
        Some(GOOD_TOKEN),
    )
    .await;
    let nonce = gate.mint_nonce();

    let resp = gate
        .post_token(&format!("/setup/token?nonce={nonce}"), GOOD_TOKEN)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let page = Gate::body(resp).await;
    assert!(
        page.to_lowercase().contains("no expiry") || page.to_lowercase().contains("never expire"),
        "missing expiry header must warn: {page}"
    );
    assert_eq!(gate.custody_load().unwrap().as_deref(), Some(GOOD_TOKEN));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_with_gone_nonce_is_410_and_never_contacts_upstream() {
    let gate = spawn().await;
    let resp = gate
        .post_token("/setup/token?nonce=unknown", GOOD_TOKEN)
        .await;
    assert_eq!(resp.status(), StatusCode::GONE);
    let requests = gate.upstream.received_requests().await.unwrap();
    assert!(
        requests.is_empty(),
        "no GitHub call may happen for a dead nonce"
    );
}
