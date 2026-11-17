//! Git smart-HTTP proxy (GATE-002/005): clone/fetch pass straight through
//! to the upstream forge with machine-account Basic auth; pushes are parsed
//! and classified first — the pipeline (`plan_push`) turns each ref update
//! into an allow / hold-for-approval / deny decision, and only a full
//! allow (or an approved hold) forwards the original bytes upstream.
//!
//! Everything fails closed: parse errors, kill-switched repos, unknown
//! ancestry, empty update sets and unrecognized-namespace deletes never
//! reach the upstream, and the agent's own credentials never do either —
//! upstream requests authenticate as the machine account only.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use base64::Engine as _;
use reqwest::Client;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::approval::{ApprovalBackend, ApprovalDetail, ApprovalOutcome, ApprovalRequest};
use crate::audit::{Audit, AuditEntry};
use crate::github::Platform;
use crate::pktline::{
    classify_ref_update, is_zero, parse_receive_pack_updates, ParseError, RefUpdate,
};
use crate::policy::PolicyEngine;
use crate::routes::GateState;
use crate::sphere::SphereResolver;
use crate::state::State as DbState;
use crate::tokens::AgentIdentity;
use crate::types::{ActionClass, Ancestry, Classification, Decision};

/// Path-grammar-valid owner/repo segment (T4 review pointer): ASCII
/// letters/digits/`.`/`_`/`-`, no `..` runs, never a bare `.` (URL
/// normalization would desync the gated repo key from the upstream path),
/// never empty.
fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Split the captured `owner` and `repo` (which carries the `.git` suffix
/// git always appends) into the validated pair the platform API wants.
fn split_repo_path(owner: &str, repo_raw: &str) -> Option<(String, String)> {
    let repo = repo_raw.strip_suffix(".git").unwrap_or(repo_raw);
    if !valid_segment(owner) || !valid_segment(repo) {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

/// Stable one-shot scope key (controller ruling 4): a hash over the agent,
/// repo and every `old:new:refname` triple, so an approval can never be
/// replayed on a different operation.
fn hold_id(agent: &str, repo: &str, updates: &[RefUpdate]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"gate-hold-v1");
    hasher.update(agent.as_bytes());
    hasher.update(b"|");
    hasher.update(repo.as_bytes());
    hasher.update(b"|");
    for u in updates {
        hasher.update(u.old.to_hex().as_bytes());
        hasher.update(b":");
        hasher.update(u.new.to_hex().as_bytes());
        hasher.update(b":");
        hasher.update(u.refname.as_bytes());
        hasher.update(b"|");
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn class_str(class: ActionClass) -> &'static str {
    match class {
        ActionClass::Push => "push",
        ActionClass::ForcePush => "force_push",
        ActionClass::BranchDelete => "branch_delete",
        ActionClass::TagDelete => "tag_delete",
    }
}

/// One pkt-line: 4 lowercase-hex length bytes (counting the header) then
/// the payload, exactly as git frames it.
pub(crate) fn pkt_line(payload: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", payload.len() + 4).into_bytes();
    out.extend_from_slice(payload);
    out
}

/// The push pipeline's verdict (plan-pinned shape).
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// Every ref decided Allow: forward the original bytes upstream.
    Forward,
    /// At least one ref decided Approve (or ancestry was Unknown): hold the
    /// whole push — the request carries ALL updates — until a human decides.
    Hold(ApprovalRequest),
    /// At least one ref decided Deny, or the body itself was unacceptable.
    /// Deny beats Hold and Forward (controller ruling 3).
    Deny(DenyReason),
}

/// Why the pipeline denied (drives the audit basis).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    /// The receive-pack body was not parseable pkt-line commands.
    ParseError(ParseError),
    /// Flush-only body: nothing to forward.
    EmptyUpdateSet,
    /// Repo kill-switched (GATE-005).
    KillSwitch,
    /// Delete of a ref outside the recognized namespaces (controller
    /// ruling 2): never consult the compare API with a zero `new`.
    UnknownNamespaceDelete(String),
    /// The policy file denied this class.
    PolicyDenied(ActionClass),
}

/// Ancestry + commit-list lookups for the pipeline, so `plan_push` stays a
/// pure function testable without a forge.
#[async_trait]
pub trait AncestrySource: Send + Sync {
    async fn ancestry(&self, update: &RefUpdate) -> Ancestry;
    async fn commits_between(&self, update: &RefUpdate) -> Vec<String>;
}

/// The real adapter over the gate's platform.
struct PlatformAdapter {
    platform: Arc<dyn Platform>,
    owner: String,
    repo: String,
}

#[async_trait]
impl AncestrySource for PlatformAdapter {
    async fn ancestry(&self, update: &RefUpdate) -> Ancestry {
        self.platform
            .ancestry(&self.owner, &self.repo, &update.old, &update.new)
            .await
    }

    async fn commits_between(&self, update: &RefUpdate) -> Vec<String> {
        self.platform
            .commits_between(&self.owner, &self.repo, &update.old, &update.new)
            .await
    }
}

/// Route-level adapter that remembers every ancestry verdict so the audit
/// basis can name the resolved action classes.
struct RouteAdapter {
    inner: PlatformAdapter,
    verdicts: Mutex<HashMap<String, Ancestry>>,
}

impl RouteAdapter {
    fn new(platform: Arc<dyn Platform>, owner: String, repo: String) -> Self {
        Self {
            inner: PlatformAdapter { platform, owner, repo },
            verdicts: Mutex::new(HashMap::new()),
        }
    }

    fn verdict_snapshot(&self) -> HashMap<String, Ancestry> {
        self.verdicts.lock().expect("verdict lock").clone()
    }
}

#[async_trait]
impl AncestrySource for RouteAdapter {
    async fn ancestry(&self, update: &RefUpdate) -> Ancestry {
        let verdict = self.inner.ancestry(update).await;
        self.verdicts
            .lock()
            .expect("verdict lock")
            .insert(update.refname.clone(), verdict);
        verdict
    }

    async fn commits_between(&self, update: &RefUpdate) -> Vec<String> {
        self.inner.commits_between(update).await
    }
}

/// Per-ref resolved action classes for the audit basis. Refs whose class
/// needed ancestry use the recorded verdict; Unknown labels conservatively
/// as `force_push`.
fn parsed_class_list(
    body: &[u8],
    verdicts: &HashMap<String, Ancestry>,
) -> Vec<&'static str> {
    let Ok(updates) = parse_receive_pack_updates(body) else {
        return Vec::new();
    };
    updates
        .iter()
        .filter_map(|u| match classify_ref_update(u) {
            Classification::Class(c) => Some(class_str(c)),
            Classification::NeedsAncestry if is_zero(&u.new) => None,
            Classification::NeedsAncestry => Some(match verdicts.get(&u.refname) {
                Some(Ancestry::FastForward) => "push",
                _ => "force_push",
            }),
        })
        .collect()
}

