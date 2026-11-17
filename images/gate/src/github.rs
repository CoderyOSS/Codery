// GitHub adapter (GATE-008): the ONLY module that may contain
// `api.github.com` / `github.com` literals. Everything forge-specific
// (URLs, REST payloads, status mapping) lives behind the [`Platform`]
// trait so the rest of the gate stays forge-agnostic.
//
// Every mapping fails closed (GATE-005): transport errors, non-200
// statuses and unrecognized payloads collapse to `Ancestry::Unknown` /
// `RepoAccessError`, never to an allow-looking value.

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;
use std::time::Duration;

use crate::pktline::Oid;
use crate::types::{Ancestry, Role};

/// What the gate needs from the forge upstream (GATE-008).
///
/// `#[async_trait]` keeps the methods object-safe so later tasks can hold
/// `Arc<dyn Platform>` (sphere resolver / server); signatures are
/// otherwise identical to the plan's.
#[async_trait]
pub trait Platform: Send + Sync {
    /// Is `new` a strict descendant of `old`? Definitive only when the
    /// upstream compare says so; anything else is [`Ancestry::Unknown`].
    async fn ancestry(&self, owner: &str, repo: &str, old: &Oid, new: &Oid) -> Ancestry;

    /// The authenticated caller's role on a repo and whether it is private.
    /// 404 means the repo is invisible to the caller (`NoAccess`), not that
    /// it does not exist.
    async fn repo_permissions(&self, owner: &str, repo: &str)
        -> Result<RepoAccess, RepoAccessError>;

    /// Push URL for a repo hosted upstream.
    fn git_upstream_url(&self, owner: &str, repo: &str) -> Url;

    /// REST API root this platform talks to.
    fn api_base(&self) -> Url;

    /// The commits an update would bring, newest-first, rendered as
    /// `"<sha> <first line of commit.message>"` — the human-readable commit
    /// list shown on a push approval. Any upstream failure yields an empty
    /// list (the approval just carries fewer details, never an error).
    async fn commits_between(&self, owner: &str, repo: &str, old: &Oid, new: &Oid)
        -> Vec<String>;

    /// Is the machine account an active member of `org`? Anything but a
    /// 200 with `state == "active"` is false (fail closed).
    async fn org_member(&self, org: &str) -> bool;

    /// Verify the machine credential at request time: GET /user with the
    /// custody token. `None` when custody is empty or the upstream rejects
    /// the token — the API proxy forwards NOTHING in that state.
    async fn user_login(&self) -> Option<String>;

    /// Verify a machine-account PAT at intake (GATE-004): GET /user with
    /// the candidate token; the returned `login` must equal
    /// `expected_login` (passed by the caller — the adapter itself holds
    /// no notion of the machine login), `two_factor_authentication` must
    /// be true, all required scopes present (first missing reported), and
    /// the `github-authentication-token-expiration` header captured
    /// (`None` = no expiry; accepting that is the CALLER's decision, with
    /// a warning). The token is used only in the request header — never
    /// logged, never stored by the adapter.
    async fn verify_token(
        &self,
        token: &str,
        expected_login: &str,
    ) -> Result<VerifiedToken, VerifyError>;
}

/// Result of a successful machine-token verification (GATE-004): the
/// machine identity and the token's expiry as GitHub reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedToken {
    pub login: String,
    pub id: i64,
    /// `github-authentication-token-expiration` header value; `None` when
    /// absent (token never expires).
    pub expires: Option<String>,
}

/// Intake verification failure (GATE-004), in check order: wrong login,
/// 2FA off, first missing scope, anything upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// GET /user answered, but for a different login — carries the login
    /// found (typically the user's main account).
    WrongLogin(String),
    /// The machine account has `two_factor_authentication: false`.
    TwoFactorOff,
    /// First missing required scope, in `repo, workflow, read:org` order.
    MissingScope(String),
    /// Any other upstream failure; carries the HTTP status, or 0 when the
    /// failure was at the transport level.
    Upstream(u16),
}

/// Scopes the machine PAT must carry, in report order.
const REQUIRED_SCOPES: &[&str] = &["repo", "workflow", "read:org"];

/// Prefilled classic-PAT creation page for the machine account (GATE-004
/// §6.4 rotate intake): correct scopes, ready-made description. Like every
/// `github.com` literal, lives only in this module.
pub const TOKEN_NEW_URL: &str = "https://github.com/settings/tokens/new?scopes=repo,workflow,read:org&description=GitHub%20agent%20proxy";

