use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::process::Command;

use crate::config;
use crate::service_def::ServiceDef;
use crate::state;

/// Per-app subdomain→container-port route loaded from apps-routes.json (or test fixtures).
#[derive(Deserialize)]
pub struct AppRoute {
    pub subdomain: String,
    pub port: u16,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Regenerate /etc/caddy/Caddyfile from all service YAMLs and reload Caddy.
///
/// Each service's active color is read from its state file. If a state file
/// is missing the service defaults to "blue".
pub fn apply_all() -> Result<()> {
    let defs = ServiceDef::load_all()?;
    let domain = config::load_domain();
    let colors: HashMap<String, String> = defs
        .iter()
        .map(|d| {
            let color = state::read_active(&d.service).unwrap_or_else(|_| "blue".to_string());
            (d.service.clone(), color)
        })
        .collect();

    let content = generate_from_defs(&defs, &colors, None, &domain)?;
    fs::write(config::CADDY_CONFIG, &content)
        .with_context(|| format!("failed to write {}", config::CADDY_CONFIG))?;
    reload_caddy()?;
    println!("[caddy] Caddyfile written and Caddy reloaded (apply_all)");
    Ok(())
}

// ── Generation ────────────────────────────────────────────────────────────────

/// Generate Caddyfile content from a slice of `ServiceDef` values.
///
/// `route_overrides` lets tests inject pre-loaded routes instead of reading
/// from disk. Pass `None` in production to load from each service's `routes_file`.
pub fn generate_from_defs(
    defs: &[ServiceDef],
    colors: &HashMap<String, String>,
    route_overrides: Option<&HashMap<String, Vec<AppRoute>>>,
    domain: &str,
) -> Result<String> {
    let mut caddy = String::from(
        "{\n    acme_dns cloudflare {$CLOUDFLARE_API_TOKEN}\n}\n",
    );

    for def in defs {
        let color = colors.get(&def.service).map(|s| s.as_str()).unwrap_or("blue");

        // Named ports with subdomains (sandbox-style services).
        for port in &def.ports {
            if let Some(subdomain) = &port.subdomain {
                let host_port = def.port_scheme.host_port(color, port.container_port);
                let fqdn = format!("{}.{}", subdomain, domain);
                caddy.push_str(&caddy_block(&fqdn, host_port));
            }
        }

        // Routes file (apps-style services): load JSON routes, apply port_scheme offset.
        if let Some(routes_file) = &def.routes_file {
            let routes: Vec<AppRoute> = if let Some(overrides) = route_overrides {
                // Test path: use injected routes (avoid disk I/O).
                overrides
                    .get(&def.service)
                    .map(|v| v.iter().map(|r| AppRoute { subdomain: r.subdomain.clone(), port: r.port }).collect())
                    .unwrap_or_default()
            } else {
                load_routes_file(routes_file)?
            };

            for route in &routes {
                let fqdn = if route.subdomain.contains('.') {
                    route.subdomain.clone()
                } else {
                    format!("{}.{}", route.subdomain, domain)
                };
                let host_port = def.port_scheme.host_port(color, route.port);
                caddy.push_str(&caddy_block(&fqdn, host_port));
            }
        }
    }

    // MCP server — fixed host process, not a container, no color formula.
    caddy.push_str(&caddy_block(&config::mcp_host(domain), config::MCP_PORT));
    // Rollback UI server — fixed host process, not a container, no color formula.
    caddy.push_str(&caddy_block(&config::ui_host(domain), config::UI_PORT));

    Ok(caddy)
}

fn caddy_block(host: &str, port: u16) -> String {
    format!(
        r#"
{host} {{
    bind {{$TAILSCALE_IP}}
    reverse_proxy localhost:{port}
}}
"#,
        host = host,
        port = port
    )
}

fn load_routes_file(path: &str) -> Result<Vec<AppRoute>> {
    if !std::path::Path::new(path).exists() {
        return Ok(vec![]);
    }
    let data = fs::read_to_string(path).with_context(|| format!("failed to read {}", path))?;
    serde_json::from_str(&data).with_context(|| format!("failed to parse {}", path))
}

// ── Caddy process management ──────────────────────────────────────────────────

fn reload_caddy() -> Result<()> {
    let env_pairs = load_env_pairs();

    let out = Command::new("caddy")
        .args(["reload", "--config", config::CADDY_CONFIG, "--adapter", "caddyfile"])
        .envs(env_pairs)
        .output()
        .context("failed to run caddy reload")?;

    if !out.status.success() {
        anyhow::bail!(
            "caddy reload failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// Parse /opt/codery/.env into (key, value) pairs for subprocess environments.
/// Also injects TAILSCALE_IP from /run/tailscale.ip if not already present in .env.
fn load_env_pairs() -> Vec<(String, String)> {
    let content = match std::fs::read_to_string(config::ENV_FILE) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let mut pairs: Vec<(String, String)> = content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let pos = l.find('=')?;
            Some((l[..pos].to_string(), l[pos + 1..].to_string()))
        })
        .collect();

    if !pairs.iter().any(|(k, _)| k == "TAILSCALE_IP") {
        if let Ok(ip) = std::fs::read_to_string(config::TAILSCALE_IP_FILE) {
            let ip = ip.trim().to_string();
            if !ip.is_empty() {
                pairs.push(("TAILSCALE_IP".to_string(), ip));
            }
        }
    }

    pairs
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox_def() -> ServiceDef {
        serde_yaml::from_str(r#"
service: sandbox
image: ghcr.io/CoderyOSS/codery:sandbox-{sha}
port_scheme:
  blue_offset: 10000
  green_offset: 20000
ports:
  - name: opencode
    container_port: 3000
    subdomain: opencode
  - name: vscode
    container_port: 7000
    subdomain: vscode
  - name: ttyd
    container_port: 7681
    subdomain: cli
health_check:
  type: tcp
  port: opencode
  timeout_secs: 60
  interval_secs: 2
volumes: []
required_env: []
network: codery-net
"#).unwrap()
    }

    fn apps_def() -> ServiceDef {
        serde_yaml::from_str(r#"
service: apps
image: ghcr.io/CoderyOSS/codery:apps-{sha}
port_scheme:
  blue_offset: 0
  green_offset: 10000
port_range:
  container_start: 8000
  container_end: 9000
routes_file: /nonexistent/path.json
health_check:
  type: docker
  timeout_secs: 90
volumes: []
required_env: []
network: codery-net
"#).unwrap()
    }

    fn colors(sandbox: &str, apps: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("sandbox".to_string(), sandbox.to_string());
        m.insert("apps".to_string(), apps.to_string());
        m
    }

    #[test]
    fn generate_blue_sandbox_no_extras() {
        let defs = vec![sandbox_def()];
        let caddy = generate_from_defs(&defs, &colors("blue", "blue"), None, "example.com").unwrap();
        assert!(caddy.contains("reverse_proxy localhost:13000")); // opencode
        assert!(caddy.contains("reverse_proxy localhost:17000")); // vscode
        assert!(caddy.contains("reverse_proxy localhost:17681")); // ttyd
        assert!(caddy.contains(&format!("reverse_proxy localhost:{}", config::MCP_PORT)));
    }

    #[test]
    fn generate_green_sandbox_no_extras() {
        let defs = vec![sandbox_def()];
        let caddy = generate_from_defs(&defs, &colors("green", "blue"), None, "example.com").unwrap();
        assert!(caddy.contains("reverse_proxy localhost:23000")); // opencode green
        assert!(caddy.contains("reverse_proxy localhost:27000")); // vscode green
        assert!(caddy.contains("reverse_proxy localhost:27681")); // ttyd green
    }

    #[test]
    fn apps_offset_for_green() {
        let defs = vec![sandbox_def(), apps_def()];
        let mut route_overrides = HashMap::new();
        route_overrides.insert(
            "apps".to_string(),
            vec![AppRoute { subdomain: "test.example.com".to_string(), port: 8000 }],
        );
        let caddy = generate_from_defs(&defs, &colors("blue", "green"), Some(&route_overrides), "example.com").unwrap();
        assert!(caddy.contains("reverse_proxy localhost:13000")); // sandbox unaffected
        assert!(caddy.contains("reverse_proxy localhost:18000")); // 8000 + 10000 (green offset)
    }

    #[test]
    fn apps_offset_for_blue() {
        let defs = vec![sandbox_def(), apps_def()];
        let mut route_overrides = HashMap::new();
        route_overrides.insert(
            "apps".to_string(),
            vec![AppRoute { subdomain: "test.example.com".to_string(), port: 8000 }],
        );
        let caddy = generate_from_defs(&defs, &colors("blue", "blue"), Some(&route_overrides), "example.com").unwrap();
        assert!(caddy.contains("reverse_proxy localhost:8000")); // 8000 + 0 (blue offset)
    }

    #[test]
    fn mcp_block_always_present() {
        let defs = vec![sandbox_def()];
        let caddy = generate_from_defs(&defs, &colors("blue", "blue"), None, "example.com").unwrap();
        assert!(caddy.contains(&config::mcp_host("example.com")));
        assert!(caddy.contains(&format!("localhost:{}", config::MCP_PORT)));
    }

    #[test]
    fn ui_block_always_present() {
        let defs = vec![sandbox_def()];
        let caddy = generate_from_defs(&defs, &colors("blue", "blue"), None, "example.com").unwrap();
        assert!(caddy.contains(&config::ui_host("example.com")));
        assert!(caddy.contains(&format!("localhost:{}", config::UI_PORT)));
    }
}
