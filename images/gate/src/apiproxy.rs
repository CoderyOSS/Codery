//! API proxy (GATE-007): the GitHub-API half of the gate, so EVERY agent
//! action — git AND API — flows through it. `ANY /api/v3/{*path}` forwards
//! to `{api_base}/{path}` with the method, query string, headers (minus
//! hop-by-hop) and body preserved, authenticating upstream as the machine
//! account only (`Authorization: Bearer {token}`); the agent's own gate
//! credentials never leave the gate.
//!
//! Classification: GET/HEAD are reads (passthrough + audit); a POST to
//! `/api/v3/graphql` is a mutation only when the body contains
//! `"mutation"`, else a read; every other method is a mutation. Mutations
//! are policy-gated (`[api] mutations`, default `approve`; third-party
//! always approves per GATE-005), held through the approval backend on
//! Approve, and forwarded byte-identical once approved.
//!
//! Fail closed everywhere (Review Focus 5): without a VERIFIED machine
//! credential at request time (`Platform::user_login`) nothing is
//! forwarded, and upstream 4xx failures map to the GATE-010 agent error
//! shapes (`errors::AgentError`) instead of raw upstream leakage.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Extension, Json, Router};
use reqwest::Client;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::approval::{ApprovalBackend, ApprovalDetail, ApprovalOutcome, ApprovalRequest};
use crate::audit::{Audit, AuditEntry};
use crate::errors::AgentError;
use crate::github::{Platform, RepoAccessError};
use crate::policy::PolicyEngine;
use crate::rolemapping::{classify_api, min_role, role_from_name, role_name, Action};
use crate::routes::GateState;
use crate::sphere::SphereResolver;
use crate::state::State as DbState;
use crate::tokens::AgentIdentity;
use crate::types::{ActionClass, Decision, Role};

/// Cap on the body echo stored in an approval payload (`body_snippet`);
/// the full body is covered by `body_sha` regardless of size.
const SNIPPET_CAP: usize = 4096;

/// Hop-by-hop headers (RFC 9110 §7.6.1) plus the framing/credential
/// headers the gate must strip before forwarding: the agent's
/// `Authorization` is gate-internal and the machine bearer token replaces
/// it; `Host`/`Content-Length` are re-derived by the client.
const STRIPPED_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
    "authorization",
];

/// Everything the API routes need beyond shared gate state.
pub struct ApiDeps {
    pub platform: Arc<dyn Platform>,
    pub policy: PolicyEngine,
    pub backend: Arc<dyn ApprovalBackend>,
    pub sphere: SphereResolver,
    pub machine_token: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    pub approval_timeout: Duration,
    pub http: Client,
    /// Grant-offer hook (GATE-010): `Some` ⇒ a MUTATION whose upstream
    /// answer is a repo-scoped 404 (the machine account has no access)
    /// offers the Telegram grant flow, debounced once per repo per 24 h.
    /// `None` (no bot configured, or unit tests) ⇒ the error mapping
    /// alone, no offer message.
    pub offer: Option<crate::telegram::grant::GrantOfferDeps>,
    /// Personal-repo ceiling channel (GATE-010 §9.3): where the DIY
    /// instructions go when a Maintain/Admin need lands on a personal
    /// repo (no elevation flow can exist there). `None` ⇒ the denial
    /// alone.
    pub elevation_notify: Option<(Arc<dyn crate::telegram::TelegramApi>, i64)>,
}

impl ApiDeps {
    pub fn new(
        registry: Arc<Mutex<DbState>>,
        platform: Arc<dyn Platform>,
        policy: PolicyEngine,
        backend: Arc<dyn ApprovalBackend>,
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
            machine_token,
            approval_timeout,
            http,
            offer: None,
            elevation_notify: None,
        })
    }
}

/// The API proxy routes (mounted behind the auth middleware, like the git
/// proxy): `ANY /api/v3/{*path}` (+ the bare `/api/v3` root) and the
/// elevation status endpoint Plan B's control plane and agents poll. The
/// body cap (`DefaultBodyLimit::max`) pairs with the capped read in
/// [`handle_api`]: over-limit requests get a 413 before anything is
/// buffered.
pub fn router(state: Arc<GateState>) -> Router<Arc<GateState>> {
    Router::new()
        .route("/api/v3/{*path}", any(api_v3))
        .route("/api/v3", any(api_root))
        .route("/proxy/elevations/{id}", get(elevation))
        .layer(DefaultBodyLimit::max(state.max_body_bytes))
        .with_state(state)
}

async fn api_v3(
    State(gs): State<Arc<GateState>>,
    Extension(agent): Extension<AgentIdentity>,
    req: Request,
) -> Response {
    handle_api(&gs, &agent, req).await
}

/// Bare `/api/v3` root (upstream API discovery document) — same pipeline
/// with an empty path.
async fn api_root(
    State(gs): State<Arc<GateState>>,
    Extension(agent): Extension<AgentIdentity>,
    req: Request,
) -> Response {
    handle_api(&gs, &agent, req).await
}

