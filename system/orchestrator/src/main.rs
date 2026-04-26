use anyhow::Result;

mod caddy;
mod config;
mod daemon;
mod deploy;
mod deploy_lock;
mod images;
mod mcp;
mod preflight;
mod service_def;
mod state;
mod tcp_proxy;
mod ui;
mod validate;

/// Insert an iptables ACCEPT rule for the given port from Docker bridge subnets,
/// unless one already exists. Silently ignores failures (iptables may not be
/// available or the rule may already be installed).
pub(crate) fn open_port_for_docker_bridges(port: u16) {
    let port_str = port.to_string();
    // -C checks if the rule exists; exit code 0 = exists, non-zero = absent.
    let already_open = std::process::Command::new("iptables")
        .args(["-C", "INPUT", "-p", "tcp", "--dport", &port_str,
               "-s", "172.16.0.0/12", "-j", "ACCEPT"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !already_open {
        let result = std::process::Command::new("iptables")
            .args(["-I", "INPUT", "1", "-p", "tcp", "--dport", &port_str,
                   "-s", "172.16.0.0/12", "-j", "ACCEPT"])
            .output();
        match result {
            Ok(o) if o.status.success() =>
                println!("[mcp] Added iptables ACCEPT rule: Docker bridges → port {}", port),
            Ok(o) =>
                eprintln!("[mcp] iptables rule failed: {}", String::from_utf8_lossy(&o.stderr)),
            Err(e) =>
                eprintln!("[mcp] iptables not available: {}", e),
        }
    } else {
        println!("[mcp] iptables ACCEPT rule already present for port {}", port);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("--version") | Some("-V") => {
            println!("codery-ci {}", env!("CARGO_PKG_VERSION"));
        }
        Some("preflight") => {
            preflight::run()?;
            println!("[preflight] all checks passed");
        }
        Some("deploy") => {
            let service = args
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("missing service argument"))?;
            let sha = args
                .get(3)
                .ok_or_else(|| anyhow::anyhow!("missing sha argument"))?;
            let _lock = match deploy_lock::DeployLock::try_acquire(service) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[deploy] ERROR ({}): {}", service, e);
                    std::process::exit(1);
                }
            };
            deploy::run(service, sha).await?;
        }
        Some("validate") => {
            // Dry-run validation: checks all preconditions without starting any containers.
            // Usage: codery-ci validate <service> <sha>
            let service = args
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("missing service argument"))?;
            let sha = args
                .get(3)
                .ok_or_else(|| anyhow::anyhow!("missing sha argument"))?;

            let def = service_def::ServiceDef::load(service)?;
            let docker = bollard::Docker::connect_with_socket_defaults()?;
            let active = state::read_active(service)?;
            let inactive = config::flip(&active);
            validate::check_deploy(&def, sha, inactive, &docker).await?;
            println!("[validate] Passed — safe to deploy {} @ {}", service, sha);
        }
        Some("reload-routes") => {
            // Regenerate Caddyfile from all service YAMLs and reload Caddy.
            // Use this when proxy/apps-routes.json changes without a container deploy.
            caddy::apply_all()?;
            println!("[routes] Reloaded Caddyfile from all service definitions");
        }
        Some("serve") => {
            // Start the MCP server. Reads --port N or defaults to MCP_PORT.
            let port = args
                .windows(2)
                .find(|w| w[0] == "--port")
                .and_then(|w| w[1].parse::<u16>().ok())
                .unwrap_or(config::MCP_PORT);

            // Allow Docker bridge networks (172.16.0.0/12) to reach the MCP server.
            // On Linux hosts, UFW or iptables may block connections from Docker bridge
            // IPs to host ports not explicitly opened. OpenCode runs in a Docker
            // container and connects via host.docker.internal (the Docker bridge gateway),
            // so we insert an explicit ACCEPT rule for the MCP port.
            open_port_for_docker_bridges(port);

            mcp::serve(port).await?;
        }
        Some("serve-ui") => {
            let port = args
                .windows(2)
                .find(|w| w[0] == "--port")
                .and_then(|w| w[1].parse::<u16>().ok())
                .unwrap_or(config::UI_PORT);
            ui::serve(port).await?;
        }
        Some("serve-tcp-proxy") => {
            tcp_proxy::serve().await?;
        }
        Some("daemon") => {
            daemon::serve().await?;
        }
        _ => {
            eprintln!(
                "Usage: codery-ci [--version | preflight | deploy <service> <sha> | \
                 validate <service> <sha> | reload-routes | daemon | \
                 serve [--port N] | serve-ui [--port N] | serve-tcp-proxy]"
            );
            std::process::exit(1);
        }
    }
    Ok(())
}