/// The machine account's token list on github.com — where the operator
/// deletes the old/revoked token by hand (the gate has no API for it).
pub const TOKENS_LIST_URL: &str = "https://github.com/settings/tokens";

/// The authenticated caller's access to a repo (GATE-008).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepoAccess {
    pub role: Role,
    pub private: bool,
}

/// Permission lookup failure (GATE-008), fail closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoAccessError {
    /// 404: repo does not exist or the caller cannot see it.
    NoAccess,
    /// Any other upstream failure; carries the HTTP status, or 0 when the
    /// failure was at the transport level (no HTTP response at all).
    Upstream(u16),
}

/// The `permissions` object of a GitHub repo resource: one boolean per
/// tier, all absent/false meaning no access at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct PermissionsObj {
    #[serde(default)]
    pub admin: bool,
    #[serde(default)]
    pub maintain: bool,
    #[serde(default)]
    pub push: bool,
    #[serde(default)]
    pub triage: bool,
    #[serde(default)]
    pub pull: bool,
}

/// Highest permission bit that is set; none set ⇒ [`Role::None`].
pub fn role_of(permissions: &PermissionsObj) -> Role {
    if permissions.admin {
        Role::Admin
    } else if permissions.maintain {
        Role::Maintain
    } else if permissions.push {
        Role::Write
    } else if permissions.triage {
        Role::Triage
    } else if permissions.pull {
        Role::Read
    } else {
        Role::None
    }
}

#[derive(Debug, Deserialize)]
struct CompareResponse {
    status: String,
    #[serde(default)]
    commits: Vec<CompareCommit>,
}

#[derive(Debug, Deserialize)]
struct CompareCommit {
    sha: String,
    #[serde(default)]
    commit: CompareCommitMeta,
}

#[derive(Debug, Default, Deserialize)]
struct CompareCommitMeta {
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct MembershipResponse {
    #[serde(default)]
    state: String,
}

#[derive(Debug, Deserialize)]
struct RepoResponse {
    #[serde(default)]
    private: bool,
    #[serde(default)]
    permissions: PermissionsObj,
}

#[derive(Debug, Deserialize)]
struct UserResponse {
    #[serde(default)]
    login: String,
    #[serde(default)]
    id: i64,
    #[serde(default)]
    two_factor_authentication: bool,
}

/// REST client for GitHub with per-request machine-token injection. The
/// token getter is consulted for every request and its value is never
/// logged; custody of the token itself is wired in later tasks.
pub struct GitHubPlatform {
    client: Client,
    api_base: Url,
    git_base: Url,
    token: Arc<dyn Fn() -> Option<String> + Send + Sync>,
}

impl GitHubPlatform {
    /// Production platform: github.com bases, caller-supplied token custody.
    pub fn new(token: Arc<dyn Fn() -> Option<String> + Send + Sync>) -> Self {
        let api_base = Url::parse("https://api.github.com/").expect("hard-coded api base");
        let git_base = Url::parse("https://github.com/").expect("hard-coded git base");
        Self::with_bases(api_base, git_base, token)
    }