async fn handle_api(gs: &GateState, agent: &AgentIdentity, req: Request) -> Response {
    let agent_name = agent.name.clone();
    let method = req.method().clone();
    let query = req.uri().query().map(str::to_string);
    let req_headers = req.headers().clone();
    // The path is taken verbatim from the request URI — no percent-decode /
    // re-encode round trip, which would change the %2F-in-segment semantics
    // some GitHub paths (e.g. /contents/a%2Fb) rely on.
    let raw_path = req.uri().path().to_string();
    // Body reads are capped at the configured limit: an over-limit request
    // gets a 413 (GitHub-shaped), is audited, and never reaches upstream.
    let body = match axum::body::to_bytes(req.into_body(), gs.max_body_bytes).await {
        Ok(body) => body,
        Err(_) => {
            audit_api(&gs.audit, &agent_name, "-", "deny", "payload too large");
            return payload_too_large();
        }
    };

    let Some(deps) = gs.api.clone() else {
        return not_configured();
    };

    let rest = raw_path
        .strip_prefix("/api/v3")
        .unwrap_or(&raw_path)
        .strip_prefix('/')
        .unwrap_or("")
        .to_string();

    // `..` segments must never reach URL construction (a decoded `../`
    // could otherwise escape the api base).
    if rest.split('/').any(|seg| seg == "..") {
        audit_api(&gs.audit, &agent_name, "-", "deny", "path traversal rejected");
        return bad_request("invalid path");
    }

    // GATE-004 §6.3: the master forwarding flag is consulted BEFORE the
    // credential probe — flag off ⇒ credential_unavailable immediately
    // (no upstream probe, no state churn). With the flag on, the probe
    // below remains the second line of defense.
    if !gs.forwarding_allowed() {
        audit_api(
            &gs.audit,
            &agent_name,
            "-",
            "deny",
            "forwarding disabled (machine credential unverified)",
        );
        return AgentError::credential_unavailable().into_response();
    }

    // Review Focus 5: a request may only proceed when the machine
    // credential verifies RIGHT NOW; otherwise nothing is forwarded.
    if deps.platform.user_login().await.is_none() {
        audit_api(
            &gs.audit,
            &agent_name,
            "-",
            "deny",
            "machine credential unavailable",
        );
        return AgentError::credential_unavailable().into_response();
    }

    let is_read = match method {
        Method::GET | Method::HEAD => true,
        // GraphQL special case: only a body containing "mutation" is a
        // mutation; queries (and GETs) are reads.
        Method::POST if rest == "graphql" => !contains_slice(&body, b"mutation"),
        _ => false,
    };

    if is_read {
        return forward_and_respond(
            gs,
            &deps,
            &agent_name,
            &method,
            &rest,
            query.as_deref(),
            &req_headers,
            &body,
            format!("read {method} /{rest}"),
        )
        .await;
    }

    // Mutation path: repo scoping → own sphere → pre-flight → policy.
    let scoped = repo_of_path(&rest);
    let repo_label = scoped
        .as_ref()
        .map(|(o, r)| format!("{o}/{r}"))
        .unwrap_or_else(|| "-".to_string());
    let own_sphere = match &scoped {
        Some((owner, repo)) => deps.sphere.is_own_sphere(owner, repo).await,
        // GraphQL and other non-repo-scoped mutations cannot be proven
        // own-sphere: fail closed to the approval flow (GATE-005).
        None => false,
    };

    // Pre-flight role gap (GATE-010 role map, task B8): the minimum role
    // the map assigns to this action decides. Only own-sphere, repo-scoped,
    // write-method mutations on REGISTERED repos are checked; third-party
    // repos skip to approval. Fail-fast (binding ADR ruling): a gap
    // answers `permission_pending` immediately and opens (or joins) the
    // repo's elevation flow — the request NEVER waits on the role change;
    // the agent retries after the elevation completes.
    if let (Some((owner, repo)), true) = (&scoped, is_write_method(&method)) {
        if own_sphere {
            let needed = min_role(Action::Api(classify_api(method.as_str(), &format!("/{rest}"))));
            let needed_name = role_name(needed);
            let row = {
                let guard = gs.state.lock().expect("state lock must be poison-free");
                guard.registry_get(&repo_label).ok().flatten()
            };
            if let Some(row) = row {
                let granted = role_from_name(&row.granted_role).unwrap_or(Role::None);
                if granted < needed {
                    if let Some(resp) = personal_repo_ceiling(
                        gs,
                        &deps,
                        &agent_name,
                        owner,
                        &repo_label,
                        &format!("/{rest}"),
                        needed,
                    )
                    .await
                    {
                        return resp;
                    }
                    let id = {
                        let guard =
                            gs.state.lock().expect("state lock must be poison-free");
                        crate::telegram::elevation::open_or_join_flow(
                            &guard,
                            &repo_label,
                            &format!("/{rest}"),
                            needed,
                        )
                    };
                    audit_api(
                        &gs.audit,
                        &agent_name,
                        &repo_label,
                        "deny",
                        &format!("preflight: registry role below {needed_name}"),
                    );
                    return AgentError::permission_pending(id, needed_name).into_response();
                }
                // Registry grant covers the map; verify it still holds.
                match deps.platform.repo_permissions(owner, repo).await {
                    Err(RepoAccessError::NoAccess) => {
                        // GATE-010 grant-offer trigger: a mutation hit a
                        // repo the machine account cannot see — offer the
                        // grant flow (debounced once per repo per 24 h).
                        if let Some(offer) = &deps.offer {
                            let _ = crate::telegram::grant::maybe_offer_grant(
                                &offer.state,
                                &offer.notify,
                                &repo_label,
                                &offer.machine_login,
                            )
                            .await;
                        }
                        audit_api(
                            &gs.audit,
                            &agent_name,
                            &repo_label,
                            "deny",
                            "preflight: repo no-access",
                        );
                        return AgentError::access_pending(&repo_label).into_response();
                    }
                    // A transient upstream failure must not invent a deny;
                    // let forwarding surface the real error.
                    Err(RepoAccessError::Upstream(_)) => {}
                    Ok(access) if access.role < needed => {
                        if let Some(resp) = personal_repo_ceiling(
                            gs,
                            &deps,
                            &agent_name,
                            owner,
                            &repo_label,
                            &format!("/{rest}"),
                            needed,
                        )
                        .await
                        {
                            return resp;
                        }
                        let id = {
                            let guard = gs
                                .state
                                .lock()
                                .expect("state lock must be poison-free");
                            crate::telegram::elevation::open_or_join_flow(
                                &guard,
                                &repo_label,
                                &format!("/{rest}"),
                                needed,
                            )
                        };
                        audit_api(
                            &gs.audit,
                            &agent_name,
                            &repo_label,
                            "deny",
                            &format!("preflight: live role below {needed_name}"),
                        );
                        return AgentError::permission_pending(id, needed_name).into_response();
                    }
                    Ok(_) => {}
                }
            }
        }
    }

    match deps.policy.decide_api(own_sphere) {
        Decision::Deny => {
            audit_api(
                &gs.audit,
                &agent_name,
                &repo_label,
                "deny",
                "policy: deny (api.mutations)",
            );
            // Plain GitHub-shaped deny: the AgentError set carries
            // access/permission/credential semantics, a policy deny needs
            // none of those.
            deny_response()
        }
        Decision::Allow => {
            forward_and_respond(
                gs,
                &deps,
                &agent_name,
                &method,
                &rest,
                query.as_deref(),
                &req_headers,
                &body,
                "policy: allow (api.mutations)".to_string(),
            )
            .await
        }
        Decision::Approve => {
            let sha = body_sha(&body);
            let hold = ApprovalRequest {
                id: hold_id_api(&agent_name, &repo_label, method.as_str(), &rest, &sha),
                agent: agent_name.clone(),
                repo: repo_label.clone(),
                // API mutations have no git class; Push is the "ordinary
                // mutation" stand-in — the human-readable detail lives in
                // ApprovalDetail::Api (method/path/snippet/sha).
                class: ActionClass::Push,
                detail: ApprovalDetail::Api {
                    method: method.to_string(),
                    path: format!("/{rest}"),
                    body_snippet: body_snippet(&body),
                    body_sha: sha,
                },
                timeout: deps.approval_timeout,
            };
            let basis = if own_sphere {
                "policy: approve (api.mutations)".to_string()
            } else {
                "policy: approve (third-party)".to_string()
            };
            audit_api(
                &gs.audit,
                &agent_name,
                &repo_label,
                "hold",
                &format!("{basis}; api {method} /{rest}"),
            );
            if deps.backend.present(&hold).await.is_err() {
                audit_api(
                    &gs.audit,
                    &agent_name,
                    &repo_label,
                    "deny",
                    "approval backend unavailable",
                );
                return deny_response();
            }
            match deps.backend.wait(&hold.id).await {
                ApprovalOutcome::Approved => {
                    forward_and_respond(
                        gs,
                        &deps,
                        &agent_name,
                        &method,
                        &rest,
                        query.as_deref(),
                        &req_headers,
                        &body,
                        "approval: approved".to_string(),
                    )
                    .await
                }
                ApprovalOutcome::Denied => {
                    audit_api(
                        &gs.audit,
                        &agent_name,
                        &repo_label,
                        "deny",
                        "approval: denied",
                    );
                    deny_response()
                }
                ApprovalOutcome::TimedOut => {
                    audit_api(
                        &gs.audit,
                        &agent_name,
                        &repo_label,
                        "deny",
                        "approval: timed out",
                    );
                    deny_response()
                }
            }
        }
    }
}

