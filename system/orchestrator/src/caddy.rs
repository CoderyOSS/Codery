use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::process::Command;

use crate::config;
use crate::db::{self, UnifiedRoute};
use crate::service_def::ServiceDef;
use crate::state;

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

    let conn = db::open()?;
    let routes = db::build_route_map(&conn)?;

    let content = generate_from_routes(&routes, &colors, &domain)?;
    fs::write(config::CADDY_CONFIG, &content)
        .with_context(|| format!("failed to write {}", config::CADDY_CONFIG))?;
    reload_caddy()?;
    println!("[caddy] Caddyfile written and Caddy reloaded (apply_all)");
    Ok(())
}

pub fn generate_from_routes(
    routes: &[UnifiedRoute],
    colors: &HashMap<String, String>,
    domain: &str,
) -> Result<String> {
    let mut caddy = String::from(
        "{\n    acme_dns cloudflare {$CLOUDFLARE_API_TOKEN}\n}\n",
    );

    for route in routes {
        let fqdn = if route.subdomain.contains('.') {
            route.subdomain.clone()
        } else {
            format!("{}.{}", route.subdomain, domain)
        };

        let host_port = match route.target.as_str() {
            "host" => route.port,
            "sandbox" => {
                let color = colors.get("sandbox").map(|s| s.as_str()).unwrap_or("blue");
                sandbox_host_port(color, route.port)
            }
            "apps" => {
                let color = colors.get("apps").map(|s| s.as_str()).unwrap_or("blue");
                let offset: u16 = if color == "blue" { 0 } else { 10000 };
                route.port + offset
            }
            target => {
                let c = colors.get(target).map(|s| s.as_str()).unwrap_or("blue");
                let def = ServiceDef::load(target).ok();
                if let Some(def) = def {
                    def.port_scheme.host_port(c, route.port)
                } else {
                    route.port
                }
            }
        };
        caddy.push_str(&caddy_block(&fqdn, host_port));
    }

    Ok(caddy)
}

fn sandbox_host_port(color: &str, container_port: u16) -> u16 {
    let offset: u16 = if color == "blue" { 10000 } else { 20000 };
    container_port + offset
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox_route(subdomain: &str, port: u16) -> UnifiedRoute {
        UnifiedRoute {
            subdomain: subdomain.to_string(),
            port,
            target: "sandbox".to_string(),
            internal_port: None,
        }
    }

    fn apps_route(subdomain: &str, internal_port: u16) -> UnifiedRoute {
        UnifiedRoute {
            subdomain: subdomain.to_string(),
            port: 8080,
            target: "apps".to_string(),
            internal_port: Some(internal_port),
        }
    }

    fn host_route(subdomain: &str, port: u16) -> UnifiedRoute {
        UnifiedRoute {
            subdomain: subdomain.to_string(),
            port,
            target: "host".to_string(),
            internal_port: None,
        }
    }

    fn colors(sandbox: &str, apps: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("sandbox".to_string(), sandbox.to_string());
        m.insert("apps".to_string(), apps.to_string());
        m
    }

    #[test]
    fn sandbox_blue_uses_10k_offset() {
        let routes = vec![
            sandbox_route("opencode", 3000),
            sandbox_route("cli", 7681),
        ];
        let caddy = generate_from_routes(&routes, &colors("blue", "blue"), "example.com").unwrap();
        assert!(caddy.contains("reverse_proxy localhost:13000"));
        assert!(caddy.contains("reverse_proxy localhost:17681"));
    }

    #[test]
    fn sandbox_green_uses_20k_offset() {
        let routes = vec![
            sandbox_route("opencode", 3000),
        ];
        let caddy = generate_from_routes(&routes, &colors("green", "blue"), "example.com").unwrap();
        assert!(caddy.contains("reverse_proxy localhost:23000"));
    }

    #[test]
    fn apps_green_uses_10k_offset() {
        let routes = vec![
            apps_route("myapp", 3001),
        ];
        let caddy = generate_from_routes(&routes, &colors("blue", "green"), "example.com").unwrap();
        assert!(caddy.contains("reverse_proxy localhost:18080"));
    }

    #[test]
    fn apps_blue_no_offset() {
        let routes = vec![
            apps_route("myapp", 3001),
        ];
        let caddy = generate_from_routes(&routes, &colors("blue", "blue"), "example.com").unwrap();
        assert!(caddy.contains("reverse_proxy localhost:8080"));
    }

    #[test]
    fn host_routes_no_offset() {
        let routes = vec![
            host_route("mcp", 4040),
        ];
        let caddy = generate_from_routes(&routes, &colors("blue", "blue"), "example.com").unwrap();
        assert!(caddy.contains("reverse_proxy localhost:4040"));
    }

    #[test]
    fn fqdn_subdomain_used_as_is() {
        let routes = vec![
            UnifiedRoute {
                subdomain: "myapp.custom.com".to_string(),
                port: 8080,
                target: "apps".to_string(),
                internal_port: Some(3001),
            },
        ];
        let caddy = generate_from_routes(&routes, &colors("blue", "blue"), "example.com").unwrap();
        assert!(caddy.contains("myapp.custom.com"));
        assert!(!caddy.contains("myapp.custom.com.example.com"));
    }
}
