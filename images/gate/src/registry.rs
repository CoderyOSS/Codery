//! Access registry maintenance (GATE-010): keeps the gate's knowledge of
//! the machine account's roles current, and tells the operator — once —
//! when a role drops.
//!
//! - [`refresh`] re-reads one repo's permissions via
//!   `Platform::repo_permissions` and reconciles the registry row:
//!   - **unchanged** ⇒ silent `last_verified_at` stamp;
//!   - **role dropped** (to a lower tier, including None) ⇒ row updated +
//!     exactly ONE Telegram notice (`"rob-agent now has None on acme/api"`)
//!     — informational only, never a fix request (GATE-010: the user makes
//!     every access decision on GitHub);
//!   - **role rose** ⇒ row updated silently (gains need no noise);
//!   - **404/NoAccess** ⇒ the row is KEPT with `granted_role = none`
//!     (deletion was considered and rejected: a deleted row loses
//!     `intended_role`, while a none-row lets `/status` show "access
//!     removed"). A NoAccess for a repo with NO prior row creates a
//!     none-row WITHOUT a notice — nothing dropped, it never had access;
//!   - **upstream failure** ⇒ row left untouched (a stale verified role
//!     beats a wrong fresh one; `last_verified_at` means "verified", so it
//!     is not stamped on failure).
//!
//! - [`sweep_tick`] / [`run_registry_sweep`]: every registry row is
//!   re-checked every 30 minutes ([`REGISTRY_SWEEP_INTERVAL`]; override
//!   `GATE_REGISTRY_SWEEP_SECS` for tests) — the self-healing loop that
//!   notices role changes made on GitHub without the gate watching.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;

use crate::github::{Platform, RepoAccessError};
use crate::rolemapping::{role_from_name, role_name};
use crate::state::{State as DbState, RegistryRow};
use crate::telegram::send::SendRequest;
use crate::telegram::TelegramApi;
use crate::types::Role;

/// 30 minutes: the registry re-check cadence (GATE-010). Overridable with
/// `GATE_REGISTRY_SWEEP_SECS` (see `config::sweep_secs_from`) so tests can
/// shrink it.
pub const REGISTRY_SWEEP_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Everything a registry refresh reads: the shared state, the platform to
/// ask, the machine login notices name, and the optional Telegram channel
/// notices ride on (`None` ⇒ checks still run, notices are skipped).
#[derive(Clone)]
pub struct RegistryDeps {
    pub state: Arc<Mutex<DbState>>,
    pub platform: Arc<dyn Platform>,
    /// The machine account login, as named in drop notices.
    pub machine_login: String,
    /// `(api, chat_id)` for gate-initiated notices; `None` = no channel.
    pub notify: Option<(Arc<dyn TelegramApi>, i64)>,
}

/// What one [`refresh`] decided; `UpstreamFailed` is the only outcome that
/// leaves the stored row untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// Upstream role equals the stored one (or the row was created at this
    /// role on first sight): registry current, no notice.
    Verified(Role),
    /// Stored role dropped to a lower tier (including None): registry
    /// updated, one notice sent/attempted.
    Dropped { from: Role, to: Role },
    /// Stored role rose: registry updated silently.
    Gained { from: Role, to: Role },
    /// Upstream could not answer (transport failure or non-200/404): the
    /// stored row is left as it was.
    UpstreamFailed,
}