/// Forward the request upstream (machine bearer auth, stripped headers,
/// exact body) and map the response: GATE-010 agent errors replace mapped
/// upstream failures; everything else passes through with status +
/// content-type + body. `success_basis` is the audit basis used when the
/// upstream answer passes through (appended with `-> {status}`).
async fn forward_and_respond(
    gs: &GateState,
    deps: &ApiDeps,
    agent: &str,
    method: &Method,
    rest: &str,
    query: Option<&str>,
    req_headers: &HeaderMap,
    body: &[u8],
    success_basis: String,
) -> Response {
    let repo_label = repo_label_of(rest);
    // Custody could have emptied between the verified check and the
    // forward — re-check, still fail closed.
    let Some(token) = (deps.machine_token)() else {
        audit_api(
            &gs.audit,
            agent,
            &repo_label,
            "deny",
            "machine credential unavailable",
        );
        return AgentError::credential_unavailable().into_response();
    };
    let url = build_url(deps.platform.api_base(), rest, query);
    let mut rb = deps
        .http
        .request(method.clone(), url)
        .headers(forward_headers(req_headers))
        .bearer_auth(token);
    if !body.is_empty() {
        rb = rb.body(body.to_vec());
    }
    let up = match rb.send().await {
        Ok(up) => up,
        Err(_) => {
            audit_api(&gs.audit, agent, &repo_label, "deny", "upstream unreachable");
            return bad_gateway();
        }
    };
    let status = up.status().as_u16();
    let resp_headers = up.headers().clone();
    let resp_body = match up.bytes().await {
        Ok(body) => body,
        Err(_) => {
            audit_api(
                &gs.audit,
                agent,
                &repo_label,
                "deny",
                "upstream response unreadable",
            );
            return bad_gateway();
        }
    };
    let text = String::from_utf8_lossy(&resp_body).into_owned();
    let mapping = {
        let guard = gs.state.lock().expect("state lock must be poison-free");
        map_upstream_error(status, &resp_headers, &text, method.as_str(), rest, &guard)
    };
    match mapping {
        Ok(()) => {
            // §9.2 bookkeeping (task 9): a 2xx on a repo-scoped request
            // means the (possibly retried) action SUCCEEDED — record
            // work_done=1 on the repo's temporary elevation flow if one
            // exists. A record only: the reduction proceeds at DONE.
            if (200..300).contains(&status) && repo_label != "-" {
                let guard = gs.state.lock().expect("state lock must be poison-free");
                crate::telegram::elevation::mark_work_done(&guard, &repo_label);
            }
            audit_api(
                &gs.audit,
                agent,
                &repo_label,
                "forward",
                &format!("{success_basis} -> {status}"),
            );
            passthrough(status, &resp_headers, resp_body)
        }
        Err(e) => {
            // GATE-010 grant-offer trigger: a MUTATION just learned the
            // machine account has no access to this repo (upstream 404 on
            // a repo-scoped path). Reads never offer; a reachable public
            // repo never lands here (third-party goes through approval).
            if let (AgentError::AccessPending { repo, .. }, true) = (&e, is_write_method(method)) {
                if let Some(offer) = &deps.offer {
                    let _ = crate::telegram::grant::maybe_offer_grant(
                        &offer.state,
                        &offer.notify,
                        repo,
                        &offer.machine_login,
                    )
                    .await;
                }
            }
            // GATE-010 contradiction logging (B6 handoff): GitHub rejected
            // with a role-gap the registry's role actually covers — the map
            // said "allowed", reality said no. Audit it for map fixes.
            if let AgentError::PermissionPending { .. } = &e {
                log_rolemap_contradiction(gs, agent, method, rest, status);
            }
            audit_api(
                &gs.audit,
                agent,
                &repo_label,
                "deny",
                &format!("upstream {status} -> {}", e.kind()),
            );
            e.into_response()
        }
    }
}

