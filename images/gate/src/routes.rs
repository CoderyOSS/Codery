//! Auth middleware (GATE-003): protects every gate route. Agents present
//! `Authorization: Basic <base64 user:token>` (git's native shape; the user
//! part is ignored for auth) or `Authorization: Bearer <token>`. A valid
//! token injects `AgentIdentity` as a request extension; failures return
//! GitHub-shaped 401s, and five or more failures per source inside a minute
//! blocks that source for 60 seconds.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use base64::Engine as _;
use serde_json::json;

use crate::apiproxy::ApiDeps;
use crate::audit::{Audit, AuditEntry};
use crate::config::DEFAULT_MAX_BODY_BYTES;
use crate::gitproxy::GitDeps;
use crate::state::State as DbState;
use crate::tokens::{AgentIdentity, TokenStore};

/// Rolling-window failure count that trips the per-source block.
const FAILURE_LIMIT: usize = 5;
/// Window over which failures are counted.
const WINDOW: Duration = Duration::from_secs(60);
/// How long a tripped source stays blocked.
const BLOCK: Duration = Duration::from_secs(60);

/// Everything the middleware needs: shared state, audit sink, rate limit,
/// the git-proxy dependencies and the API-proxy dependencies (absent only
/// on probe-only routers).
pub struct GateState {
    pub state: Arc<Mutex<DbState>>,
    pub audit: Arc<Audit>,
    pub rate: Mutex<RateLimiter>,
    pub git: Option<Arc<GitDeps>>,
    pub api: Option<Arc<ApiDeps>>,
    /// Onboarding-page dependencies (`GET /` + the one-time token form);
    /// absent only on probe-only routers.
    pub onepage: Option<Arc<crate::onepage::OnePageDeps>>,
    /// Cap on proxied request bodies; over-limit requests get a 413
    /// before anything is forwarded upstream.
    pub max_body_bytes: usize,
}

impl GateState {
    /// Probe-only router state (no proxy deps).
    pub fn new(state: DbState, audit: Audit) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
            audit: Arc::new(audit),
            rate: Mutex::new(RateLimiter::default()),
            git: None,
            api: None,
            onepage: None,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }

    /// Full router state; `state` and `audit` are shared with the approval
    /// backend / sphere resolver by the caller.
    pub fn with_git(state: Arc<Mutex<DbState>>, audit: Arc<Audit>, git: GitDeps) -> Self {
        Self {
            state,
            audit,
            rate: Mutex::new(RateLimiter::default()),
            git: Some(Arc::new(git)),
            api: None,
            onepage: None,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }

    /// Audit a failed authentication and count it against `peer`. The
    /// basis text must be static — credential material never reaches the
    /// audit log.
    fn deny_and_record(&self, peer: &str, basis: &str) {
        self.audit.record(AuditEntry {
            ts: crate::state::now(),
            agent: None,
            repo: "-".to_string(),
            action: "token.validate".to_string(),
            decision: "deny".to_string(),
            basis: basis.to_string(),
            approver: None,
        });
        self.rate
            .lock()
            .expect("rate limiter lock must be poison-free")
            .record_failure(peer);
    }
}

/// In-memory per-source failure tracking (per-instance; no persistence).
#[derive(Default)]
pub struct RateLimiter {
    peers: HashMap<String, PeerState>,
}

#[derive(Default)]
struct PeerState {
    failures: Vec<Instant>,
    blocked_until: Option<Instant>,
}

impl RateLimiter {
    fn is_blocked(&mut self, peer: &str) -> bool {
        let now = Instant::now();
        let ps = self.peer(peer, now);
        ps.blocked_until.is_some_and(|until| now < until)
    }

    fn record_failure(&mut self, peer: &str) {
        let now = Instant::now();
        let ps = self.peer(peer, now);
        ps.failures.push(now);
        if ps.failures.len() >= FAILURE_LIMIT {
            ps.blocked_until = Some(now + BLOCK);
        }
    }