/// Re-read one repo's permissions and reconcile its registry row. `repo`
/// is the registry key `owner/repo`. The drop notice fires AFTER the row
/// is written, so the registry stays consistent even if the send fails.
pub async fn refresh(deps: &RegistryDeps, repo: &str) -> RefreshOutcome {
    let Some((owner, name)) = repo.split_once('/') else {
        eprintln!("registry: malformed repo key '{repo}' — refresh skipped");
        return RefreshOutcome::UpstreamFailed;
    };
    let new_role = match deps.platform.repo_permissions(owner, name).await {
        Ok(access) => access.role,
        // 404 ⇒ the repo is invisible to the machine account ⇒ no access
        // (GATE-008 fail closed; also GitHub's "removed our access" shape).
        Err(RepoAccessError::NoAccess) => Role::None,
        Err(RepoAccessError::Upstream(status)) => {
            eprintln!("registry: refresh of {repo} failed (upstream {status}) — row left stale");
            return RefreshOutcome::UpstreamFailed;
        }
    };
    let prev = {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        guard.registry_get(repo).ok().flatten()
    };
    // Every branch below upserts, which stamps last_verified_at = now; the
    // unchanged branch passes the stored fields straight through, so only
    // the timestamp moves.
    let outcome = {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        match &prev {
            None => {
                // First sight: record what the account holds now. A
                // NoAccess-first-sight becomes a none-row WITHOUT a notice
                // (nothing dropped — it never had access).
                let granted = role_name(new_role);
                let _ = guard.registry_upsert(repo, granted, granted, "unknown");
                RefreshOutcome::Verified(new_role)
            }
            Some(row) => {
                let old = stored_role(row);
                if old == new_role {
                    let _ = guard.registry_upsert(
                        repo,
                        &row.granted_role,
                        &row.intended_role,
                        &row.via,
                    );
                    RefreshOutcome::Verified(new_role)
                } else if new_role < old {
                    let _ = guard.registry_upsert(
                        repo,
                        role_name(new_role),
                        &row.intended_role,
                        &row.via,
                    );
                    RefreshOutcome::Dropped {
                        from: old,
                        to: new_role,
                    }
                } else {
                    let _ = guard.registry_upsert(
                        repo,
                        role_name(new_role),
                        &row.intended_role,
                        &row.via,
                    );
                    RefreshOutcome::Gained {
                        from: old,
                        to: new_role,
                    }
                }
            }
        }
    };
    if let RefreshOutcome::Dropped { to, .. } = outcome {
        let _ = crate::tasks::send_notice(
            &deps.notify,
            SendRequest::text(drop_notice_text(&deps.machine_login, to, repo)),
        )
        .await;
    }
    outcome
}

/// One sweep pass: refresh every registry row. Rows are read once up
/// front, so rows added mid-sweep wait for the next tick; one failing
/// repo never blocks the rest. Returns how many rows were reconciled
/// (upstream failures excluded).
pub async fn sweep_tick(deps: &RegistryDeps) -> usize {
    let repos: Vec<String> = {
        let guard = deps.state.lock().expect("state lock must be poison-free");
        match guard.registry_all() {
            Ok(rows) => rows.into_iter().map(|r| r.repo).collect(),
            Err(e) => {
                eprintln!("registry: sweep could not list rows: {e}");
                return 0;
            }
        }
    };
    let mut reconciled = 0;
    for repo in &repos {
        if !matches!(refresh(deps, repo).await, RefreshOutcome::UpstreamFailed) {
            reconciled += 1;
        }
    }
    reconciled
}

/// The 30-minute registry sweep loop. First tick is one full interval
/// after startup (fresh state needs no immediate re-check). Exits on
/// shutdown.
pub async fn run_registry_sweep(
    deps: RegistryDeps,
    mut shutdown: watch::Receiver<bool>,
    interval: Duration,
) {
    shutdown.borrow_and_update();
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep(interval) => {
                let reconciled = sweep_tick(&deps).await;
                if reconciled > 0 {
                    eprintln!("registry: sweep re-verified {reconciled} repo(s)");
                }
            }
        }
    }
}

/// Role word for human text ("None", "Write", …) — [`Role`]'s Display.
/// (Storage names stay lowercase via `rolemapping::role_name`.)
fn drop_notice_text(login: &str, to: Role, repo: &str) -> String {
    format!("{login} now has {to} on {repo}")
}