/// Audit a `rolemap.contradiction` event when the upstream refused an
/// action (403 admin-rights) the role map + registry said was allowed.
/// Carries method + path so the P0 heuristic table is fixable. The audit
/// is deliberately one event per contradiction, keyed by method/path.
fn log_rolemap_contradiction(
    gs: &GateState,
    agent: &str,
    method: &Method,
    rest: &str,
    status: u16,
) {
    let Some((owner, repo)) = repo_of_path(rest) else {
        return;
    };
    let repo_label = format!("{owner}/{repo}");
    let needed = min_role(Action::Api(classify_api(method.as_str(), &format!("/{rest}"))));
    let granted = {
        let guard = gs.state.lock().expect("state lock must be poison-free");
        guard
            .registry_get(&repo_label)
            .ok()
            .flatten()
            .and_then(|r| role_from_name(&r.granted_role))
            .unwrap_or(Role::None)
    };
    if granted < needed {
        return; // the map agreed with reality — no contradiction
    }
    gs.audit.record(AuditEntry {
        ts: crate::state::now(),
        agent: Some(agent.to_string()),
        repo: repo_label,
        action: "rolemap.contradiction".to_string(),
        decision: "deny".to_string(),
        basis: format!(
            "{method} /{rest}: registry role {} covers {} but upstream answered {status}",
            role_name(granted),
            role_name(needed),
        ),
        approver: None,
    });
}

