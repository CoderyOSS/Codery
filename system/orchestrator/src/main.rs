use anyhow::{anyhow, Context, Result};

mod caddy;
mod config;
mod daemon;
mod db;
mod deploy;
mod deploy_lock;
mod images;
mod mcp;
mod nginx;
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
            let conn = db::open()?;
            db::init(&conn)?;
            db::sync_launchy(&conn)?;
            caddy::apply_all()?;
            nginx::generate_and_reload().await?;
            println!("[routes] Reloaded Caddyfile and Nginx");
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
            let (events_tx, _) = tokio::sync::broadcast::channel::<String>(64);
            let ops: ui::Ops = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
            ui::serve(port, std::sync::Arc::new(events_tx), ops).await?;
        }
        Some("serve-tcp-proxy") => {
            tcp_proxy::serve().await?;
        }
        Some("daemon") => {
            daemon::serve().await?;
        }
        Some("build") => {
            // Usage: codery-ci build <service> <tag> [--dockerfile PATH] [--context PATH]
            //
            // Wraps `docker build` with the canonical image tag for the service.
            // Run from the Codery repo root so the default dockerfile paths resolve.
            //
            // Default dockerfile lookup:
            //   sandbox → examples/Dockerfile.sandbox
            //   apps    → containers/apps/Dockerfile
            //   <other> → containers/<service>/Dockerfile
            //
            // Tags the result as ghcr.io/coderyoss/codery:{service}-{tag} so that
            // `deploy-preview` / `deploy` can find it locally with `sha=tag`.
            let service = args
                .get(2)
                .ok_or_else(|| anyhow!("missing service argument"))?;
            let tag = args
                .get(3)
                .ok_or_else(|| anyhow!("missing tag argument (e.g. host-XYZ or a sha)"))?;

            let dockerfile = flag_value(&args, "--dockerfile").map(PathBuf::from).unwrap_or_else(|| {
                default_dockerfile(service)
                    .unwrap_or_else(|| PathBuf::from(format!("containers/{}/Dockerfile", service)))
            });
            let context = flag_value(&args, "--context")
                .unwrap_or_else(|| ".".to_string());
            let image = config::image_ref(service, tag);

            println!(
                "[build] docker build -t {} -f {} {}",
                image,
                dockerfile.display(),
                context
            );
            if !dockerfile.exists() {
                return Err(anyhow!(
                    "dockerfile not found at {} (cwd: {}) — pass --dockerfile PATH explicitly",
                    dockerfile.display(),
                    std::env::current_dir().unwrap_or_default().display()
                ));
            }

            let status = std::process::Command::new("docker")
                .args(["build", "-t", &image, "-f", &dockerfile.to_string_lossy(), &context])
                .status()
                .context("failed to spawn docker build")?;
            if !status.success() {
                return Err(anyhow!("docker build failed (exit {:?})", status.code()));
            }
            println!("[build] Built {} — deploy with: codery-ci deploy-preview {} {}", image, service, tag);
        }
        Some("deploy-preview") => {
            // Usage: codery-ci deploy-preview <service> <sha> [--port <container_port>]
            //
            // Starts the inactive color WITHOUT cutting over, then registers a
            // preview route at {service}-preview.{domain} pointing at the inactive
            // color's host port. Active container keeps running — safe to verify
            // the new image before promoting.
            //
            // Preview port auto-resolution:
            //   1. --port flag if given
            //   2. First named port with a subdomain (e.g. sandbox → opencode:3000)
            //   3. 8080 if the service has a port_range that includes it (apps)
            //
            // Follow up with either:
            //   codery-ci cutover <service>            # promote (kills old active)
            //   codery-ci cancel-preview <service>     # abort (keeps old active)
            let service = args
                .get(2)
                .ok_or_else(|| anyhow!("missing service argument"))?;
            let sha = args
                .get(3)
                .ok_or_else(|| anyhow!("missing sha argument"))?;

            let _lock = match deploy_lock::DeployLock::try_acquire(service) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[deploy-preview] ERROR ({}): {}", service, e);
                    std::process::exit(1);
                }
            };

            let def = service_def::ServiceDef::load(service)?;
            let preview_container_port: u16 = if let Some(p) = flag_value(&args, "--port") {
                p.parse().context("--port must be a number")?
            } else {
                resolve_preview_port(&def)?
            };

            let inactive = deploy::run_start_inactive(service, sha).await?;

            // Compute host port for the inactive color and register the preview route.
            let host_port = def.port_scheme.host_port(&inactive, preview_container_port);
            let subdomain = db::preview_subdomain(service);
            let conn = db::open()?;
            db::init(&conn)?;
            db::upsert_preview(&conn, &db::PreviewRecord {
                service: service.to_string(),
                subdomain: subdomain.clone(),
                host_port,
                sha: sha.to_string(),
                color: inactive.clone(),
                created_at: String::new(),
            })?;
            caddy::apply_all()?;
            nginx::generate_and_reload().await?;

            let domain = config::load_domain();
            println!(
                "[deploy-preview] {} inactive={} started. Preview at https://{}:{} (direct) or https://{}.{}",
                service, inactive, "localhost", host_port, subdomain, domain
            );
            println!(
                "[deploy-preview] promote:    codery-ci cutover {}",
                service
            );
            println!(
                "[deploy-preview] abort:      codery-ci cancel-preview {}",
                service
            );
        }
        Some("cutover") => {
            // Usage: codery-ci cutover <service> [--sha <sha>]
            //
            // Promotes the currently-running inactive color to active. Stops the
            // old active container. State files / Caddy / Nginx all reload.
            //
            // --sha records the SHA that was deployed. If omitted, the preview's
            // recorded SHA is used (from deploy-preview). If neither is available,
            // an error is returned — cutover refuses to run without a SHA record.
            //
            // WARNING: this kills any sessions in the previously-active container.
            let service = args
                .get(2)
                .ok_or_else(|| anyhow!("missing service argument"))?;
            let _lock = match deploy_lock::DeployLock::try_acquire(service) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[cutover] ERROR ({}): {}", service, e);
                    std::process::exit(1);
                }
            };

            let conn = db::open()?;
            db::init(&conn)?;
            let sha = if let Some(s) = flag_value(&args, "--sha") {
                s
            } else if let Some(p) = db::find_preview(&conn, service)? {
                println!("[cutover] Using SHA {} from existing preview record", p.sha);
                p.sha
            } else {
                return Err(anyhow!(
                    "no --sha given and no preview record for '{}'. \
                     Run deploy-preview first, or pass --sha <sha>.",
                    service
                ));
            };

            deploy::run_cutover(service, &sha).await?;

            // Clean up preview route (now redundant — the new active serves the main subdomain).
            if db::delete_preview(&conn, service)? {
                caddy::apply_all()?;
                nginx::generate_and_reload().await?;
                println!("[cutover] Removed preview route for {}", service);
            }
            println!("[cutover] {} promoted.", service);
        }
        Some("cancel-preview") => {
            // Usage: codery-ci cancel-preview <service>
            //
            // Stops the inactive container and removes the preview route.
            // Active container is untouched. Use to abort a preview deploy.
            let service = args
                .get(2)
                .ok_or_else(|| anyhow!("missing service argument"))?;
            let _lock = match deploy_lock::DeployLock::try_acquire(service) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[cancel-preview] ERROR ({}): {}", service, e);
                    std::process::exit(1);
                }
            };

            deploy::run_cancel_inactive(service).await?;

            let conn = db::open()?;
            db::init(&conn)?;
            if db::delete_preview(&conn, service)? {
                caddy::apply_all()?;
                nginx::generate_and_reload().await?;
                println!("[cancel-preview] Removed preview route for {}", service);
            }
            println!("[cancel-preview] {} preview cancelled — active untouched.", service);
        }
        _ => {
            eprintln!(
                "Usage: codery-ci [--version | preflight | deploy <service> <sha> | \
                 validate <service> <sha> | reload-routes | daemon | \
                 serve [--port N] | serve-ui [--port N] | serve-tcp-proxy | \
                 build <service> <tag> [--dockerfile PATH] [--context PATH] | \
                 deploy-preview <service> <sha> [--port N] | \
                 cutover <service> [--sha <sha>] | \
                 cancel-preview <service>]"
            );
            std::process::exit(1);
        }
    }
    Ok(())
}

