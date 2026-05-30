use anyhow::{Context, Result};
use bollard::Docker;
use bollard::exec::{CreateExecOptions, StartExecResults};
use futures_util::StreamExt;
use std::fs;

use crate::db::{self, UnifiedRoute};
use crate::{config, state};

pub async fn generate_and_reload() -> Result<()> {
    let conn = db::open()?;
    let routes = db::build_route_map(&conn)?;
    let domain = config::load_domain();
    let content = generate_config(&routes, &domain);

    let to_write = if content.is_empty() {
        "server {\n    listen 8080 default_server;\n    return 503 \"No apps configured\";\n}\n".to_string()
    } else {
        content
    };

    if let Some(parent) = std::path::Path::new(config::NGINX_CONFIG).parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent dir for {}", config::NGINX_CONFIG))?;
    }
    fs::write(config::NGINX_CONFIG, &to_write)
        .with_context(|| format!("failed to write {}", config::NGINX_CONFIG))?;

    let count = routes.iter().filter(|r| r.internal_port.is_some()).count();
    println!("[nginx] Wrote config with {} server block(s)", count);

    reload_in_active_container().await
}

pub(crate) fn generate_config(routes: &[UnifiedRoute], domain: &str) -> String {
    let blocks: Vec<String> = routes
        .iter()
        .filter_map(|r| {
            let internal_port = r.internal_port?;
            if r.target != "apps" {
                return None;
            }
            let fqdn = if r.subdomain.contains('.') {
                r.subdomain.clone()
            } else {
                format!("{}.{}", r.subdomain, domain)
            };
            let cache_headers = if r.no_cache {
                "\n        add_header Cache-Control \"no-store, no-cache, must-revalidate, max-age=0\" always;\
                 \n        add_header Pragma \"no-cache\" always;\
                 \n        add_header Expires \"0\" always;"
            } else {
                ""
            };
            Some(format!(
                "server {{\n    listen 8080;\n    server_name {fqdn};\n    location / {{{cache_headers}\n        proxy_pass http://127.0.0.1:{internal_port};\n        proxy_set_header Host $host;\n        proxy_set_header X-Real-IP $remote_addr;\n        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;\n    }}\n}}\n"
            ))
        })
        .collect();

    if blocks.is_empty() {
        return String::new();
    }

    let mut out = blocks.join("\n");
    out.push_str("\nserver {\n    listen 8080 default_server;\n    return 404;\n}\n");
    out
}

async fn reload_in_active_container() -> Result<()> {
    let color = match state::read_active("apps") {
        Ok(c) => c,
        Err(_) => {
            println!("[nginx] No active apps state — skipping reload");
            return Ok(());
        }
    };
    let container = config::container_name("apps", &color);

    let docker = Docker::connect_with_socket_defaults()
        .context("failed to connect to Docker")?;

    let running = docker.inspect_container(&container, None).await
        .ok()
        .and_then(|i| i.state)
        .and_then(|s| s.running)
        .unwrap_or(false);

    if !running {
        println!("[nginx] Container {} not running — skipping reload", container);
        return Ok(());
    }

    let exec = docker.create_exec(
        &container,
        CreateExecOptions {
            cmd: Some(vec!["nginx", "-s", "reload"]),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        },
    ).await.with_context(|| format!("failed to create exec in {}", container))?;

    if let StartExecResults::Attached { mut output, .. } = docker.start_exec(&exec.id, None).await? {
        while let Some(msg) = output.next().await {
            if let bollard::container::LogOutput::StdErr { message } = msg? {
                let text = String::from_utf8_lossy(&message);
                if !text.trim().is_empty() {
                    eprint!("[nginx] {}", text);
                }
            }
        }
    }

    println!("[nginx] Reloaded Nginx in {}", container);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apps_route(subdomain: &str, internal_port: Option<u16>) -> UnifiedRoute {
        UnifiedRoute {
            subdomain: subdomain.to_string(),
            port: 8080,
            target: "apps".to_string(),
            internal_port,
            no_cache: false,
        }
    }

    fn host_route(subdomain: &str, port: u16) -> UnifiedRoute {
        UnifiedRoute {
            subdomain: subdomain.to_string(),
            port,
            target: "host".to_string(),
            internal_port: None,
            no_cache: false,
        }
    }

    #[test]
    fn two_apps_produce_two_server_blocks_plus_default() {
        let routes = vec![
            apps_route("myapp", Some(3001)),
            apps_route("otherapp", Some(3002)),
        ];
        let cfg = generate_config(&routes, "example.com");
        assert!(cfg.contains("server_name myapp.example.com;"));
        assert!(cfg.contains("proxy_pass http://127.0.0.1:3001;"));
        assert!(cfg.contains("server_name otherapp.example.com;"));
        assert!(cfg.contains("proxy_pass http://127.0.0.1:3002;"));
        assert!(cfg.contains("listen 8080 default_server;"));
    }

    #[test]
    fn route_without_internal_port_is_skipped() {
        let routes = vec![apps_route("myapp", None)];
        let cfg = generate_config(&routes, "example.com");
        assert!(cfg.is_empty());
    }

    #[test]
    fn empty_routes_returns_empty_string() {
        assert!(generate_config(&[], "example.com").is_empty());
    }

    #[test]
    fn full_fqdn_subdomain_used_as_is() {
        let routes = vec![apps_route("myapp.custom.com", Some(3001))];
        let cfg = generate_config(&routes, "example.com");
        assert!(cfg.contains("server_name myapp.custom.com;"));
        assert!(!cfg.contains("myapp.custom.com.example.com"));
    }

    #[test]
    fn host_routes_excluded_from_nginx() {
        let routes = vec![host_route("mcp", 4040)];
        let cfg = generate_config(&routes, "example.com");
        assert!(cfg.is_empty());
    }

    #[test]
    fn no_cache_route_adds_cache_headers() {
        let routes = vec![UnifiedRoute {
            subdomain: "myapp".to_string(),
            port: 8080,
            target: "apps".to_string(),
            internal_port: Some(3001),
            no_cache: true,
        }];
        let cfg = generate_config(&routes, "example.com");
        assert!(cfg.contains("Cache-Control"));
        assert!(cfg.contains("no-store"));
        assert!(cfg.contains("Pragma"));
        assert!(cfg.contains("Expires"));
    }

    #[test]
    fn cache_route_has_no_cache_headers() {
        let routes = vec![apps_route("myapp", Some(3001))];
        let cfg = generate_config(&routes, "example.com");
        assert!(!cfg.contains("Cache-Control"));
        assert!(!cfg.contains("Pragma"));
    }
}