/// The classification pipeline (plan-pinned signature, plus the timeout the
/// built request carries and the agent/repo the one-shot id scopes over).
///
/// Decision order: kill-switch ⇒ deny; empty set ⇒ deny (ruling 1);
/// per-ref classify — static classes go straight to policy, unrecognized
/// namespace deletes deny without a compare call (ruling 2), ancestry
/// resolves FF⇒Push / Force⇒ForcePush / Unknown⇒forced Approve (GATE-005:
/// a compare the upstream could not answer holds for a human, never
/// forwards on a guess); Deny wins over Hold, Hold over Forward (ruling 3).
pub async fn plan_push(
    agent: &str,
    repo: &str,
    updates: &[RefUpdate],
    source: &dyn AncestrySource,
    policy: &PolicyEngine,
    blocked: bool,
    own_sphere: bool,
    timeout: Duration,
) -> Plan {
    if blocked {
        return Plan::Deny(DenyReason::KillSwitch);
    }
    if updates.is_empty() {
        return Plan::Deny(DenyReason::EmptyUpdateSet);
    }

    let mut trigger: Option<(usize, ActionClass)> = None;
    for (idx, update) in updates.iter().enumerate() {
        let (class, decision) = match classify_ref_update(update) {
            Classification::Class(class) => {
                (class, policy.decide(repo, class, own_sphere))
            }
            // Unrecognized namespace delete: never consult the compare API
            // with a zero `new` (ruling 2).
            Classification::NeedsAncestry if is_zero(&update.new) => {
                return Plan::Deny(DenyReason::UnknownNamespaceDelete(
                    update.refname.clone(),
                ));
            }
            Classification::NeedsAncestry => match source.ancestry(update).await {
                Ancestry::FastForward => (
                    ActionClass::Push,
                    policy.decide(repo, ActionClass::Push, own_sphere),
                ),
                Ancestry::Force => (
                    ActionClass::ForcePush,
                    policy.decide(repo, ActionClass::ForcePush, own_sphere),
                ),
                // Policy deny is the standing authority: consult it even on
                // an Unknown compare, and never hold a ref the config
                // denies — human approval permits exceptional ops, it can
                // never override an explicit deny. Without a deny,
                // GATE-005 still applies: a compare the upstream could not
                // answer is never guessed into an allow, so the ref is
                // labelled force-push and holds below.
                Ancestry::Unknown => {
                    let denied =
                        policy.decide(repo, ActionClass::ForcePush, own_sphere)
                            == Decision::Deny;
                    (
                        ActionClass::ForcePush,
                        if denied { Decision::Deny } else { Decision::Approve },
                    )
                }
            },
        };
        // Deny wins over Hold and Forward (ruling 3).
        if decision == Decision::Deny {
            return Plan::Deny(DenyReason::PolicyDenied(class));
        }
        if decision == Decision::Approve && trigger.is_none() {
            trigger = Some((idx, class));
        }
    }

    let Some((idx, class)) = trigger else {
        return Plan::Forward;
    };
    let triggering = &updates[idx];
    let commits = source.commits_between(triggering).await;
    Plan::Hold(ApprovalRequest {
        id: hold_id(agent, repo, updates),
        agent: agent.to_string(),
        repo: repo.to_string(),
        class,
        detail: ApprovalDetail::Push {
            updates: updates.to_vec(),
            commits,
        },
        timeout,
    })
}