// ── CLI helpers ───────────────────────────────────────────────────────────────

use std::path::PathBuf;

/// Read the value of a `--flag VALUE` argument from argv.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].clone())
}

/// Default dockerfile path for known services.
fn default_dockerfile(service: &str) -> Option<PathBuf> {
    match service {
        "sandbox" => Some(PathBuf::from("examples/Dockerfile.sandbox")),
        "apps" => Some(PathBuf::from("containers/apps/Dockerfile")),
        _ => None,
    }
}

/// Resolve which container port to expose via the preview subdomain.
///
/// Priority:
///   1. First named port with a subdomain (e.g. sandbox → opencode:3000)
///   2. 8080 if the service has a port_range that includes it (apps/Nginx)
///   3. Error — caller must pass --port explicitly
fn resolve_preview_port(def: &service_def::ServiceDef) -> Result<u16> {
    if let Some(p) = def.ports.iter().find(|p| p.subdomain.is_some()) {
        return Ok(p.container_port);
    }
    if let Some(ref r) = def.port_range {
        if r.container_start <= 8080 && r.container_end >= 8080 {
            return Ok(8080);
        }
    }
    Err(anyhow!(
        "no preview port could be auto-resolved for service '{}'. \
         Pass --port <container_port> explicitly.",
        def.service
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use service_def::ServiceDef;

    #[test]
    fn flag_value_reads_dash_flag() {
        let args = vec![
            "deploy-preview".to_string(),
            "sandbox".to_string(),
            "abc".to_string(),
            "--port".to_string(),
            "7456".to_string(),
        ];
        assert_eq!(flag_value(&args, "--port"), Some("7456".to_string()));
        assert_eq!(flag_value(&args, "--missing"), None);
    }

    #[test]
    fn default_dockerfile_known_services() {
        assert_eq!(
            default_dockerfile("sandbox").unwrap().to_string_lossy(),
            "examples/Dockerfile.sandbox"
        );
        assert_eq!(
            default_dockerfile("apps").unwrap().to_string_lossy(),
            "containers/apps/Dockerfile"
        );
        assert!(default_dockerfile("custom").is_none());
    }

    #[test]
    fn resolve_preview_port_picks_first_subdomain_port() {
        let def: ServiceDef = serde_yaml::from_str(r#"
service: sandbox
image: ghcr.io/coderyoss/codery:sandbox-{sha}
port_scheme: {blue_offset: 10000, green_offset: 20000}
ports:
  - name: ssh
    container_port: 22
  - name: opencode
    container_port: 3000
    subdomain: opencode
  - name: opendesign
    container_port: 7456
    subdomain: opendesign
health_check: {type: docker, timeout_secs: 30}
volumes: []
network: codery-net
"#).unwrap();
        assert_eq!(resolve_preview_port(&def).unwrap(), 3000);
    }

    #[test]
    fn resolve_preview_port_apps_falls_back_to_8080() {
        let def: ServiceDef = serde_yaml::from_str(r#"
service: apps
image: ghcr.io/coderyoss/codery:apps-{sha}
port_scheme: {blue_offset: 0, green_offset: 10000}
port_range: {container_start: 8000, container_end: 9000}
health_check: {type: docker, timeout_secs: 90}
volumes: []
network: codery-net
"#).unwrap();
        assert_eq!(resolve_preview_port(&def).unwrap(), 8080);
    }

    #[test]
    fn resolve_preview_port_no_subdomain_no_8080_errors() {
        let def: ServiceDef = serde_yaml::from_str(r#"
service: weird
image: ghcr.io/coderyoss/codery:weird-{sha}
port_scheme: {blue_offset: 0, green_offset: 10000}
ports:
  - name: ssh
    container_port: 22
health_check: {type: docker, timeout_secs: 30}
volumes: []
network: codery-net
"#).unwrap();
        assert!(resolve_preview_port(&def).is_err());
    }
}