    /// Entry for `peer` with failures outside WINDOW pruned.
    fn peer(&mut self, peer: &str, now: Instant) -> &mut PeerState {
        let ps = self.peers.entry(peer.to_string()).or_default();
        ps.failures.retain(|t| now.duration_since(*t) < WINDOW);
        ps
    }
}

/// The full gate surface: probe route, the git smart-HTTP proxy and the
/// API proxy, all behind [`require_agent`] — plus, when onboarding deps
/// are wired, the instruction page and the one-time token form mounted
/// OUTSIDE the middleware (the agent has no gate token at intake time;
/// the form is gated by the single-use setup nonce instead).
pub fn router(state: GateState) -> Router {
    let state = Arc::new(state);
    let guarded = Router::new()
        .route("/whoami", get(whoami))
        .merge(crate::gitproxy::router(state.clone()))
        .merge(crate::apiproxy::router(state.clone()))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_agent,
        ));
    let app = if state.onepage.is_some() {
        guarded.merge(crate::onepage::router(state.clone()))
    } else {
        guarded
    };
    app.with_state(state)
}

async fn whoami(Extension(agent): Extension<AgentIdentity>) -> Json<serde_json::Value> {
    Json(json!({ "agent": agent.name }))
}

/// Pull the raw token out of an Authorization header (Basic or Bearer).
/// The Basic user part is accepted per git's native shape but ignored for
/// auth: the token alone determines the identity.
fn extract_token(req: &Request) -> Option<String> {
    let value = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = value.split_once(' ')?;
    let rest = rest.trim();
    if scheme.eq_ignore_ascii_case("bearer") {
        Some(rest.to_string())
    } else if scheme.eq_ignore_ascii_case("basic") {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(rest)
            .ok()?;
        let creds = String::from_utf8(decoded).ok()?;
        let (_user, token) = creds.split_once(':')?;
        Some(token.to_string())
    } else {
        None
    }
}

/// The gate every route lives behind. In the real server the service must
/// be built with `into_make_service_with_connect_info::<SocketAddr>()` so
/// the rate limiter sees per-client sources; where ConnectInfo is absent
/// (e.g. `oneshot` tests) all requests share one bucket.
pub async fn require_agent(
    State(gs): State<Arc<GateState>>,
    req: Request,
    next: Next,
) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
        .unwrap_or_else(|| "unidentified".to_string());

    // A blocked source gets 429 without further evaluation — fail closed.
    if gs
        .rate
        .lock()
        .expect("rate limiter lock must be poison-free")
        .is_blocked(&peer)
    {
        return too_many_requests();
    }

    let Some(raw) = extract_token(&req) else {
        gs.deny_and_record(&peer, "no usable credentials");
        return bad_credentials();
    };

    let identity = {
        let state = gs
            .state
            .lock()
            .expect("state lock must be poison-free");
        TokenStore::new(&state, &gs.audit).validate(&raw)
    };

    match identity {
        Some(agent) => {
            let mut req = req;
            req.extensions_mut().insert(agent);
            next.run(req).await
        }
        None => {
            gs.deny_and_record(&peer, "invalid credentials");
            bad_credentials()
        }
    }
}

/// GitHub-shaped 401 so git clients prompt/reject the way they do against
/// github.com.
fn bad_credentials() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"gate\""),
        )],
        Json(json!({ "message": "Bad credentials" })),
    )
        .into_response()
}