/// Parse-then-plan: a body the parser rejects is a Deny before any
/// classification happens.
pub async fn evaluate_push_body(
    agent: &str,
    repo: &str,
    body: &[u8],
    source: &dyn AncestrySource,
    policy: &PolicyEngine,
    blocked: bool,
    own_sphere: bool,
    timeout: Duration,
) -> Plan {
    match parse_receive_pack_updates(body) {
        Err(e) => Plan::Deny(DenyReason::ParseError(e)),
        Ok(updates) => {
            plan_push(
                agent,
                repo,
                &updates,
                source,
                policy,
                blocked,
                own_sphere,
                timeout,
            )
            .await
        }
    }
}

/// Everything the git routes need beyond shared gate state.
pub struct GitDeps {
    pub platform: Arc<dyn Platform>,
    pub policy: PolicyEngine,
    pub backend: Arc<dyn ApprovalBackend>,
    pub sphere: SphereResolver,
    pub machine_login: String,
    pub machine_token: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    pub approval_timeout: Duration,
    pub http: Client,
}

impl GitDeps {
    pub fn new(
        registry: Arc<Mutex<DbState>>,
        platform: Arc<dyn Platform>,
        policy: PolicyEngine,
        backend: Arc<dyn ApprovalBackend>,
        machine_login: String,
        machine_token: Arc<dyn Fn() -> Option<String> + Send + Sync>,
        approval_timeout: Duration,
    ) -> anyhow::Result<Self> {
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            sphere: SphereResolver::new(registry, platform.clone()),
            platform,
            policy,
            backend,
            machine_login,
            machine_token,
            approval_timeout,
            http,
        })
    }

    fn basic_auth(&self) -> Option<HeaderValue> {
        let token = (self.machine_token)()?;
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!(
                "{}:{}",
                self.machine_login, token
            ));
        Some(HeaderValue::from_str(&format!("Basic {encoded}")).ok()?)
    }
}

/// The git smart-HTTP routes (mounted behind the auth middleware). The
/// body cap (`DefaultBodyLimit::max`) pairs with the capped reads in the
/// handlers: over-limit requests get a 413 before anything is buffered
/// (receive-pack) or forwarded (upload-pack streams).
pub fn router(state: Arc<GateState>) -> Router<Arc<GateState>> {
    Router::new()
        .route("/git/{owner}/{repo}/info/refs", get(info_refs))
        .route("/git/{owner}/{repo}/git-upload-pack", post(upload_pack))
        .route("/git/{owner}/{repo}/git-receive-pack", post(receive_pack))
        .layer(DefaultBodyLimit::max(state.max_body_bytes))
        .with_state(state)
}

fn audit_push(audit: &Audit, agent: &str, repo: &str, decision: &str, basis: &str) {
    audit.record(AuditEntry {
        ts: crate::state::now(),
        agent: Some(agent.to_string()),
        repo: repo.to_string(),
        action: "git.receive-pack".to_string(),
        decision: decision.to_string(),
        basis: basis.to_string(),
        approver: None,
    });
}