/// The personal-repo ceiling (GATE-010 §9.3): a Maintain/Admin need on a
/// personal repo CANNOT be elevated — GitHub offers role tiers only on
/// org repos, so no flow may exist. Returns `Some(response)` when the
/// ceiling applies: the agent gets the exact `role_unavailable` shape
/// (error + message, no elevation_id) and the operator gets the DIY
/// instructions. `None` ⇒ the need is below Maintain, or the owner is an
/// org, and elevation proceeds normally. The /orgs probe runs only for
/// Maintain/Admin needs (Write-tier gaps never consult it).
async fn personal_repo_ceiling(
    gs: &GateState,
    deps: &ApiDeps,
    agent: &str,
    owner: &str,
    repo_label: &str,
    path: &str,
    needed: Role,
) -> Option<Response> {
    if needed < Role::Maintain {
        return None;
    }
    if deps.platform.owner_is_org(owner).await {
        return None;
    }
    crate::telegram::elevation::maybe_send_diy(&deps.elevation_notify, repo_label, needed, path)
        .await;
    audit_api(
        &gs.audit,
        agent,
        repo_label,
        "deny",
        &format!(
            "preflight: personal-repo ceiling — {} cannot be granted on a personal repo; DIY instructions sent",
            role_name(needed)
        ),
    );
    Some(
        AgentError::RoleUnavailable {
            message: crate::telegram::elevation::diy_agent_message(repo_label, needed),
        }
        .into_response(),
    )
}

/// Elevation status endpoint: `{"id","state",...}` from the `flows` table
/// plus the gap detail (repo, needed role, temporary flag) agents poll
/// after a `permission_pending`. The elevation state machine (telegram::
/// elevation) rewrites `state` as the flow progresses.
async fn elevation(
    State(gs): State<Arc<GateState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let row = {
        let guard = gs.state.lock().expect("state lock must be poison-free");
        guard.flow_get(&id)
    };
    let row = match row {
        Ok(Some(row)) if row.kind == "elevation" => row,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "no such flow" })),
            )
                .into_response()
        }
    };
    let payload = crate::telegram::elevation::ElevationPayload::from_json(&row.payload_json);
    let mut body = json!({ "id": id, "state": row.flow_state });
    if !payload.repo.is_empty() {
        body["repo"] = json!(payload.repo);
    }
    if !payload.needed_role.is_empty() {
        body["needs_role"] = json!(payload.needed_role);
    }
    if let Some(temporary) = payload.temporary {
        body["temporary"] = json!(temporary);
    }
    (StatusCode::OK, Json(body)).into_response()
}

