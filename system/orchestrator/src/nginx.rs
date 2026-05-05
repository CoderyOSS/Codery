use anyhow::{Context, Result};
use bollard::Docker;
use bollard::exec::{CreateExecOptions, StartExecResults};
use futures_util::StreamExt;
use std::fs;

use crate::caddy::AppRoute;
use crate::{config, state};

pub async fn generate_and_reload() -> Result<()> {
    let routes = load_routes()?;
    let domain = config::load_domain();
    let content = generate_config(&routes, &domain);

    if let Some(parent) = std::path::Path::new(config::NGINX_CONFIG).parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent dir for {}", config::NGINX_CONFIG))?;
    }
    fs::write(config::NGINX_CONFIG, &content)
        .with_context(|| format!("failed to write {}", config::NGINX_CONFIG))?;

    let count = routes.iter().filter(|r| r.internal_port.is_some()).count();
    println!("[nginx] Wrote config with {} server block(s)", count);

    reload_in_active_container().await
}

fn load_routes() -> Result<Vec<AppRoute>> {
    let path = config::APPS_ROUTES;
    if !std::path::Path::new(path).exists() {
        return Ok(vec![]);
    }
    let data = fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path))?;
    serde_json::from_str(&data)
        .with_context(|| format!("failed to parse {}", path))
}

pub(crate) fn generate_config(routes: &[AppRoute], domain: &str) -> String {
    let blocks: Vec<String> = routes
        .iter()
        .filter_map(|r| {
            let internal_port = r.internal_port?;
            let fqdn = if r.subdomain.contains('.') {
                r.subdomain.clone()
            } else {
                format!("{}.{}", r.subdomain, domain)
            };
            Some(format!(
                "server {{\n    listen 8080;\n    server_name {fqdn};\n    location / {{\n        proxy_pass http://127.0.0.1:{internal_port};\n        proxy_set_header Host $host;\n        proxy_set_header X-Real-IP $remote_addr;\n        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;\n    }}\n}}\n"
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

    fn route(subdomain: &str, internal_port: Option<u16>) -> AppRoute {
        AppRoute { subdomain: subdomain.to_string(), port: 8080, internal_port }
    }

    #[test]
    fn two_apps_produce_two_server_blocks_plus_default() {
        let routes = vec![route("myapp", Some(3001)), route("otherapp", Some(3002))];
        let cfg = generate_config(&routes, "example.com");
        assert!(cfg.contains("server_name myapp.example.com;"));
        assert!(cfg.contains("proxy_pass http://127.0.0.1:3001;"));
        assert!(cfg.contains("server_name otherapp.example.com;"));
        assert!(cfg.contains("proxy_pass http://127.0.0.1:3002;"));
        assert!(cfg.contains("listen 8080 default_server;"));
    }

    #[test]
    fn route_without_internal_port_is_skipped() {
        let routes = vec![route("myapp", None)];
        let cfg = generate_config(&routes, "example.com");
        assert!(cfg.is_empty());
    }

    #[test]
    fn empty_routes_returns_empty_string() {
        assert!(generate_config(&[], "example.com").is_empty());
    }

    #[test]
    fn full_fqdn_subdomain_used_as_is() {
        let routes = vec![route("myapp.custom.com", Some(3001))];
        let cfg = generate_config(&routes, "example.com");
        assert!(cfg.contains("server_name myapp.custom.com;"));
        assert!(!cfg.contains("myapp.custom.com.example.com"));
    }
}