/// Audit a git-read request (GATE-005: reads pass through and are logged):
/// one record per request, in the same shape as [`audit_push`] but with
/// the per-read action name.
fn audit_read(
    audit: &Audit,
    agent: &str,
    repo: &str,
    action: &str,
    decision: &str,
    basis: &str,
) {
    audit.record(AuditEntry {
        ts: crate::state::now(),
        agent: Some(agent.to_string()),
        repo: repo.to_string(),
        action: action.to_string(),
        decision: decision.to_string(),
        basis: basis.to_string(),
        approver: None,
    });
}

/// Git-friendly 403: a pkt-line ERR payload so protocol-level consumers see
/// a clean remote reject (length header computed, flush-terminated).
fn err_response() -> Response {
    let mut body = pkt_line(b"ERR deny\n");
    body.extend_from_slice(b"0000");
    (
        StatusCode::FORBIDDEN,
        [(
            header::CONTENT_TYPE,
            "application/x-git-receive-pack-result",
        )],
        body,
    )
        .into_response()
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, msg.to_string()).into_response()
}

/// GitHub-shaped 413 (final-review body cap): the request exceeded
/// `GATE_MAX_BODY_BYTES` and was never forwarded upstream.
fn payload_too_large() -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        Json(json!({ "message": "Payload too large" })),
    )
        .into_response()
}

fn bad_gateway() -> Response {
    (StatusCode::BAD_GATEWAY, "gate: upstream unreachable".to_string()).into_response()
}

fn credential_unavailable() -> Response {
    (
        StatusCode::BAD_GATEWAY,
        "gate: machine credential unavailable".to_string(),
    )
        .into_response()
}

fn not_configured() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "gate: git proxy not configured".to_string(),
    )
        .into_response()
}

