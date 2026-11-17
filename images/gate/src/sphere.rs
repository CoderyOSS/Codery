//! Own-sphere resolution (GATE-010): a repo is inside the gate's own
//! sphere when the registry holds a grant for it (verified via the setup
//! flow) or — failing that — the machine account is an active member of
//! the owning org. Every failure mode resolves to `false` (fail closed):
//! a registry DB error or an unreachable/unclear membership answer both
//! mean "treat as third-party", which routes the decision through the
//! approval backend instead of trusting policy defaults.

use std::sync::{Arc, Mutex};

use crate::github::Platform;
use crate::state::State as DbState;

pub struct SphereResolver {
    registry: Arc<Mutex<DbState>>,
    platform: Arc<dyn Platform>,
}

impl SphereResolver {
    pub fn new(registry: Arc<Mutex<DbState>>, platform: Arc<dyn Platform>) -> Self {
        Self { registry, platform }
    }

    /// Registry hit ⇒ true; otherwise the machine account's org membership
    /// on `owner` decides. Registry DB errors fail closed to `false`.
    pub async fn is_own_sphere(&self, owner: &str, repo: &str) -> bool {
        let full = format!("{owner}/{repo}");
        let row = {
            let guard = self
                .registry
                .lock()
                .expect("registry lock must be poison-free");
            guard.registry_get(&full)
        };
        match row {
            Ok(Some(_)) => true,
            Ok(None) => self.platform.org_member(owner).await,
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::github::GitHubPlatform;

    fn resolver_at(
        registry: Arc<Mutex<DbState>>,
        server: &MockServer,
    ) -> SphereResolver {
        let platform: Arc<dyn Platform> = Arc::new(GitHubPlatform::with_bases(
            reqwest::Url::parse(&server.uri()).unwrap(),
            reqwest::Url::parse("http://git.example.internal/").unwrap(),
            Arc::new(|| None),
        ));
        SphereResolver::new(registry, platform)
    }

    fn empty_registry() -> Arc<Mutex<DbState>> {
        Arc::new(Mutex::new(DbState::open(":memory:").unwrap()))
    }

    #[tokio::test]
    async fn registry_hit_is_own_sphere_without_membership_check() {
        let server = MockServer::start().await;
        // No membership mock: if the resolver consulted the platform it
        // would get wiremock's default 404 (⇒ false). The registry grant
        // alone must decide.
        let registry = empty_registry();
        registry
            .lock()
            .unwrap()
            .registry_upsert("acme/widget", "admin", "admin", "device-flow")
            .unwrap();
        let r = resolver_at(registry, &server);
        assert!(r.is_own_sphere("acme", "widget").await);
    }

    #[tokio::test]
    async fn active_membership_is_own_sphere() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/memberships/orgs/acme"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"state":"active"}"#),
            )
            .mount(&server)
            .await;
        let r = resolver_at(empty_registry(), &server);
        assert!(r.is_own_sphere("acme", "widget").await);
    }

    #[tokio::test]
    async fn membership_404_is_not_own_sphere() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/memberships/orgs/acme"))
            .respond_with(
                ResponseTemplate::new(404).set_body_string(r#"{"message":"Not Found"}"#),
            )
            .mount(&server)
            .await;
        let r = resolver_at(empty_registry(), &server);
        assert!(!r.is_own_sphere("acme", "widget").await);
    }

    #[tokio::test]
    async fn pending_membership_is_not_own_sphere() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/memberships/orgs/acme"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"state":"pending"}"#),
            )
            .mount(&server)
            .await;
        let r = resolver_at(empty_registry(), &server);
        assert!(!r.is_own_sphere("acme", "widget").await);
    }

    #[tokio::test]
    async fn registry_db_error_fails_closed_to_false() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/memberships/orgs/acme"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"state":"active"}"#),
            )
            .mount(&server)
            .await;
        // Break the registry table so the lookup errors: the resolver must
        // return false even though membership would have said active.
        let registry = empty_registry();
        registry
            .lock()
            .unwrap()
            .conn()
            .execute("DROP TABLE registry", [])
            .unwrap();
        let r = resolver_at(registry, &server);
        assert!(!r.is_own_sphere("acme", "widget").await);
    }
}
