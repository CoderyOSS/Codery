// Paths on the VPS host
pub const STATE_DIR: &str = "/opt/codery/state";
pub const CADDY_CONFIG: &str = "/etc/caddy/Caddyfile";
/// Default routes_file path used by services/apps.yml. Referenced by YAML, not Rust.
#[allow(dead_code)]
pub const APPS_ROUTES: &str = "/opt/codery/proxy/apps-routes.json";
/// Default sandbox extra-routes path. Referenced by YAML, not Rust.
#[allow(dead_code)]
pub const SANDBOX_ROUTES: &str = "/opt/codery/proxy/sandbox-routes.json";
pub const ENV_FILE: &str = "/opt/codery/.env";
pub const TAILSCALE_IP_FILE: &str = "/run/tailscale.ip";
/// Shared projects directory. Declared in service YAMLs; kept here for documentation.
#[allow(dead_code)]
pub const PROJECTS_DIR: &str = "/opt/codery/projects";
pub const REGISTRY: &str = "ghcr.io/coderyoss/codery";
/// Default Docker network. Declared in service YAMLs; kept here for documentation.
#[allow(dead_code)]
pub const NETWORK: &str = "codery-net";

/// Directory where service YAML definitions are stored on the VPS host.
/// In the repo: services/*.yml — synced here by CI before each deploy.
pub const SERVICES_DIR: &str = "/opt/codery/services";

// Supervisor
pub const SUPERVISORD_CONF: &str = "/etc/supervisor/supervisord.conf";

// GHCR authentication (stored in ENV_FILE)
pub const GHCR_HOST: &str = "ghcr.io";

// Caddy admin API (default port)
pub const CADDY_ADMIN_PORT: u16 = 2019;

pub const MCP_PORT: u16 = 4040;
pub const UI_PORT: u16 = 4041;
const DEFAULT_DOMAIN: &str = "example.com";

/// Read DOMAIN_NAME from /opt/codery/.env. Returns DEFAULT_DOMAIN if not set.
pub fn load_domain() -> String {
    let content = match std::fs::read_to_string(ENV_FILE) {
        Ok(c) => c,
        Err(_) => return DEFAULT_DOMAIN.to_string(),
    };
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(value) = line.strip_prefix("DOMAIN_NAME=") {
            let domain = value.trim().to_string();
            if !domain.is_empty() {
                return domain;
            }
        }
    }
    DEFAULT_DOMAIN.to_string()
}

pub fn mcp_host(domain: &str) -> String {
    format!("mcp.{}", domain)
}

pub fn ui_host(domain: &str) -> String {
    format!("ci.{}", domain)
}

/// Returns the container name for a service+color pair.
pub fn container_name(service: &str, color: &str) -> String {
    format!("codery-{}-{}", service, color)
}

/// Returns the Docker image reference for a service+sha.
/// Used by images.rs which hasn't been migrated to ServiceDef yet.
pub fn image_ref(service: &str, sha: &str) -> String {
    format!("{}:{}-{}", REGISTRY, service, sha)
}

/// Returns the opposite color.
pub fn flip(color: &str) -> &'static str {
    if color == "blue" { "green" } else { "blue" }
}

/// Returns the deploy lock file path for a service.
pub fn deploy_lock_path(service: &str) -> String {
    format!("/run/codery-ci-deploy-{}.lock", service)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flip_colors() {
        assert_eq!(flip("blue"), "green");
        assert_eq!(flip("green"), "blue");
    }

    #[test]
    fn container_names() {
        assert_eq!(container_name("sandbox", "blue"), "codery-sandbox-blue");
        assert_eq!(container_name("apps", "green"), "codery-apps-green");
    }

    #[test]
    fn image_refs() {
        assert_eq!(
            image_ref("sandbox", "abc123"),
            "ghcr.io/coderyoss/codery:sandbox-abc123"
        );
    }

    #[test]
    fn deploy_lock_path_uses_service_name() {
        assert_eq!(
            deploy_lock_path("sandbox"),
            "/run/codery-ci-deploy-sandbox.lock"
        );
        assert_eq!(
            deploy_lock_path("apps"),
            "/run/codery-ci-deploy-apps.lock"
        );
    }

    #[test]
    fn load_domain_default_when_no_env() {
        let domain = load_domain();
        assert_eq!(domain, "example.com");
    }
}