    /// Explicit bases (tests point these at a mock server).
    pub fn with_bases(
        api_base: Url,
        git_base: Url,
        token: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    ) -> Self {
        let mut builder = Client::builder()
            // GitHub's API rejects requests without a User-Agent.
            .user_agent(concat!("gate/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(10));
        // Test builds drop idle keep-alive sockets immediately: pooled
        // connections outliving their wiremock server get their port
        // recycled by another test's server, and the next request over the
        // stale socket lands on the wrong listener — a nondeterministic
        // cross-test request leak (seen as flaky `received_requests`
        // assertions). Production keeps default pooling.
        if cfg!(test) {
            builder = builder.pool_max_idle_per_host(0);
        }
        let client = builder
            .build()
            .expect("reqwest client with rustls backend");
        Self {
            client,
            api_base,
            git_base,
            token,
        }
    }

    /// Attach the bearer token when custody has one. The value is used only
    /// in the header — never logged, never stored on the request struct.
    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match (self.token)() {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    fn compare_url(&self, owner: &str, repo: &str, old: &Oid, new: &Oid) -> Option<Url> {
        Url::parse(&format!(
            "{}/repos/{}/{}/compare/{}...{}",
            self.api_base.as_str().trim_end_matches('/'),
            owner,
            repo,
            old.to_hex(),
            new.to_hex(),
        ))
        .ok()
    }

    fn repo_url(&self, owner: &str, repo: &str) -> Option<Url> {
        Url::parse(&format!(
            "{}/repos/{}/{}",
            self.api_base.as_str().trim_end_matches('/'),
            owner,
            repo,
        ))
        .ok()
    }
}

#[async_trait]
impl Platform for GitHubPlatform {
    async fn ancestry(&self, owner: &str, repo: &str, old: &Oid, new: &Oid) -> Ancestry {
        let Some(url) = self.compare_url(owner, repo, old, new) else {
            return Ancestry::Unknown;
        };
        let response = match self.authorize(self.client.get(url)).send().await {
            Ok(response) => response,
            Err(_) => return Ancestry::Unknown,
        };
        if response.status() != StatusCode::OK {
            // 404 (more than 250 commits apart, unknown refs) and every
            // other status are equally "cannot answer".
            return Ancestry::Unknown;
        }
        let body: CompareResponse = match response.json().await {
            Ok(body) => body,
            Err(_) => return Ancestry::Unknown,
        };
        match body.status.as_str() {
            "ahead" => Ancestry::FastForward,
            "behind" | "diverged" => Ancestry::Force,
            _ => Ancestry::Unknown,
        }
    }

    async fn repo_permissions(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<RepoAccess, RepoAccessError> {
        let Some(url) = self.repo_url(owner, repo) else {
            return Err(RepoAccessError::Upstream(0));
        };
        let response = self
            .authorize(self.client.get(url))
            .send()
            .await
            .map_err(|_| RepoAccessError::Upstream(0))?;
        match response.status() {
            StatusCode::OK => {
                let body: RepoResponse = response
                    .json()
                    .await
                    .map_err(|_| RepoAccessError::Upstream(StatusCode::OK.as_u16()))?;
                Ok(RepoAccess {
                    role: role_of(&body.permissions),
                    private: body.private,
                })
            }
            StatusCode::NOT_FOUND => Err(RepoAccessError::NoAccess),
            status => Err(RepoAccessError::Upstream(status.as_u16())),
        }
    }

    fn git_upstream_url(&self, owner: &str, repo: &str) -> Url {
        Url::parse(&format!(
            "{}/{}/{}.git",
            self.git_base.as_str().trim_end_matches('/'),
            owner,
            repo,
        ))
        .expect("owner/repo appended to a valid base form a valid url")
    }

    fn api_base(&self) -> Url {
        self.api_base.clone()
    }

    async fn commits_between(
        &self,
        owner: &str,
        repo: &str,
        old: &Oid,
        new: &Oid,
    ) -> Vec<String> {
        let Some(url) = self.compare_url(owner, repo, old, new) else {
            return Vec::new();
        };
        let response = match self.authorize(self.client.get(url)).send().await {
            Ok(response) => response,
            Err(_) => return Vec::new(),
        };
        if response.status() != StatusCode::OK {
            return Vec::new();
        }
        let body: CompareResponse = match response.json().await {
            Ok(body) => body,
            Err(_) => return Vec::new(),
        };
        body.commits
            .into_iter()
            .map(|c| {
                let subject = c
                    .commit
                    .message
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim_end()
                    .to_string();
                format!("{} {}", c.sha, subject)
            })
            .collect()
    }

    async fn org_member(&self, org: &str) -> bool {
        let Ok(url) = Url::parse(&format!(
            "{}/user/memberships/orgs/{}",
            self.api_base.as_str().trim_end_matches('/'),
            org,
        )) else {
            return false;
        };
        let response = match self.authorize(self.client.get(url)).send().await {
            Ok(response) => response,
            Err(_) => return false,
        };
        if response.status() != StatusCode::OK {
            return false;
        }
        match response.json::<MembershipResponse>().await {
            Ok(m) => m.state == "active",
            Err(_) => false,
        }
    }

    async fn user_login(&self) -> Option<String> {
        // Empty custody ⇒ no credential, without an upstream round trip.
        let token = (self.token)()?;
        let url = self.api_base.join("user").ok()?;
        let response = match self.client.get(url).bearer_auth(token).send().await {
            Ok(response) => response,
            Err(_) => return None,
        };
        if response.status() != StatusCode::OK {
            return None;
        }
        let body: UserResponse = response.json().await.ok()?;
        if body.login.is_empty() {
            None
        } else {
            Some(body.login)
        }
    }

    async fn verify_token(
        &self,
        token: &str,
        expected_login: &str,
    ) -> Result<VerifiedToken, VerifyError> {
        let url = self
            .api_base
            .join("user")
            .map_err(|_| VerifyError::Upstream(0))?;
        let response = self
            .client
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| VerifyError::Upstream(0))?;
        let status = response.status();
        if status != StatusCode::OK {
            return Err(VerifyError::Upstream(status.as_u16()));
        }
        // Headers captured before the body is consumed; the token itself
        // lives only in the request header and is never logged.
        let scopes = response
            .headers()
            .get("x-oauth-scopes")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_string());
        let expires = response
            .headers()
            .get("github-authentication-token-expiration")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_string());
        let body: UserResponse = response
            .json()
            .await
            .map_err(|_| VerifyError::Upstream(StatusCode::OK.as_u16()))?;

        // Check order = report order (GATE-004): the pointed wrong-account
        // message first, then 2FA, then scopes.
        if body.login != expected_login {
            return Err(VerifyError::WrongLogin(body.login));
        }
        if !body.two_factor_authentication {
            return Err(VerifyError::TwoFactorOff);
        }
        let granted: Vec<String> = scopes
            .map(|h| {
                h.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if let Some(missing) = REQUIRED_SCOPES
            .iter()
            .find(|s| !granted.contains(&(*s).to_string()))
        {
            return Err(VerifyError::MissingScope(missing.to_string()));
        }
        Ok(VerifiedToken {
            login: body.login,
            id: body.id,
            expires,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use reqwest::Url;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::pktline::Oid;

    const OLD: &str = "1234567890abcdef1234567890abcdef12345678";
    const NEW: &str = "fedcba0987654321fedcba0987654321fedcba09";
    const COMPARE_PATH: &str =
        "/repos/octo/hello/compare/1234567890abcdef1234567890abcdef12345678...fedcba0987654321fedcba0987654321fedcba09";
    const REPO_PATH: &str = "/repos/octo/hello";

    fn oid(hex: &str) -> Oid {
        Oid::from_hex(hex).expect("fixture oid")
    }

    fn platform_at(server: &MockServer) -> GitHubPlatform {
        GitHubPlatform::with_bases(
            Url::parse(&server.uri()).expect("mock api base"),
            Url::parse("http://git.example.internal/").expect("mock git base"),
            Arc::new(|| None),
        )
    }

    fn platform_with_token(server: &MockServer, token: Option<&str>) -> GitHubPlatform {
        let token = token.map(str::to_string);
        GitHubPlatform::with_bases(
            Url::parse(&server.uri()).expect("mock api base"),
            Url::parse("http://git.example.internal/").expect("mock git base"),
            Arc::new(move || token.clone()),
        )
    }

    async fn compare_mock(server: &MockServer, status: u16, body: &str) {
        Mock::given(method("GET"))
            .and(path(COMPARE_PATH))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn compare_ahead_is_fast_forward() {
        let server = MockServer::start().await;
        compare_mock(&server, 200, r#"{"status":"ahead"}"#).await;
        let p = platform_at(&server);
        assert_eq!(
            p.ancestry("octo", "hello", &oid(OLD), &oid(NEW)).await,
            Ancestry::FastForward
        );
    }

    #[tokio::test]
    async fn compare_behind_is_force() {
        let server = MockServer::start().await;
        compare_mock(&server, 200, r#"{"status":"behind"}"#).await;
        let p = platform_at(&server);
        assert_eq!(
            p.ancestry("octo", "hello", &oid(OLD), &oid(NEW)).await,
            Ancestry::Force
        );
    }

    #[tokio::test]
    async fn compare_diverged_is_force() {
        let server = MockServer::start().await;
        compare_mock(&server, 200, r#"{"status":"diverged"}"#).await;
        let p = platform_at(&server);
        assert_eq!(
            p.ancestry("octo", "hello", &oid(OLD), &oid(NEW)).await,
            Ancestry::Force
        );
    }

    #[tokio::test]
    async fn compare_404_is_unknown() {
        let server = MockServer::start().await;
        compare_mock(&server, 404, r#"{"message":"Not Found"}"#).await;
        let p = platform_at(&server);
        assert_eq!(
            p.ancestry("octo", "hello", &oid(OLD), &oid(NEW)).await,
            Ancestry::Unknown
        );
    }

    #[tokio::test]
    async fn compare_500_is_unknown() {
        let server = MockServer::start().await;
        compare_mock(&server, 500, r#"{"message":"boom"}"#).await;
        let p = platform_at(&server);
        assert_eq!(
            p.ancestry("octo", "hello", &oid(OLD), &oid(NEW)).await,
            Ancestry::Unknown
        );
    }

    #[tokio::test]
    async fn compare_unknown_status_string_is_unknown() {
        let server = MockServer::start().await;
        compare_mock(&server, 200, r#"{"status":"something-else"}"#).await;
        let p = platform_at(&server);
        assert_eq!(
            p.ancestry("octo", "hello", &oid(OLD), &oid(NEW)).await,
            Ancestry::Unknown
        );
    }

    #[tokio::test]
    async fn compare_non_json_body_is_unknown() {
        let server = MockServer::start().await;
        compare_mock(&server, 200, "<html>upstream maintenance</html>").await;
        let p = platform_at(&server);
        assert_eq!(
            p.ancestry("octo", "hello", &oid(OLD), &oid(NEW)).await,
            Ancestry::Unknown
        );
    }

    #[tokio::test]
    async fn transport_error_is_unknown_ancestry() {
        // A dead upstream must fail closed, never guess.
        let server = MockServer::start().await;
        let uri = server.uri();
        drop(server);
        let p = GitHubPlatform::with_bases(
            Url::parse(&uri).expect("mock api base"),
            Url::parse("http://git.example.internal/").expect("mock git base"),
            Arc::new(|| None),
        );
        assert_eq!(
            p.ancestry("octo", "hello", &oid(OLD), &oid(NEW)).await,
            Ancestry::Unknown
        );
    }

    async fn repo_mock(server: &MockServer, status: u16, body: &str) {
        Mock::given(method("GET"))
            .and(path(REPO_PATH))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn permissions_push_is_write() {
        let server = MockServer::start().await;
        repo_mock(
            &server,
            200,
            r#"{"private":true,"permissions":{"push":true,"pull":true}}"#,
        )
        .await;
        let p = platform_at(&server);
        assert_eq!(
            p.repo_permissions("octo", "hello").await,
            Ok(RepoAccess {
                role: Role::Write,
                private: true,
            })
        );
    }

    #[tokio::test]
    async fn permissions_404_is_no_access() {
        let server = MockServer::start().await;
        repo_mock(&server, 404, r#"{"message":"Not Found"}"#).await;
        let p = platform_at(&server);
        assert_eq!(
            p.repo_permissions("octo", "hello").await,
            Err(RepoAccessError::NoAccess)
        );
    }

    #[tokio::test]
    async fn permissions_all_false_is_none() {
        let server = MockServer::start().await;
        repo_mock(
            &server,
            200,
            r#"{"private":false,"permissions":{"admin":false,"maintain":false,"push":false,"triage":false,"pull":false}}"#,
        )
        .await;
        let p = platform_at(&server);
        assert_eq!(
            p.repo_permissions("octo", "hello").await,
            Ok(RepoAccess {
                role: Role::None,
                private: false,
            })
        );
    }

    #[tokio::test]
    async fn permissions_503_is_upstream_error() {
        let server = MockServer::start().await;
        repo_mock(&server, 503, r#"{"message":"unavailable"}"#).await;
        let p = platform_at(&server);
        assert_eq!(
            p.repo_permissions("octo", "hello").await,
            Err(RepoAccessError::Upstream(503))
        );
    }

    #[tokio::test]
    async fn missing_permissions_object_is_role_none() {
        let server = MockServer::start().await;
        repo_mock(&server, 200, r#"{"private":false}"#).await;
        let p = platform_at(&server);
        assert_eq!(
            p.repo_permissions("octo", "hello").await,
            Ok(RepoAccess {
                role: Role::None,
                private: false,
            })
        );
    }

    #[tokio::test]
    async fn bearer_token_is_sent_when_getter_supplies_one() {
        let server = MockServer::start().await;
        // Only a request carrying exactly this header matches: without the
        // token the mock falls through to wiremock's default 404 and the
        // call would fail closed with NoAccess instead of succeeding.
        Mock::given(method("GET"))
            .and(path(REPO_PATH))
            .and(header("authorization", "Bearer s3cr3t"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"private":false,"permissions":{"pull":true}}"#),
            )
            .mount(&server)
            .await;
        let p = platform_with_token(&server, Some("s3cr3t"));
        assert_eq!(
            p.repo_permissions("octo", "hello").await,
            Ok(RepoAccess {
                role: Role::Read,
                private: false,
            })
        );
    }

    #[tokio::test]
    async fn no_token_means_no_authorization_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(REPO_PATH))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"private":false,"permissions":{"pull":true}}"#),
            )
            .mount(&server)
            .await;
        let p = platform_at(&server);
        assert_eq!(
            p.repo_permissions("octo", "hello").await,
            Ok(RepoAccess {
                role: Role::Read,
                private: false,
            })
        );
        let requests = server.received_requests().await.expect("recorded requests");
        // Judge only OUR call (path-matched): a sibling test's stray can
        // land on a recycled port and inflate the recording.
        let repo_calls: Vec<_> = requests
            .iter()
            .filter(|r| r.url.path() == "/repos/octo/hello")
            .collect();
        assert!(!repo_calls.is_empty(), "the repo call must be recorded");
        assert!(
            repo_calls
                .iter()
                .all(|r| !r.headers.contains_key("authorization")),
            "no Authorization header may be sent without a stored token"
        );
    }

    #[tokio::test]
    async fn default_bases_are_github() {
        let p = GitHubPlatform::new(Arc::new(|| None));
        assert_eq!(p.api_base().as_str(), "https://api.github.com/");
        assert_eq!(
            p.git_upstream_url("octo", "hello").as_str(),
            "https://github.com/octo/hello.git"
        );
    }

    #[test]
    fn platform_is_dyn_compatible() {
        // Compiles only when `Platform` is object-safe (F2): later tasks
        // hold `Arc<dyn Platform>`.
        let p: Arc<dyn Platform> = Arc::new(GitHubPlatform::new(Arc::new(|| None)));
        let _: &(dyn Platform + '_) = &*p;
    }

    #[test]
    fn role_of_maps_highest_true_bit() {
        let all_true = PermissionsObj {
            admin: true,
            maintain: true,
            push: true,
            triage: true,
            pull: true,
        };
        assert_eq!(role_of(&all_true), Role::Admin);
        let maintain_only = PermissionsObj {
            admin: false,
            maintain: true,
            push: false,
            triage: false,
            pull: false,
        };
        assert_eq!(role_of(&maintain_only), Role::Maintain);
        let push_only = PermissionsObj {
            admin: false,
            maintain: false,
            push: true,
            triage: false,
            pull: true,
        };
        assert_eq!(role_of(&push_only), Role::Write);
        let triage_only = PermissionsObj {
            admin: false,
            maintain: false,
            push: false,
            triage: true,
            pull: false,
        };
        assert_eq!(role_of(&triage_only), Role::Triage);
        let pull_only = PermissionsObj {
            admin: false,
            maintain: false,
            push: false,
            triage: false,
            pull: true,
        };
        assert_eq!(role_of(&pull_only), Role::Read);
        assert_eq!(role_of(&PermissionsObj::default()), Role::None);
    }

    #[tokio::test]
    async fn commits_between_lists_sha_plus_subject_newest_first() {
        let server = MockServer::start().await;
        compare_mock(
            &server,
            200,
            r#"{"status":"diverged","commits":[
                {"sha":"aaa111","commit":{"message":"second commit\n\nbody"}},
                {"sha":"bbb222","commit":{"message":"first commit"}}
            ]}"#,
        )
        .await;
        let p = platform_at(&server);
        assert_eq!(
            p.commits_between("octo", "hello", &oid(OLD), &oid(NEW)).await,
            vec![
                "aaa111 second commit".to_string(),
                "bbb222 first commit".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn commits_between_failure_is_empty_vec() {
        let server = MockServer::start().await;
        compare_mock(&server, 404, r#"{"message":"Not Found"}"#).await;
        let p = platform_at(&server);
        assert!(p.commits_between("octo", "hello", &oid(OLD), &oid(NEW)).await.is_empty());
    }

    #[tokio::test]
    async fn commits_between_empty_commit_list_is_empty_vec() {
        let server = MockServer::start().await;
        compare_mock(&server, 200, r#"{"status":"ahead","commits":[]}"#).await;
        let p = platform_at(&server);
        assert!(p.commits_between("octo", "hello", &oid(OLD), &oid(NEW)).await.is_empty());
    }

    async fn membership_mock(server: &MockServer, status: u16, body: &str) {
        Mock::given(method("GET"))
            .and(path("/user/memberships/orgs/acme"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn org_member_active_state_is_true() {
        let server = MockServer::start().await;
        membership_mock(&server, 200, r#"{"state":"active"}"#).await;
        let p = platform_at(&server);
        assert!(p.org_member("acme").await);
    }

    #[tokio::test]
    async fn org_member_pending_state_is_false() {
        let server = MockServer::start().await;
        membership_mock(&server, 200, r#"{"state":"pending"}"#).await;
        let p = platform_at(&server);
        assert!(!p.org_member("acme").await);
    }

    #[tokio::test]
    async fn org_member_404_is_false() {
        let server = MockServer::start().await;
        membership_mock(&server, 404, r#"{"message":"Not Found"}"#).await;
        let p = platform_at(&server);
        assert!(!p.org_member("acme").await);
    }

    #[tokio::test]
    async fn org_member_non_json_is_false() {
        let server = MockServer::start().await;
        membership_mock(&server, 200, "<html>nope</html>").await;
        let p = platform_at(&server);
        assert!(!p.org_member("acme").await);
    }

    #[tokio::test]
    async fn user_login_returns_the_login_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .and(header("authorization", "Bearer s3cr3t"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"login":"gate-machine"}"#),
            )
            .mount(&server)
            .await;
        let p = platform_with_token(&server, Some("s3cr3t"));
        assert_eq!(p.user_login().await, Some("gate-machine".to_string()));
    }

    #[tokio::test]
    async fn user_login_is_none_when_upstream_rejects() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(
                ResponseTemplate::new(401).set_body_string(r#"{"message":"Bad credentials"}"#),
            )
            .mount(&server)
            .await;
        let p = platform_with_token(&server, Some("revoked-token"));
        assert_eq!(p.user_login().await, None);
    }

    #[tokio::test]
    async fn user_login_is_none_without_any_upstream_call_when_custody_empty() {
        // Canary proof of the short-circuit: if the platform called
        // upstream despite empty custody it would receive this marker
        // login and surface it. Deliberately NOT a
        // `received_requests().is_empty()` assertion — that recording is
        // nondeterministic under parallel tests: a sibling test's
        // in-flight request can land here after its own mock server was
        // dropped and the OS recycled its port (observed as flaky
        // `/repos/.../compare` strays).
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"login":"custody-leak-canary"}"#),
            )
            .mount(&server)
            .await;
        let p = platform_at(&server);
        assert_eq!(
            p.user_login().await,
            None,
            "empty custody must short-circuit before any upstream call"
        );
    }

    // -- verify_token (task 9, GATE-004 intake checks) ----------------------

    const VERIFY_TOKEN: &str = "ghp_verification_probe";
    const EXPECTED_LOGIN: &str = "gate-machine";

    fn ok_user_body(login: &str, two_fa: bool) -> String {
        format!(
            r#"{{"login":"{login}","id":4242,"two_factor_authentication":{two_fa}}}"#
        )
    }

    async fn mount_verify(
        server: &MockServer,
        status: u16,
        login: &str,
        two_fa: bool,
        scopes: Option<&str>,
        expires: Option<&str>,
        require_bearer: Option<&str>,
    ) {
        let mut tpl = ResponseTemplate::new(status);
        if status == 200 {
            tpl = tpl.set_body_string(ok_user_body(login, two_fa));
            if let Some(s) = scopes {
                tpl = tpl.append_header("X-OAuth-Scopes", s);
            }
            if let Some(e) = expires {
                tpl = tpl.append_header("github-authentication-token-expiration", e);
            }
        }
        let mut mock = Mock::given(method("GET")).and(path("/user"));
        if let Some(bearer) = require_bearer {
            mock = mock.and(header("authorization", format!("Bearer {bearer}")));
        }
        mock.respond_with(tpl).mount(server).await;
    }

    #[tokio::test]
    async fn verify_token_success_returns_login_id_and_captured_expiry() {
        let server = MockServer::start().await;
        mount_verify(
            &server,
            200,
            EXPECTED_LOGIN,
            true,
            Some("repo, workflow, read:org"),
            Some("2027-05-01T00:00:00Z"),
            Some(VERIFY_TOKEN),
        )
        .await;
        let p = platform_at(&server);
        let vt = p
            .verify_token(VERIFY_TOKEN, EXPECTED_LOGIN)
            .await
            .expect("valid machine token must verify");
        assert_eq!(vt.login, EXPECTED_LOGIN);
        assert_eq!(vt.id, 4242, "id stored for commit identity (GATE-004)");
        assert_eq!(vt.expires.as_deref(), Some("2027-05-01T00:00:00Z"));
    }

    #[tokio::test]
    async fn verify_token_without_expiry_header_captures_none() {
        let server = MockServer::start().await;
        mount_verify(
            &server,
            200,
            EXPECTED_LOGIN,
            true,
            Some("repo, workflow, read:org"),
            None,
            None,
        )
        .await;
        let p = platform_at(&server);
        let vt = p.verify_token(VERIFY_TOKEN, EXPECTED_LOGIN).await.unwrap();
        assert_eq!(vt.expires, None, "no header ⇒ no expiry");
    }

    #[tokio::test]
    async fn verify_token_login_mismatch_is_wrong_login_naming_found_login() {
        let server = MockServer::start().await;
        mount_verify(
            &server,
            200,
            "robsmith",
            true,
            Some("repo, workflow, read:org"),
            None,
            None,
        )
        .await;
        let p = platform_at(&server);
        assert_eq!(
            p.verify_token(VERIFY_TOKEN, EXPECTED_LOGIN).await,
            Err(VerifyError::WrongLogin("robsmith".to_string())),
            "a main-account token is rejected, naming the login found"
        );
    }

    #[tokio::test]
    async fn verify_token_two_factor_off_is_two_factor_off_error() {
        let server = MockServer::start().await;
        mount_verify(
            &server,
            200,
            EXPECTED_LOGIN,
            false,
            Some("repo, workflow, read:org"),
            None,
            None,
        )
        .await;
        let p = platform_at(&server);
        assert_eq!(
            p.verify_token(VERIFY_TOKEN, EXPECTED_LOGIN).await,
            Err(VerifyError::TwoFactorOff)
        );
    }

    #[tokio::test]
    async fn verify_token_missing_scope_names_first_missing_in_required_order() {
        // workflow is missing; repo and read:org are present.
        let server = MockServer::start().await;
        mount_verify(
            &server,
            200,
            EXPECTED_LOGIN,
            true,
            Some("repo, read:org"),
            None,
            None,
        )
        .await;
        let p = platform_at(&server);
        assert_eq!(
            p.verify_token(VERIFY_TOKEN, EXPECTED_LOGIN).await,
            Err(VerifyError::MissingScope("workflow".to_string()))
        );
        // No header at all: repo (first required scope) is reported.
        let bare = MockServer::start().await;
        mount_verify(&bare, 200, EXPECTED_LOGIN, true, None, None, None).await;
        let p_bare = platform_at(&bare);
        assert_eq!(
            p_bare.verify_token(VERIFY_TOKEN, EXPECTED_LOGIN).await,
            Err(VerifyError::MissingScope("repo".to_string()))
        );
    }

    #[tokio::test]
    async fn verify_token_non_200_is_upstream_error_with_status() {
        let server = MockServer::start().await;
        mount_verify(&server, 500, "", false, None, None, None).await;
        let p = platform_at(&server);
        assert_eq!(
            p.verify_token(VERIFY_TOKEN, EXPECTED_LOGIN).await,
            Err(VerifyError::Upstream(500))
        );
    }

    #[tokio::test]
    async fn verify_token_sends_the_token_as_bearer_only() {
        let server = MockServer::start().await;
        mount_verify(
            &server,
            200,
            EXPECTED_LOGIN,
            true,
            Some("repo, workflow, read:org"),
            None,
            Some(VERIFY_TOKEN),
        )
        .await;
        let p = platform_at(&server);
        // The mock only answers the bearer-authenticated request, so a
        // pass proves the token traveled in the Authorization header.
        assert!(p.verify_token(VERIFY_TOKEN, EXPECTED_LOGIN).await.is_ok());
    }
}