/// GitHub-shaped 429 for sources tripping the failure limit.
fn too_many_requests() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({ "message": "Too many failed authentications" })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// Router built over :memory: state + temp audit file; the audit path
    /// is kept for assertions and cleaned up on drop. If `mint_agent` is
    /// given, a token is minted for that agent (before the state moves into
    /// the router) and its raw value returned.
    struct Fixture {
        app: Router,
        audit_path: PathBuf,
    }

    fn fixture(mint_agent: Option<&str>) -> (Fixture, Option<String>) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir()
            .join(format!("gate-routes-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let audit_path = dir.join("audit.jsonl");
        let state = DbState::open(":memory:").unwrap();
        let audit = Audit::open(&audit_path);
        let store = TokenStore::new(&state, &audit);
        let raw = mint_agent.map(|agent| store.mint(agent).unwrap());
        drop(store);
        let app = router(GateState::new(state, audit));
        (Fixture { app, audit_path }, raw)
    }

    impl Fixture {
        async fn send(&self, req: axum::http::Request<Body>) -> Response {
            self.app.clone().oneshot(req).await.unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(dir) = self.audit_path.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    fn get(headers: &[(&str, String)]) -> axum::http::Request<Body> {
        let mut b = axum::http::Request::builder()
            .method("GET")
            .uri("/whoami");
        for (k, v) in headers {
            b = b.header(*k, v);
        }
        b.body(Body::empty()).unwrap()
    }

    fn basic(user: &str, token: &str) -> (&'static str, String) {
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(format!("{user}:{token}"));
        ("authorization", format!("Basic {encoded}"))
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!("body must be json: {e}; body: {:?}", String::from_utf8_lossy(&bytes))
        })
    }

    #[tokio::test]
    async fn no_auth_gets_github_shaped_401() {
        let (f, _) = fixture(None);
        let resp = f.send(get(&[])).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let www = resp
            .headers()
            .get("www-authenticate")
            .expect("WWW-Authenticate header")
            .to_str()
            .unwrap();
        assert_eq!(www, "Basic realm=\"gate\"");
        let v = body_json(resp).await;
        assert_eq!(v["message"], "Bad credentials");
    }

    #[tokio::test]
    async fn basic_valid_token_injects_agent_identity() {
        let (f, raw) = fixture(Some("claude-code"));
        let resp = f
            .send(get(&[basic("claude-code", &raw.unwrap())]))
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["agent"], "claude-code");
    }

    #[tokio::test]
    async fn bearer_valid_token_ok() {
        let (f, raw) = fixture(Some("codex-cli"));
        let resp = f
            .send(get(&[("authorization", format!("Bearer {}", raw.unwrap()))]))
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["agent"], "codex-cli");
    }

    #[tokio::test]
    async fn malformed_authorization_is_401() {
        let (f, _) = fixture(Some("claude-code"));
        // Basic without a password (colon) part.
        let encoded = base64::engine::general_purpose::STANDARD.encode("solo-user");
        let resp = f
            .send(get(&[("authorization", format!("Basic {encoded}"))]))
            .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // Unknown scheme.
        let resp = f
            .send(get(&[("authorization", "Token ghp_whatever".to_string())]))
            .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_token_is_401_not_429() {
        let (f, _) = fixture(Some("claude-code"));
        let resp = f.send(get(&[basic("claude-code", "wrong-token")])).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn six_rapid_failures_yield_github_shaped_429() {
        let (f, _) = fixture(Some("claude-code"));
        let mut statuses = Vec::new();
        for i in 0..6 {
            let resp = f
                .send(get(&[basic("claude-code", &format!("bad-{i}"))]))
                .await;
            statuses.push(resp.status());
            if i == 5 {
                let v = body_json(resp).await;
                assert_eq!(v["message"], "Too many failed authentications");
            }
        }
        assert_eq!(
            statuses,
            vec![
                StatusCode::UNAUTHORIZED,
                StatusCode::UNAUTHORIZED,
                StatusCode::UNAUTHORIZED,
                StatusCode::UNAUTHORIZED,
                StatusCode::UNAUTHORIZED,
                StatusCode::TOO_MANY_REQUESTS,
            ],
            "five 401s then the block kicks in"
        );
    }

    #[tokio::test]
    async fn blocked_source_rejects_even_valid_tokens() {
        let (f, raw) = fixture(Some("claude-code"));
        for i in 0..5 {
            let resp = f
                .send(get(&[basic("claude-code", &format!("bad-{i}"))]))
                .await;
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }
        let resp = f
            .send(get(&[(
                "authorization",
                format!("Bearer {}", raw.unwrap()),
            )]))
            .await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}