/// Status + content-type + streamed body, straight from upstream.
async fn passthrough_response(up: reqwest::Response) -> Response {
    let status =
        StatusCode::from_u16(up.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = up.headers().get(header::CONTENT_TYPE).cloned();
    let mut builder = Response::builder().status(status);
    if let Some(ct) = content_type {
        builder = builder.header(header::CONTENT_TYPE, ct);
    }
    builder
        .body(Body::from_stream(up.bytes_stream()))
        .unwrap_or_else(|_| bad_gateway())
}

/// Append a subpath to an upstream repo URL: `{repo}.git` + `/info/refs`
/// etc. (Url::path() never carries a trailing slash here).
fn upstream_subpath(url: &mut reqwest::Url, sub: &str) {
    let base = url.path().trim_end_matches('/');
    url.set_path(&format!("{base}/{sub}"));
}

async fn info_refs(
    State(gs): State<Arc<GateState>>,
    Extension(agent): Extension<AgentIdentity>,
    Path((owner_raw, repo_raw)): Path<(String, String)>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Response {
    let agent_name = agent.name.clone();
    let Some(service) = params.get("service").map(String::as_str) else {
        return bad_request("missing service parameter");
    };
    if service != "git-upload-pack" && service != "git-receive-pack" {
        return bad_request("unsupported service");
    }
    let Some((owner, repo)) = split_repo_path(&owner_raw, &repo_raw) else {
        return bad_request("invalid owner/repo path");
    };
    let Some(deps) = gs.git.clone() else {
        return not_configured();
    };
    // GATE-004 §6.3: the forwarding flag is consulted BEFORE custody —
    // flag off ⇒ credential_unavailable immediately (no custody load, no
    // upstream probe).
    if !gs.forwarding_allowed() {
        return credential_unavailable();
    }
    let Some(auth) = deps.basic_auth() else {
        return credential_unavailable();
    };
    let repo_full = format!("{owner}/{repo}");
    let mut url = deps.platform.git_upstream_url(&owner, &repo);
    upstream_subpath(&mut url, "info/refs");
    url.set_query(Some(&format!("service={service}")));
    match deps
        .http
        .get(url)
        .header(header::AUTHORIZATION, auth)
        .send()
        .await
    {
        Ok(up) => {
            audit_read(
                &gs.audit,
                &agent_name,
                &repo_full,
                "git.info-refs",
                "forward",
                &format!("service={service}"),
            );
            passthrough_response(up).await
        }
        Err(_) => {
            audit_read(
                &gs.audit,
                &agent_name,
                &repo_full,
                "git.info-refs",
                "deny",
                "upstream unreachable",
            );
            bad_gateway()
        }
    }
}

async fn upload_pack(
    State(gs): State<Arc<GateState>>,
    Extension(agent): Extension<AgentIdentity>,
    Path((owner_raw, repo_raw)): Path<(String, String)>,
    req: axum::extract::Request,
) -> Response {
    let agent_name = agent.name.clone();
    let Some((owner, repo)) = split_repo_path(&owner_raw, &repo_raw) else {
        return bad_request("invalid owner/repo path");
    };
    let Some(deps) = gs.git.clone() else {
        return not_configured();
    };
    // Forwarding flag gate (GATE-004 §6.3) — before custody, as above.
    if !gs.forwarding_allowed() {
        return credential_unavailable();
    }
    let Some(auth) = deps.basic_auth() else {
        return credential_unavailable();
    };
    let repo_full = format!("{owner}/{repo}");
    let mut url = deps.platform.git_upstream_url(&owner, &repo);
    upstream_subpath(&mut url, "git-upload-pack");
    let stream = req.into_body().into_data_stream();
    match deps
        .http
        .post(url)
        .header(header::AUTHORIZATION, auth)
        .header(header::CONTENT_TYPE, "application/x-git-upload-pack-request")
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
    {
        Ok(up) => {
            audit_read(
                &gs.audit,
                &agent_name,
                &repo_full,
                "git.upload-pack",
                "forward",
                "service=git-upload-pack",
            );
            passthrough_response(up).await
        }
        Err(_) => {
            audit_read(
                &gs.audit,
                &agent_name,
                &repo_full,
                "git.upload-pack",
                "deny",
                "upstream unreachable",
            );
            bad_gateway()
        }
    }
}

async fn receive_pack(
    State(gs): State<Arc<GateState>>,
    Extension(agent): Extension<AgentIdentity>,
    Path((owner_raw, repo_raw)): Path<(String, String)>,
    req: axum::extract::Request,
) -> Response {
    let agent_name = agent.name.clone();
    // Body reads are capped at the configured limit: an over-limit push
    // gets a 413 (GitHub-shaped), is audited, and never reaches upstream.
    let body = match axum::body::to_bytes(req.into_body(), gs.max_body_bytes).await {
        Ok(body) => body,
        Err(_) => {
            audit_push(
                &gs.audit,
                &agent_name,
                &format!("{owner_raw}/{repo_raw}"),
                "deny",
                "payload too large",
            );
            return payload_too_large();
        }
    };
    let Some((owner, repo)) = split_repo_path(&owner_raw, &repo_raw) else {
        audit_push(
            &gs.audit,
            &agent_name,
            &format!("{owner_raw}/{repo_raw}"),
            "deny",
            "invalid owner/repo path",
        );
        return bad_request("invalid owner/repo path");
    };
    let Some(deps) = gs.git.clone() else {
        return not_configured();
    };
    let repo_full = format!("{owner}/{repo}");

    let blocked = {
        let guard = gs.state.lock().expect("state lock must be poison-free");
        guard.is_blocked(&repo_full).unwrap_or(true)
    };
    let own_sphere = deps.sphere.is_own_sphere(&owner, &repo).await;

    let adapter = RouteAdapter::new(deps.platform.clone(), owner.clone(), repo.clone());
    let plan = evaluate_push_body(
        &agent_name,
        &repo_full,
        &body,
        &adapter,
        &deps.policy,
        blocked,
        own_sphere,
        deps.approval_timeout,
    )
    .await;
    let classes = parsed_class_list(&body, &adapter.verdict_snapshot());

    match plan {
        Plan::Deny(reason) => {
            let basis = match &reason {
                DenyReason::ParseError(e) => format!("parse-error: {e:?}"),
                DenyReason::EmptyUpdateSet => "empty update set".to_string(),
                DenyReason::KillSwitch => "kill-switch".to_string(),
                DenyReason::UnknownNamespaceDelete(refname) => {
                    format!("unrecognized-namespace delete: {refname}")
                }
                DenyReason::PolicyDenied(class) => format!(
                    "policy: deny ({})",
                    class_str(*class)
                ),
            };
            audit_push(&gs.audit, &agent_name, &repo_full, "deny", &basis);
            err_response()
        }
        Plan::Hold(req) => {
            let basis = format!("policy: approve; classes: {}", classes.join("+"));
            audit_push(&gs.audit, &agent_name, &repo_full, "hold", &basis);
            if deps.backend.present(&req).await.is_err() {
                audit_push(
                    &gs.audit,
                    &agent_name,
                    &repo_full,
                    "deny",
                    "approval backend unavailable",
                );
                return err_response();
            }
            match deps.backend.wait(&req.id).await {
                ApprovalOutcome::Approved => {
                    audit_push(
                        &gs.audit,
                        &agent_name,
                        &repo_full,
                        "forward",
                        "approval: approved",
                    );
                    forward_receive(&gs, &deps, &agent_name, &repo_full, &owner, &repo, body)
                        .await
                }
                ApprovalOutcome::Denied => {
                    audit_push(
                        &gs.audit,
                        &agent_name,
                        &repo_full,
                        "deny",
                        "approval: denied",
                    );
                    err_response()
                }
                ApprovalOutcome::TimedOut => {
                    audit_push(
                        &gs.audit,
                        &agent_name,
                        &repo_full,
                        "deny",
                        "approval: timed out",
                    );
                    err_response()
                }
            }
        }
        Plan::Forward => {
            let basis = format!("policy: allow; classes: {}", classes.join("+"));
            audit_push(&gs.audit, &agent_name, &repo_full, "forward", &basis);
            forward_receive(&gs, &deps, &agent_name, &repo_full, &owner, &repo, body).await
        }
    }
}

async fn forward_receive(
    gs: &GateState,
    deps: &GitDeps,
    agent: &str,
    repo_full: &str,
    owner: &str,
    repo: &str,
    body: Bytes,
) -> Response {
    // Forwarding flag gate (GATE-004 §6.3): nothing is forwarded — not
    // even the custody read — while the credential is unverified.
    if !gs.forwarding_allowed() {
        audit_push(
            &gs.audit,
            agent,
            repo_full,
            "deny",
            "machine credential unavailable",
        );
        return credential_unavailable();
    }
    let Some(auth) = deps.basic_auth() else {
        audit_push(
            &gs.audit,
            agent,
            repo_full,
            "deny",
            "machine credential unavailable",
        );
        return credential_unavailable();
    };
    let mut url = deps.platform.git_upstream_url(owner, repo);
    upstream_subpath(&mut url, "git-receive-pack");
    match deps
        .http
        .post(url)
        .header(header::AUTHORIZATION, auth)
        .header(header::CONTENT_TYPE, "application/x-git-receive-pack-request")
        .body(body)
        .send()
        .await
    {
        Ok(up) => passthrough_response(up).await,
        Err(_) => {
            audit_push(&gs.audit, agent, repo_full, "deny", "upstream unreachable");
            bad_gateway()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use super::*;
    use crate::pktline::Oid;
    use crate::policy::{PolicyEngine, PolicyFile};

    const OLD: &str = "1234567890abcdef1234567890abcdef12345678";
    const NEW: &str = "fedcba0987654321fedcba0987654321fedcba09";
    const OTHER_OLD: &str = "aaaabbbbccccddddeeeeffff0000111122223333";
    const OTHER_NEW: &str = "0000111122223333444455556666777788889999";
    const ZERO: &str = "0000000000000000000000000000000000000000";

    const TIMEOUT: Duration = Duration::from_secs(300);
    const AGENT: &str = "claude-code";
    const REPO: &str = "acme/widget";

    const ALLOW_ALL: &str = "\
[defaults]
push = \"allow\"
force-push = \"allow\"
branch-delete = \"allow\"
tag-delete = \"allow\"
";

    const APPROVE_FORCE: &str = "\
[defaults]
push = \"allow\"
force-push = \"approve\"
branch-delete = \"approve\"
tag-delete = \"approve\"
";

    const DENY_BRANCH_DELETE: &str = "\
[defaults]
push = \"allow\"
force-push = \"approve\"
branch-delete = \"approve\"
tag-delete = \"approve\"

[repos.\"acme/widget\"]
branch-delete = \"deny\"
";

    fn oid(hex: &str) -> Oid {
        Oid::from_hex(hex).expect("fixture oid")
    }

    fn update(old: &str, new: &str, refname: &str) -> RefUpdate {
        RefUpdate {
            old: oid(old),
            new: oid(new),
            refname: refname.to_string(),
        }
    }

    fn engine(src: &str) -> PolicyEngine {
        PolicyEngine::new(&PolicyFile::parse(src).expect("policy parse"))
    }

    /// Canned ancestry verdicts keyed by refname (default Unknown).
    struct Canned {
        ancestry: HashMap<String, Ancestry>,
        commits: Vec<String>,
    }

    impl Canned {
        fn with(refname: &str, ancestry: Ancestry) -> Self {
            let mut map = HashMap::new();
            map.insert(refname.to_string(), ancestry);
            Self {
                ancestry: map,
                commits: vec!["deadbeef a commit".to_string()],
            }
        }
    }

    #[async_trait]
    impl AncestrySource for Canned {
        async fn ancestry(&self, u: &RefUpdate) -> Ancestry {
            self.ancestry.get(&u.refname).copied().unwrap_or(Ancestry::Unknown)
        }
        async fn commits_between(&self, _u: &RefUpdate) -> Vec<String> {
            self.commits.clone()
        }
    }

    async fn plan(
        updates: &[RefUpdate],
        source: &dyn AncestrySource,
        policy: &PolicyEngine,
        blocked: bool,
        own_sphere: bool,
    ) -> Plan {
        plan_push(
            AGENT,
            REPO,
            updates,
            source,
            policy,
            blocked,
            own_sphere,
            TIMEOUT,
        )
        .await
    }

    #[tokio::test]
    async fn all_fast_forward_own_sphere_allow_forwards() {
        let src = Canned::with("refs/heads/main", Ancestry::FastForward);
        let updates = vec![update(OLD, NEW, "refs/heads/main")];
        assert_eq!(
            plan(&updates, &src, &engine(ALLOW_ALL), false, true).await,
            Plan::Forward
        );
    }

    #[tokio::test]
    async fn force_plus_ff_multi_ref_holds_with_both_updates() {
        let mut src = Canned::with("refs/heads/topic", Ancestry::Force);
        src.ancestry.insert("refs/heads/main".to_string(), Ancestry::FastForward);
        let updates = vec![
            update(OLD, NEW, "refs/heads/topic"),
            update(OTHER_OLD, OTHER_NEW, "refs/heads/main"),
        ];
        let Plan::Hold(req) =
            plan(&updates, &src, &engine(APPROVE_FORCE), false, true).await
        else {
            panic!("multi-ref approve must hold");
        };
        let ApprovalDetail::Push { updates: held, commits } = req.detail else {
            panic!("push detail");
        };
        assert_eq!(held.len(), 2, "the hold carries ALL updates");
        assert_eq!(held[0].refname, "refs/heads/topic");
        assert_eq!(held[1].refname, "refs/heads/main");
        assert_eq!(req.class, ActionClass::ForcePush, "triggering ref's class");
        assert_eq!(commits, vec!["deadbeef a commit".to_string()]);
        assert_eq!(req.agent, AGENT);
        assert_eq!(req.repo, REPO);
        assert_eq!(req.timeout, TIMEOUT);
    }

    #[tokio::test]
    async fn blocked_repo_denies() {
        let src = Canned::with("refs/heads/main", Ancestry::FastForward);
        let updates = vec![update(OLD, NEW, "refs/heads/main")];
        assert_eq!(
            plan(&updates, &src, &engine(ALLOW_ALL), true, true).await,
            Plan::Deny(DenyReason::KillSwitch)
        );
    }

    #[tokio::test]
    async fn parse_error_input_denies() {
        let src = Canned::with("refs/heads/main", Ancestry::FastForward);
        let plan = evaluate_push_body(
            AGENT,
            REPO,
            b"this is not pkt-line at all",
            &src,
            &engine(ALLOW_ALL),
            false,
            true,
            TIMEOUT,
        )
        .await;
        assert!(matches!(plan, Plan::Deny(DenyReason::ParseError(_))));
    }

    #[tokio::test]
    async fn empty_update_set_denies() {
        let src = Canned::with("refs/heads/main", Ancestry::FastForward);
        // Flush-only body parses Ok(vec![]) — still denied (ruling 1).
        let plan = evaluate_push_body(
            AGENT,
            REPO,
            b"0000",
            &src,
            &engine(ALLOW_ALL),
            false,
            true,
            TIMEOUT,
        )
        .await;
        assert_eq!(plan, Plan::Deny(DenyReason::EmptyUpdateSet));
    }

    #[tokio::test]
    async fn unrecognized_namespace_delete_denies_without_compare_call() {
        let src = Canned::with("refs/notes/x", Ancestry::FastForward);
        let updates = vec![update(OLD, ZERO, "refs/notes/x")];
        assert_eq!(
            plan(&updates, &src, &engine(ALLOW_ALL), false, true).await,
            Plan::Deny(DenyReason::UnknownNamespaceDelete("refs/notes/x".to_string()))
        );
    }

    #[tokio::test]
    async fn third_party_holds_even_when_policy_allows() {
        let src = Canned::with("refs/heads/main", Ancestry::FastForward);
        let updates = vec![update(OLD, NEW, "refs/heads/main")];
        let Plan::Hold(_) =
            plan(&updates, &src, &engine(ALLOW_ALL), false, false).await
        else {
            panic!("third-party mutation must hold, whatever the file says");
        };
    }

    #[tokio::test]
    async fn unknown_ancestry_holds_instead_of_guessing() {
        // GATE-005: a compare the upstream could not answer holds for a
        // human even though policy would allow force-pushes outright.
        let src = Canned::with("refs/heads/main", Ancestry::Unknown);
        let updates = vec![update(OLD, NEW, "refs/heads/main")];
        let Plan::Hold(req) =
            plan(&updates, &src, &engine(ALLOW_ALL), false, true).await
        else {
            panic!("Unknown ancestry must hold");
        };
        assert_eq!(req.class, ActionClass::ForcePush);
    }

    #[tokio::test]
    async fn unknown_ancestry_policy_deny_denies_without_hold() {
        // Policy deny is the standing authority: an Unknown-ancestry ref in
        // a force-push-denying repo denies outright — never held, so no
        // human decision can override the configured deny, and nothing is
        // forwarded upstream.
        let policy = engine("\
[defaults]
push = \"allow\"
force-push = \"deny\"
branch-delete = \"approve\"
tag-delete = \"approve\"
");
        let src = Canned::with("refs/heads/main", Ancestry::Unknown);
        let updates = vec![update(OLD, NEW, "refs/heads/main")];
        assert_eq!(
            plan(&updates, &src, &policy, false, true).await,
            Plan::Deny(DenyReason::PolicyDenied(ActionClass::ForcePush))
        );
    }

    #[tokio::test]
    async fn deny_wins_over_hold_in_multi_ref() {
        let mut src = Canned::with("refs/heads/topic", Ancestry::Force);
        src.ancestry.insert("refs/heads/old".to_string(), Ancestry::FastForward);
        let updates = vec![
            update(OLD, NEW, "refs/heads/topic"),
            update(OTHER_OLD, ZERO, "refs/heads/old"),
        ];
        assert_eq!(
            plan(&updates, &src, &engine(DENY_BRANCH_DELETE), false, true).await,
            Plan::Deny(DenyReason::PolicyDenied(ActionClass::BranchDelete))
        );
    }

    #[tokio::test]
    async fn hold_id_scopes_agent_repo_and_refs_deterministically() {
        let a = hold_id(AGENT, REPO, &[update(OLD, NEW, "refs/heads/main")]);
        let b = hold_id(AGENT, REPO, &[update(OLD, NEW, "refs/heads/main")]);
        assert_eq!(a, b, "same op => same one-shot id");
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));

        let new_sha = hold_id(AGENT, REPO, &[update(OLD, OTHER_NEW, "refs/heads/main")]);
        assert_ne!(a, new_sha, "different new oid => different id");

        let other_repo = hold_id(AGENT, "acme/other", &[update(OLD, NEW, "refs/heads/main")]);
        assert_ne!(a, other_repo, "different repo => different id");

        let other_agent = hold_id("codex-cli", REPO, &[update(OLD, NEW, "refs/heads/main")]);
        assert_ne!(a, other_agent, "different agent => different id");

        let multi = hold_id(
            AGENT,
            REPO,
            &[
                update(OLD, NEW, "refs/heads/main"),
                update(OTHER_OLD, OTHER_NEW, "refs/heads/dev"),
            ],
        );
        assert_ne!(a, multi, "different ref set => different id");
    }

    #[test]
    fn pkt_line_frames_length_correctly() {
        assert_eq!(pkt_line(b"AB"), b"0006AB");
        // "ERR deny\n" is 9 bytes => 13 = 0x000d.
        assert_eq!(pkt_line(b"ERR deny\n"), b"000dERR deny\n");
    }

    #[test]
    fn path_grammar_rejects_traversal_and_bad_chars() {
        assert!(valid_segment("octo"));
        assert!(valid_segment("hello.world_2-x"));
        assert!(!valid_segment(""));
        assert!(!valid_segment("."));
        assert!(!valid_segment(".."));
        assert!(!valid_segment("a..b"));
        assert!(!valid_segment("a/b"));
        assert!(!valid_segment("a?b"));
        assert!(!valid_segment("a#b"));
        assert!(!valid_segment("a b"));
        assert_eq!(
            split_repo_path("octo", "hello.git"),
            Some(("octo".to_string(), "hello".to_string()))
        );
        assert_eq!(split_repo_path("oc to", "hello.git"), None);
        assert_eq!(split_repo_path("octo", "../etc.git"), None);
        assert_eq!(split_repo_path(".", "hello.git"), None);
        assert_eq!(split_repo_path("octo", ".git"), None);
    }
}