/// Map an upstream failure to a GATE-010 agent error; `Ok(())` means "not
/// one of the mapped semantics — pass the raw upstream response through".
///
/// - 2xx/3xx pass through.
/// - 404 on a repo-scoped path ⇒ `access_pending` (the registry re-check
///   is Plan B's job; here the agent just learns the setup flow must run).
/// - 401 ⇒ `credential_unavailable` (the machine credential stopped
///   working mid-flight).
/// - 403 + body containing "admin rights" ⇒ `permission_pending` with an
///   elevation flow row (`kind='elevation'`) opened/joined through the
///   SAME flow system the pre-flight uses — the needed role comes from the
///   GATE-010 role map (`classify_api` on method+path), and the endpoint +
///   the elevation sweep see one system.
/// - 403 + `X-Accepted-OAuth-Scopes` naming a scope the token does not
///   carry ⇒ `role_unavailable` naming that scope.
pub fn map_upstream_error(
    status: u16,
    headers: &HeaderMap,
    body: &str,
    method: &str,
    path: &str,
    state: &DbState,
) -> Result<(), AgentError> {
    if (200..400).contains(&status) {
        return Ok(());
    }
    let repo = repo_of_path(path).map(|(o, r)| format!("{o}/{r}"));
    match status {
        401 => Err(AgentError::credential_unavailable()),
        404 => match repo {
            Some(repo) => Err(AgentError::access_pending(&repo)),
            None => Ok(()),
        },
        403 => {
            if body.contains("admin rights") {
                let needed = min_role(Action::Api(classify_api(method, &format!("/{path}"))));
                // A flow can only ever verify on a repo-scoped path (the
                // flow key is the repo): on anything else NO row is
                // opened — the old `"-"` flow could never leave VERIFYING
                // and just sat until expiry. The agent still gets the
                // exact permission_pending shape (its id has no row, so
                // the elevation endpoint answers "no such flow"); the
                // deny itself is audited by the forwarding path.
                let id = match repo {
                    Some(repo) => crate::telegram::elevation::open_or_join_flow(
                        state, &repo, path, needed,
                    ),
                    None => crate::state::uuid_v4(),
                };
                Err(AgentError::permission_pending(id, role_name(needed)))
            } else if let Some(scope) = missing_scope(headers) {
                Err(AgentError::role_unavailable(&scope))
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
    }
}

/// `owner/repo` for `/repos/{owner}/{repo}...` paths (repo-scoped), else
/// `None`.
fn repo_of_path(path: &str) -> Option<(String, String)> {
    let mut segs = path.split('/');
    if segs.next() != Some("repos") {
        return None;
    }
    let owner = segs.next()?;
    let repo = segs.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

fn repo_label_of(rest: &str) -> String {
    repo_of_path(rest)
        .map(|(o, r)| format!("{o}/{r}"))
        .unwrap_or_else(|| "-".to_string())
}

/// Methods the P0 heuristic treats as needing Write: POST/PUT/PATCH/DELETE.
fn is_write_method(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

/// First scope listed in `X-Accepted-OAuth-Scopes` that the token's
/// `X-OAuth-Scopes` does not carry.
fn missing_scope(headers: &HeaderMap) -> Option<String> {
    let accepted = headers.get("x-accepted-oauth-scopes")?.to_str().ok()?;
    let granted = headers
        .get("x-oauth-scopes")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let granted: HashSet<&str> = granted
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    accepted
        .split(',')
        .map(str::trim)
        .find(|s| !s.is_empty() && !granted.contains(s))
        .map(str::to_string)
}

/// Case-sensitive substring search (the GraphQL "mutation" sniff).
fn contains_slice(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// `{api_base}/{path}` with the original query string preserved verbatim.
fn build_url(base: reqwest::Url, rest: &str, query: Option<&str>) -> reqwest::Url {
    let mut url = reqwest::Url::parse(&format!(
        "{}/{}",
        base.as_str().trim_end_matches('/'),
        rest
    ))
    .expect("api base plus a validated path forms a valid url");
    url.set_query(query);
    url
}

fn forward_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        if STRIPPED_HEADERS
            .iter()
            .any(|s| name.as_str().eq_ignore_ascii_case(s))
        {
            continue;
        }
        out.insert(name.clone(), value.clone());
    }
    out
}

/// Stable one-shot scope key for an API hold: a hash over the agent, repo,
/// method, path and body digest, so an approval can never be replayed on a
/// different operation.
fn hold_id_api(agent: &str, repo: &str, method: &str, path: &str, body_sha: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"gate-hold-api-v1");
    hasher.update(agent.as_bytes());
    hasher.update(b"|");
    hasher.update(repo.as_bytes());
    hasher.update(b"|");
    hasher.update(method.as_bytes());
    hasher.update(b"|");
    hasher.update(path.as_bytes());
    hasher.update(b"|");
    hasher.update(body_sha.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// First `SNIPPET_CAP` bytes of the body for the approval echo; a longer
/// body is truncated with an inline note, so the snippet (including the
/// note) never exceeds the cap. The full body is always covered by
/// `body_sha`.
fn body_snippet(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body).into_owned();
    if text.len() <= SNIPPET_CAP {
        return text;
    }
    let note = format!("…[truncated, {} bytes total]", text.len());
    let mut keep = SNIPPET_CAP - note.len();
    while keep > 0 && !text.is_char_boundary(keep) {
        keep -= 1;
    }
    let mut out = text[..keep].to_string();
    out.push_str(&note);
    out
}

/// SHA-256 hex digest of the FULL body.
fn body_sha(body: &[u8]) -> String {
    let digest = Sha256::digest(body);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn audit_api(audit: &Audit, agent: &str, repo: &str, decision: &str, basis: &str) {
    audit.record(AuditEntry {
        ts: crate::state::now(),
        agent: Some(agent.to_string()),
        repo: repo.to_string(),
        action: "api.request".to_string(),
        decision: decision.to_string(),
        basis: basis.to_string(),
        approver: None,
    });
}

fn deny_response() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "message": "blocked by gate policy" })),
    )
        .into_response()
}

fn bad_gateway() -> Response {
    (StatusCode::BAD_GATEWAY, "gate: upstream unreachable".to_string()).into_response()
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

fn not_configured() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "gate: api proxy not configured".to_string(),
    )
        .into_response()
}

/// Response headers forwarded back to agents: `content-type` plus the
/// GitHub headers agents depend on — pagination (`Link`), redirects
/// (`Location`), caching (`ETag`), backoff (`Retry-After`) and rate-limit
/// budget (`X-RateLimit-*`). Everything else is dropped.
const RESPONSE_HEADERS: &[&str] = &[
    "content-type",
    "link",
    "location",
    "etag",
    "retry-after",
    "x-ratelimit-limit",
    "x-ratelimit-remaining",
    "x-ratelimit-reset",
    "x-ratelimit-used",
];