/// The stored role of a row as a [`Role`]; an unparsable stored value
/// reads as `Role::None` (fail conservative: any real role then counts as
/// a silent gain, never as a spurious drop notice).
fn stored_role(row: &RegistryRow) -> Role {
    role_from_name(&row.granted_role).unwrap_or(Role::None)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use reqwest::Url;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::github::RepoAccess;
    use crate::telegram::fake::FakeTelegram;

    const LOGIN: &str = "rob-agent";
    const REPO: &str = "acme/api";
    const CHAT: i64 = 4242;

    // -- fake platform: scripted per-repo results + call counting ----------

    #[derive(Default)]
    struct FakePlatform {
        perms: Mutex<HashMap<String, Result<RepoAccess, RepoAccessError>>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakePlatform {
        fn with(repo: &str, result: Result<RepoAccess, RepoAccessError>) -> Arc<Self> {
            let fake = Self::default();
            fake.perms
                .lock()
                .unwrap()
                .insert(repo.to_string(), result);
            Arc::new(fake)
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }

        fn called_repos(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    fn access(role: Role) -> Result<RepoAccess, RepoAccessError> {
        Ok(RepoAccess {
            role,
            private: false,
        })
    }

    #[async_trait]
    impl Platform for FakePlatform {
        async fn repo_permissions(
            &self,
            owner: &str,
            repo: &str,
        ) -> Result<RepoAccess, RepoAccessError> {
            let key = format!("{owner}/{repo}");
            self.calls.lock().unwrap().push(key.clone());
            self.perms
                .lock()
                .unwrap()
                .get(&key)
                .cloned()
                .unwrap_or(Err(RepoAccessError::NoAccess))
        }

        async fn ancestry(
            &self,
            _: &str,
            _: &str,
            _: &crate::pktline::Oid,
            _: &crate::pktline::Oid,
        ) -> crate::types::Ancestry {
            unimplemented!("not used by registry tests")
        }

        fn git_upstream_url(&self, _: &str, _: &str) -> Url {
            unimplemented!("not used by registry tests")
        }

        fn api_base(&self) -> Url {
            unimplemented!("not used by registry tests")
        }

        async fn commits_between(
            &self,
            _: &str,
            _: &str,
            _: &crate::pktline::Oid,
            _: &crate::pktline::Oid,
        ) -> Vec<String> {
            unimplemented!("not used by registry tests")
        }

        async fn org_member(&self, _: &str) -> bool {
            unimplemented!("not used by registry tests")
        }

        async fn user_login(&self) -> Option<String> {
            unimplemented!("not used by registry tests")
        }

        async fn verify_token(
            &self,
            _: &str,
            _: &str,
        ) -> Result<crate::github::VerifiedToken, crate::github::VerifyError> {
            unimplemented!("not used by registry tests")
        }

        async fn user_exists(&self, _: &str) -> bool {
            unimplemented!("not used by registry tests")
        }

        async fn repository_invitations(
            &self,
        ) -> Result<Vec<crate::github::Invitation>, crate::github::InviteError> {
            unimplemented!("not used by registry tests")
        }

        async fn accept_invitation(&self, _: i64) -> Result<(), crate::github::InviteError> {
            unimplemented!("not used by registry tests")
        }

        async fn org_plan_note(&self, _: &str) -> Option<String> {
            unimplemented!("not used by registry tests")
        }
    }

    // -- harness -------------------------------------------------------------

    fn deps_for(
        state: Arc<Mutex<DbState>>,
        platform: Arc<dyn Platform>,
        notify: Option<&Arc<FakeTelegram>>,
    ) -> RegistryDeps {
        RegistryDeps {
            state,
            platform,
            machine_login: LOGIN.to_string(),
            notify: notify.map(|fake| {
                let api: Arc<dyn TelegramApi> = fake.clone();
                (api, CHAT)
            }),
        }
    }

    fn seeded_state(repo: &str, granted: &str) -> Arc<Mutex<DbState>> {
        let state = Arc::new(Mutex::new(DbState::open(":memory:").expect("open")));
        state
            .lock()
            .unwrap()
            .registry_upsert(repo, granted, granted, "collaborator")
            .unwrap();
        state
    }

    fn row_of(state: &Arc<Mutex<DbState>>, repo: &str) -> RegistryRow {
        state
            .lock()
            .unwrap()
            .registry_get(repo)
            .expect("registry read")
            .expect("row present")
    }

    fn backdate(state: &Arc<Mutex<DbState>>, repo: &str, ts: i64) {
        state
            .lock()
            .unwrap()
            .conn()
            .execute(
                "UPDATE registry SET last_verified_at = ?1 WHERE repo = ?2",
                rusqlite::params![ts, repo],
            )
            .unwrap();
    }

    async fn wiremock_permissions(server: &MockServer, status: u16, body: &str) {
        Mock::given(method("GET"))
            .and(path("/repos/acme/api"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(server)
            .await;
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
            r#"{{"private":false,"permissions":{{"admin":{admin},"maintain":{maintain},
               "push":{push},"triage":{triage},"pull":{pull}}}}}"#
        )
        .replace('\n', "")
    }

    async fn github_platform_at(server: &MockServer) -> Arc<dyn Platform> {
        Arc::new(crate::github::GitHubPlatform::with_bases(
            Url::parse(&server.uri()).expect("mock api base"),
            Url::parse("http://git.example.internal/").expect("mock git base"),
            Arc::new(|| None),
        ))
    }

    // -- refresh: the GATE-010 reconcile rules --------------------------------

    #[tokio::test]
    async fn refresh_write_to_none_updates_registry_and_sends_exactly_one_notice() {
        let server = MockServer::start().await;
        wiremock_permissions(&server, 200, &permissions_body(Role::None)).await;
        let platform = github_platform_at(&server).await;
        let state = seeded_state(REPO, "write");
        let fake = Arc::new(FakeTelegram::new());
        let deps = deps_for(state.clone(), platform, Some(&fake));

        let outcome = refresh(&deps, REPO).await;
        assert_eq!(
            outcome,
            RefreshOutcome::Dropped {
                from: Role::Write,
                to: Role::None
            }
        );
        let row = row_of(&state, REPO);
        assert_eq!(row.granted_role, "none", "the drop lands in the registry");
        assert_eq!(row.intended_role, "write", "intended_role survives the drop");
        assert_eq!(row.via, "collaborator", "via survives the drop");
        assert_eq!(fake.sent().len(), 1, "exactly ONE notice for one drop");
        assert_eq!(
            fake.sent()[0].text,
            format!("{LOGIN} now has None on {REPO}")
        );
    }

    #[tokio::test]
    async fn refresh_unchanged_role_is_silent_but_stamps_last_verified() {
        let server = MockServer::start().await;
        wiremock_permissions(&server, 200, &permissions_body(Role::Write)).await;
        let platform = github_platform_at(&server).await;
        let state = seeded_state(REPO, "write");
        backdate(&state, REPO, 1000);
        let fake = Arc::new(FakeTelegram::new());
        let deps = deps_for(state.clone(), platform, Some(&fake));

        let outcome = refresh(&deps, REPO).await;
        assert_eq!(outcome, RefreshOutcome::Verified(Role::Write));
        assert!(fake.sent().is_empty(), "unchanged ⇒ NO notice");
        let row = row_of(&state, REPO);
        assert_eq!(row.granted_role, "write");
        assert!(
            row.last_verified_at > 1000,
            "silent refresh still stamps last_verified_at (got {})",
            row.last_verified_at
        );
    }

    #[tokio::test]
    async fn refresh_no_access_without_prior_row_creates_none_row_without_notice() {
        let server = MockServer::start().await;
        wiremock_permissions(&server, 404, r#"{"message":"Not Found"}"#).await;
        let platform = github_platform_at(&server).await;
        let state = Arc::new(Mutex::new(DbState::open(":memory:").expect("open")));
        let fake = Arc::new(FakeTelegram::new());
        let deps = deps_for(state.clone(), platform, Some(&fake));

        let outcome = refresh(&deps, REPO).await;
        assert_eq!(
            outcome,
            RefreshOutcome::Verified(Role::None),
            "first sight of an inaccessible repo is NOT a drop"
        );
        let row = row_of(&state, REPO);
        assert_eq!(row.granted_role, "none");
        assert!(fake.sent().is_empty(), "nothing dropped ⇒ nothing to announce");
    }

    #[tokio::test]
    async fn refresh_no_access_with_prior_row_drops_with_one_notice() {
        let server = MockServer::start().await;
        wiremock_permissions(&server, 404, r#"{"message":"Not Found"}"#).await;
        let platform = github_platform_at(&server).await;
        let state = seeded_state(REPO, "write");
        let fake = Arc::new(FakeTelegram::new());
        let deps = deps_for(state.clone(), platform, Some(&fake));

        let outcome = refresh(&deps, REPO).await;
        assert_eq!(
            outcome,
            RefreshOutcome::Dropped {
                from: Role::Write,
                to: Role::None
            }
        );
        assert_eq!(row_of(&state, REPO).granted_role, "none");
        assert_eq!(fake.sent().len(), 1);
    }

    #[tokio::test]
    async fn refresh_partial_drop_to_lower_role_updates_and_notifies_once() {
        let server = MockServer::start().await;
        wiremock_permissions(&server, 200, &permissions_body(Role::Triage)).await;
        let platform = github_platform_at(&server).await;
        let state = seeded_state(REPO, "maintain");
        let fake = Arc::new(FakeTelegram::new());
        let deps = deps_for(state.clone(), platform, Some(&fake));

        let outcome = refresh(&deps, REPO).await;
        assert_eq!(
            outcome,
            RefreshOutcome::Dropped {
                from: Role::Maintain,
                to: Role::Triage
            },
            "role lower than before but non-none is still a drop"
        );
        assert_eq!(row_of(&state, REPO).granted_role, "triage");
        assert_eq!(fake.sent().len(), 1);
        assert_eq!(fake.sent()[0].text, format!("{LOGIN} now has Triage on {REPO}"));
    }

    #[tokio::test]
    async fn refresh_role_gain_updates_silently() {
        let server = MockServer::start().await;
        wiremock_permissions(&server, 200, &permissions_body(Role::Write)).await;
        let platform = github_platform_at(&server).await;
        let state = seeded_state(REPO, "read");
        let fake = Arc::new(FakeTelegram::new());
        let deps = deps_for(state.clone(), platform, Some(&fake));

        let outcome = refresh(&deps, REPO).await;
        assert_eq!(
            outcome,
            RefreshOutcome::Gained {
                from: Role::Read,
                to: Role::Write
            }
        );
        assert_eq!(row_of(&state, REPO).granted_role, "write");
        assert!(fake.sent().is_empty(), "gains are silent");
    }

    #[tokio::test]
    async fn refresh_upstream_failure_leaves_the_row_stale_without_notice() {
        let server = MockServer::start().await;
        wiremock_permissions(&server, 503, r#"{"message":"unavailable"}"#).await;
        let platform = github_platform_at(&server).await;
        let state = seeded_state(REPO, "write");
        backdate(&state, REPO, 1000);
        let fake = Arc::new(FakeTelegram::new());
        let deps = deps_for(state.clone(), platform, Some(&fake));

        let outcome = refresh(&deps, REPO).await;
        assert_eq!(outcome, RefreshOutcome::UpstreamFailed);
        let row = row_of(&state, REPO);
        assert_eq!(row.granted_role, "write", "stale beats wrong");
        assert_eq!(row.last_verified_at, 1000, "a failed refresh is NOT a verification");
        assert!(fake.sent().is_empty());
    }

    #[tokio::test]
    async fn refresh_without_a_notify_channel_still_updates_the_registry() {
        let server = MockServer::start().await;
        wiremock_permissions(&server, 404, r#"{"message":"Not Found"}"#).await;
        let platform = github_platform_at(&server).await;
        let state = seeded_state(REPO, "write");
        let deps = deps_for(state.clone(), platform, None);

        let outcome = refresh(&deps, REPO).await;
        assert_eq!(
            outcome,
            RefreshOutcome::Dropped {
                from: Role::Write,
                to: Role::None
            },
            "no channel ⇒ checks and writes still happen, only the notice is skipped"
        );
        assert_eq!(row_of(&state, REPO).granted_role, "none");
    }

    // -- sweep ----------------------------------------------------------------

    #[tokio::test]
    async fn sweep_tick_refreshes_every_registry_row_and_counts_calls() {
        let state = Arc::new(Mutex::new(DbState::open(":memory:").expect("open")));
        {
            let guard = state.lock().unwrap();
            guard
                .registry_upsert("acme/api", "write", "write", "collaborator")
                .unwrap();
            guard
                .registry_upsert("acme/other", "read", "read", "org_team")
                .unwrap();
        }
        let mut perms = HashMap::new();
        perms.insert("acme/api".to_string(), access(Role::Write));
        perms.insert("acme/other".to_string(), access(Role::Read));
        let fake = Arc::new(FakePlatform::default());
        *fake.perms.lock().unwrap() = perms;
        let platform: Arc<dyn Platform> = fake.clone();
        let tg = Arc::new(FakeTelegram::new());
        let deps = deps_for(state.clone(), platform, Some(&tg));

        let reconciled = sweep_tick(&deps).await;
        assert_eq!(reconciled, 2, "every row gets one refresh");
        let mut called = fake.called_repos();
        called.sort();
        assert_eq!(called, vec!["acme/api".to_string(), "acme/other".to_string()]);
        assert!(tg.sent().is_empty(), "nothing changed ⇒ no notices");
    }

    #[tokio::test]
    async fn sweep_tick_survives_an_upstream_failure_and_still_checks_the_rest() {
        let state = Arc::new(Mutex::new(DbState::open(":memory:").expect("open")));
        {
            let guard = state.lock().unwrap();
            guard
                .registry_upsert("acme/api", "write", "write", "collaborator")
                .unwrap();
            guard
                .registry_upsert("acme/down", "write", "write", "collaborator")
                .unwrap();
        }
        let mut perms = HashMap::new();
        perms.insert(
            "acme/api".to_string(),
            Err(RepoAccessError::Upstream(503)),
        );
        perms.insert("acme/down".to_string(), access(Role::None));
        let fake = Arc::new(FakePlatform::default());
        *fake.perms.lock().unwrap() = perms;
        let platform: Arc<dyn Platform> = fake.clone();
        let tg = Arc::new(FakeTelegram::new());
        let deps = deps_for(state.clone(), platform, Some(&tg));

        let reconciled = sweep_tick(&deps).await;
        assert_eq!(reconciled, 1, "the 503 repo is excluded, the other still lands");
        assert_eq!(fake.call_count(), 2, "both repos were attempted");
        assert_eq!(
            row_of(&state, "acme/down").granted_role,
            "none",
            "the drop on the reachable repo still lands"
        );
        assert_eq!(tg.sent().len(), 1);
    }

    #[tokio::test]
    async fn sweep_loop_ticks_until_shutdown() {
        let state = Arc::new(Mutex::new(DbState::open(":memory:").expect("open")));
        state
            .lock()
            .unwrap()
            .registry_upsert("acme/api", "write", "write", "collaborator")
            .unwrap();
        let fake = FakePlatform::with("acme/api", access(Role::Write));
        let platform: Arc<dyn Platform> = fake.clone();
        let tg = Arc::new(FakeTelegram::new());
        let deps = deps_for(state.clone(), platform, Some(&tg));

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run_registry_sweep(
            deps,
            shutdown_rx,
            Duration::from_millis(5),
        ));
        // Deterministic-enough wait: the 5 ms interval must produce ticks.
        let mut waited = 0;
        while fake.call_count() < 1 && waited < 600 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            waited += 1;
        }
        assert!(fake.call_count() >= 1, "the loop ticked at least once");
        shutdown_tx.send_replace(true);
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
    }
}