/// Status + response-header allowlist + body, from upstream.
fn passthrough(status: u16, headers: &HeaderMap, body: Bytes) -> Response {
    let sc = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(sc);
    for name in RESPONSE_HEADERS {
        for value in headers.get_all(*name) {
            builder = builder.header(*name, value.clone());
        }
    }
    builder
        .body(Body::from(body))
        .unwrap_or_else(|_| bad_gateway())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header;

    fn state() -> DbState {
        DbState::open(":memory:").unwrap()
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            let name = header::HeaderName::from_lowercase(k.to_lowercase().as_bytes())
                .expect("lowercase header name");
            map.insert(name, header::HeaderValue::from_str(v).unwrap());
        }
        map
    }

    // -- map_upstream_error -------------------------------------------------

    #[test]
    fn success_statuses_pass_through() {
        let s = state();
        for status in [200, 201, 204, 301, 302, 399] {
            assert_eq!(
                map_upstream_error(status, &headers(&[]), "", "GET", "repos/o/r", &s),
                Ok(()),
                "{status}"
            );
        }
    }

    #[test]
    fn repo_scoped_404_is_access_pending_with_repo() {
        let s = state();
        let e = map_upstream_error(
            404,
            &headers(&[]),
            r#"{"message":"Not Found"}"#,
            "GET",
            "repos/octo/hello/issues/1",
            &s,
        )
        .unwrap_err();
        assert_eq!(e.kind(), "access_pending");
        let AgentError::AccessPending { repo, .. } = e else {
            panic!("variant");
        };
        assert_eq!(repo, "octo/hello");
    }

    #[test]
    fn non_repo_404_passes_through() {
        let s = state();
        assert_eq!(
            map_upstream_error(404, &headers(&[]), r#"{"message":"Not Found"}"#, "GET", "user", &s),
            Ok(())
        );
    }

    #[test]
    fn upstream_401_is_credential_unavailable() {
        let s = state();
        let e = map_upstream_error(
            401,
            &headers(&[]),
            r#"{"message":"Bad credentials"}"#,
            "GET",
            "repos/octo/hello",
            &s,
        )
        .unwrap_err();
        assert_eq!(e.kind(), "credential_unavailable");
    }

    #[test]
    fn admin_rights_403_is_permission_pending_and_writes_flow_row() {
        let s = state();
        let e = map_upstream_error(
            403,
            &headers(&[]),
            r#"{"message":"Must have admin rights to Repository."}"#,
            "POST",
            "repos/octo/hello/hooks",
            &s,
        )
        .unwrap_err();
        assert_eq!(e.kind(), "permission_pending");
        let AgentError::PermissionPending {
            elevation_id,
            needs_role,
            ..
        } = e
        else {
            panic!("variant");
        };
        assert_eq!(needs_role, "admin");
        // uuid v4 shape, and a DETECTED flow row backs it.
        assert_eq!(elevation_id.len(), 36);
        assert_eq!(elevation_id.as_bytes()[14], b'4');
        assert_eq!(s.flow_state(&elevation_id).unwrap().as_deref(), Some("DETECTED"));
        let (kind, _): (String, String) = s
            .conn()
            .query_row(
                "SELECT kind, payload_json FROM flows WHERE id = ?1",
                [&elevation_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, "elevation");
    }

    #[test]
    fn scope_gap_403_is_role_unavailable_naming_the_scope() {
        let s = state();
        let hs = headers(&[
            ("x-accepted-oauth-scopes", "repo, admin:org"),
            ("x-oauth-scopes", "public_repo, read:org"),
        ]);
        let e = map_upstream_error(403, &hs, r#"{"message":"Forbidden"}"#, "POST", "repos/octo/hello", &s)
            .unwrap_err();
        assert_eq!(e.kind(), "role_unavailable");
        let AgentError::RoleUnavailable { message } = e else {
            panic!("variant");
        };
        assert!(message.contains("'repo'"), "{message}");
    }

    #[test]
    fn unmapped_403_and_5xx_pass_through() {
        let s = state();
        assert_eq!(
            map_upstream_error(403, &headers(&[]), r#"{"message":"Forbidden"}"#, "POST", "repos/o/r", &s),
            Ok(())
        );
        assert_eq!(
            map_upstream_error(500, &headers(&[]), "boom", "POST", "repos/o/r", &s),
            Ok(())
        );
    }

    // -- helpers ------------------------------------------------------------

    #[test]
    fn repo_of_path_scopes_and_rejects() {
        assert_eq!(
            repo_of_path("repos/octo/hello/issues"),
            Some(("octo".to_string(), "hello".to_string()))
        );
        assert_eq!(repo_of_path("repos/octo"), None);
        assert_eq!(repo_of_path("repos//hello"), None);
        assert_eq!(repo_of_path("user"), None);
        assert_eq!(repo_of_path("graphql"), None);
    }

    #[test]
    fn write_methods_are_the_mutation_verbs() {
        assert!(is_write_method(&Method::POST));
        assert!(is_write_method(&Method::PUT));
        assert!(is_write_method(&Method::PATCH));
        assert!(is_write_method(&Method::DELETE));
        assert!(!is_write_method(&Method::GET));
        assert!(!is_write_method(&Method::HEAD));
    }

    #[test]
    fn missing_scope_finds_first_ungranted() {
        let hs = headers(&[
            ("X-Accepted-OAuth-Scopes", "repo, admin:org"),
            ("X-OAuth-Scopes", "repo"),
        ]);
        assert_eq!(missing_scope(&hs).as_deref(), Some("admin:org"));
        let hs = headers(&[
            ("X-Accepted-OAuth-Scopes", "repo"),
            ("X-OAuth-Scopes", "repo, gist"),
        ]);
        assert_eq!(missing_scope(&hs), None);
        // No accepted header ⇒ not applicable.
        assert_eq!(missing_scope(&headers(&[])), None);
        // Accepted-but-empty ⇒ not applicable.
        let hs = headers(&[("X-Accepted-OAuth-Scopes", "")]);
        assert_eq!(missing_scope(&hs), None);
    }

    #[test]
    fn contains_slice_is_case_sensitive() {
        assert!(contains_slice(b"query \"mutation {", b"mutation"));
        assert!(!contains_slice(b"query \"MUTATION {", b"mutation"));
        assert!(!contains_slice(b"", b"mutation"));
    }

    #[test]
    fn build_url_appends_path_and_query() {
        let base = reqwest::Url::parse("http://up.internal/").unwrap();
        let url = build_url(base, "repos/octo/hello/issues", Some("state=open"));
        assert_eq!(
            url.as_str(),
            "http://up.internal/repos/octo/hello/issues?state=open"
        );
        // Percent-encoding in the path survives verbatim.
        let base = reqwest::Url::parse("http://up.internal/").unwrap();
        let url = build_url(base, "repos/octo/hello/contents/a%2Fb", None);
        assert_eq!(
            url.as_str(),
            "http://up.internal/repos/octo/hello/contents/a%2Fb"
        );
    }

    #[test]
    fn forward_headers_strip_hop_by_hop_and_gate_credentials() {
        let hs = headers(&[
            ("content-type", "application/json"),
            ("x-custom", "kept"),
            ("connection", "close"),
            ("transfer-encoding", "chunked"),
            ("authorization", "Basic Z2F0ZTp0b2tlbg=="),
            ("host", "gate.internal"),
            ("content-length", "17"),
        ]);
        let out = forward_headers(&hs);
        assert_eq!(out.get("content-type").unwrap(), "application/json");
        assert_eq!(out.get("x-custom").unwrap(), "kept");
        assert!(out.get("connection").is_none());
        assert!(out.get("transfer-encoding").is_none());
        assert!(out.get("authorization").is_none(), "agent credentials never ride along");
        assert!(out.get("host").is_none());
        assert!(out.get("content-length").is_none());
    }

    #[test]
    fn hold_id_api_scopes_every_axis_deterministically() {
        let a = hold_id_api("agent", "o/r", "POST", "repos/o/r/issues", "sha");
        let b = hold_id_api("agent", "o/r", "POST", "repos/o/r/issues", "sha");
        assert_eq!(a, b, "same op => same one-shot id");
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(
            a,
            hold_id_api("other", "o/r", "POST", "repos/o/r/issues", "sha")
        );
        assert_ne!(a, hold_id_api("agent", "o/x", "POST", "repos/o/r/issues", "sha"));
        assert_ne!(a, hold_id_api("agent", "o/r", "PUT", "repos/o/r/issues", "sha"));
        assert_ne!(
            a,
            hold_id_api("agent", "o/r", "POST", "repos/o/r/labels", "sha")
        );
        assert_ne!(a, hold_id_api("agent", "o/r", "POST", "repos/o/r/issues", "sha2"));
    }

    #[test]
    fn body_snippet_short_bodies_pass_through() {
        assert_eq!(body_snippet(br#"{"title":"x"}"#), r#"{"title":"x"}"#);
        // Just under the cap stays whole.
        let body = vec![b'x'; SNIPPET_CAP];
        assert_eq!(body_snippet(&body).len(), SNIPPET_CAP);
    }

    #[test]
    fn body_snippet_caps_over_4kib_with_note() {
        let body = vec![b'a'; 5120]; // 5 KiB
        let snippet = body_snippet(&body);
        assert!(snippet.len() <= SNIPPET_CAP, "{} bytes", snippet.len());
        assert!(snippet.contains("truncated"), "note present: {snippet}");
        assert!(snippet.contains("5120"), "note names the full size: {snippet}");
    }

    #[test]
    fn body_sha_is_sha256_hex_of_full_body() {
        let body = vec![b'a'; 5120];
        let mut hasher = Sha256::new();
        hasher.update(&body);
        let expected: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(body_sha(&body), expected);
        assert_eq!(body_sha(&body).len(), 64);
    }

    #[test]
    fn uuid_v4_is_rfc4122_shaped() {
        for _ in 0..8 {
            let id = crate::state::uuid_v4();
            assert_eq!(id.len(), 36, "{id}");
            let parts: Vec<&str> = id.split('-').collect();
            assert_eq!(
                parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
                vec![8, 4, 4, 4, 12]
            );
            assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
            assert!(id.starts_with(&parts[0][..]));
            assert_eq!(parts[2].chars().next().unwrap(), '4', "version nibble: {id}");
            let variant = u8::from_str_radix(&parts[3][..1], 16).unwrap();
            assert!((8..=11).contains(&variant), "variant bits: {id}");
        }
        assert_ne!(crate::state::uuid_v4(), crate::state::uuid_v4());
    }
}
