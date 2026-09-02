use anyhow::{anyhow, Context, Result};

mod caddy;
mod config;
mod daemon;
mod diagnose;
mod db;
mod deploy;
mod deploy_lock;
mod images;
mod mcp;
mod mcp_exec;
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

    // Per-subcommand --help: short-circuit before the main match so users can
    // discover usage without reading the flat usage string at the bottom.
    // e.g. `codery-ci cutover --help` prints focused cutover usage.
    if args.len() >= 3 {
        let third = args.get(2).map(|s| s.as_str()).unwrap_or("");
        if third == "--help" || third == "-h" {
            if let Some(sub) = args.get(1) {
                print_subcommand_help(sub);
                return Ok(());
            }
        }
    }

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
            db::sync_s6(&conn)?;
            caddy::apply_all()?;
            nginx::generate_and_reload().await?;
            println!("[routes] Reloaded Caddyfile and Nginx");
        }
        Some("diagnose") => {
            // Usage: codery-ci diagnose [--json]
            //
            // Read-only mismatch detector. Cross-checks:
            //   - state file vs running containers (color mismatch / dead)
            //   - route targets vs TCP listeners (dead port)
            //   - preview table (uncut-over deploy-preview)
            //
            // Prints a human-readable report by default; --json emits the
            // structured DiagnoseReport. Exit 1 if any issue found, 0 otherwise.
            let as_json = args.iter().any(|a| a == "--json");
            let report = diagnose::run().await?;
            if as_json {
                let json = serde_json::to_string_pretty(&report)?;
                println!("{}", json);
            } else {
                print!("{}", report.format_human());
            }
            if !report.all_healthy {
                std::process::exit(1);
            }
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
            // Promotes the inactive color to active. Stops the old active
            // container. State files / Caddy / Nginx all reload.
            //
            // SHA resolution priority:
            //   1. --sha <sha>          (explicit; wins over everything)
            //   2. staged preview       (from a prior `deploy-preview`)
            //   3. newest local image   (auto; nothing-newer exits 0)
            //
            // Safety: refuses to flip state if the inactive color isn't actually
            // running the promoted image. Stale previews bail with a fix message.
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
            let sha_opt = flag_value(&args, "--sha");
            let preview = db::find_preview(&conn, service)?.map(|p| p.sha);
            if let (None, Some(p)) = (sha_opt.as_deref(), preview.as_deref()) {
                println!("[cutover] Found staged preview sha={}", p);
            }

            deploy::run_cutover(service, sha_opt.as_deref(), preview.as_deref()).await?;

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
        Some("mcp-exec") => {
            // Usage: codery-ci mcp-exec <enable|disable|status>
            //
            // Toggles whether the codery_exec MCP tool is allowed to spawn
            // codery-ci subcommands (build-only allowlist). The toggle is a
            // state file at /opt/codery/state/mcp-exec.enabled.
            //
            // Default: disabled. Enable when handing the agent the build loop;
            // disable when not actively iterating to keep the security surface flat.
            let sub = args
                .get(2)
                .map(|s| s.as_str())
                .ok_or_else(|| anyhow!("missing subcommand: enable|disable|status"))?;
            match sub {
                "enable" => {
                    mcp_exec::set_toggle(true)?;
                    println!("[mcp-exec] Enabled — codery_exec MCP tool can run build/validate/deploy-preview/cancel-preview");
                }
                "disable" => {
                    mcp_exec::set_toggle(false)?;
                    println!("[mcp-exec] Disabled — codery_exec MCP tool will refuse all calls");
                }
                "status" => {
                    let state = if mcp_exec::toggle_enabled() { "enabled" } else { "disabled" };
                    println!("[mcp-exec] {}", state);
                }
                other => {
                    return Err(anyhow!(
                        "unknown mcp-exec subcommand '{}': expected enable|disable|status",
                        other
                    ));
                }
            }
        }
        _ => {
            eprintln!(
                "Usage: codery-ci [--version | preflight | deploy <service> <sha> | \
                 validate <service> <sha> | reload-routes | diagnose [--json] | daemon | \
                 serve [--port N] | serve-ui [--port N] | serve-tcp-proxy | \
                 build <service> <tag> [--dockerfile PATH] [--context PATH] | \
                 deploy-preview <service> <sha> [--port N] | \
                 cutover <service> [--sha <sha>] | \
                 cancel-preview <service> | \
                 mcp-exec <enable|disable|status>]"
            );
            eprintln!("\nRun `codery-ci <command> --help` for focused usage on any subcommand.");
            std::process::exit(1);
        }
    }
    Ok(())
}

// ── Per-subcommand --help ────────────────────────────────────────────────────

fn print_subcommand_help(sub: &str) {
    let text = match sub {
        "preflight" => "\
codery-ci preflight

Run preflight health checks: supervisord, Tailscale, and the Caddy admin API.
Exits non-zero if any check fails.

Docs: AGENTS.md → \"Host Environment\"
",
        "deploy" => "\
codery-ci deploy <service> <sha>

Full blue/green deploy. Pulls the image from GHCR, starts the inactive color,
health-checks it, cuts over (rewrites Caddyfile + state file), stops the old
container, prunes stale images. Acquires a per-service deploy lock.

Arguments:
  <service>  Service name (sandbox, apps, ...)
  <sha>      Image tag suffix (resolved as ghcr.io/coderyoss/codery:<service>-<sha>)

Example:
  codery-ci deploy sandbox abc123

Docs: AGENTS.md → \"Blue/Green Deployment\"
",
        "validate" => "\
codery-ci validate <service> <sha>

Dry-run pre-deploy validation. Runs all preconditions (required_env, bind-mount
paths, image pullability, free host ports) without starting any containers.
Locally-cached images skip the GHCR pull check.

Arguments:
  <service>  Service name
  <sha>      Image tag suffix

Example:
  codery-ci validate sandbox host-xyz

Docs: AGENTS.md → \"Pre-deploy validation\" / \"Dry-run validation\"
",
        "reload-routes" => "\
codery-ci reload-routes

Regenerate the Caddyfile and Nginx config from all service YAMLs, routes.yaml,
and runtime apps in codery.db, then reload both. No container restart.

Use after editing proxy/routes.yaml or a service YAML's routing fields.
For Dockerfile / volume / image changes, push to main and run a full deploy.

Docs: AGENTS.md → \"Service Declarations\"
",
        "diagnose" => "\
codery-ci diagnose [--json]

Read-only mismatch detector. Cross-checks the state file against running
containers, route target ports against TCP listeners, and flags any uncut-over
preview deploys. Each issue includes the exact shell command to fix it.

Exit code 1 if any issue is found, 0 if all healthy.

Flags:
  --json  Emit the structured DiagnoseReport instead of human-readable text

Example:
  codery-ci diagnose
  codery-ci diagnose --json

Docs: docs/superpowers/specs/2026-07-28-diagnose-command-design.md
",
        "build" => "\
codery-ci build <service> <tag> [--dockerfile PATH] [--context PATH]

Wraps `docker build` with the canonical image tag for the service. Tags the
result as ghcr.io/coderyoss/codery:<service>-<tag> so deploy-preview / deploy
can find it locally without a GHCR pull.

Default Dockerfile lookup:
  sandbox → examples/Dockerfile.sandbox
  apps    → containers/apps/Dockerfile
  <other> → containers/<service>/Dockerfile

Flags:
  --dockerfile PATH  Override the default Dockerfile path
  --context PATH     Override the build context (default: current directory)

Example:
  codery-ci build sandbox host-xyz

Docs: AGENTS.md → \"Preview Deploys\"
",
        "deploy-preview" => "\
codery-ci deploy-preview <service> <sha> [--port N]

Start the inactive color with the given image and register a preview route at
<service>-preview.<domain>. The active container keeps running — sessions
inside it survive. Promote with `cutover`, abort with `cancel-preview`.

Locally-built images skip the GHCR pull (see build subcommand).

Arguments:
  <service>  Service name
  <sha>      Image tag suffix (matches a tag built via `codery-ci build`)

Flags:
  --port N  Container port to expose via the preview subdomain.
            Auto-resolved from service YAML if omitted.

Example:
  codery-ci deploy-preview sandbox host-xyz

Docs: AGENTS.md → \"Preview Deploys\"
",
        "cutover" => "\
codery-ci cutover <service> [--sha <sha>]

Promote the inactive color to active. Updates the state file, regenerates the
Caddyfile, stops the previously-active container, and removes the preview route.

SHA resolution (in priority order):
  1. --sha <sha>          Explicit; wins over everything.
  2. staged preview       From a prior `deploy-preview`. Bails if the inactive
                          container is not actually running the previewed image
                          (stale preview — run cancel-preview then cutover again).
  3. newest local image   Auto-picked from the host Docker cache. If the active
                          container already runs the newest, prints \"nothing
                          newer to cut to\" and exits 0.

Safety: refuses to stop the active container unless the inactive color is
verified running the promoted image.

Use to finalize a preview deploy, promote a freshly-built image, or recover
when routing points at a dead container (run `diagnose` to confirm state).

Arguments:
  <service>  Service name

Flags:
  --sha <sha>  Promote this SHA explicitly. Default: preview → newest local.

Example:
  codery-ci cutover sandbox
  codery-ci cutover sandbox --sha sandbox-nixos-v1

Docs: AGENTS.md → \"Preview Deploys\"
",
        "cancel-preview" => "\
codery-ci cancel-preview <service>

Abort a preview deploy. Stops the inactive container and removes the preview
route. The active container is untouched.

Arguments:
  <service>  Service name

Example:
  codery-ci cancel-preview sandbox

Docs: AGENTS.md → \"Preview Deploys\"
",
        "mcp-exec" => "\
codery-ci mcp-exec <enable|disable|status>

Toggle whether the codery_exec MCP tool is allowed to spawn build/validate/
deploy-preview/cancel-preview jobs. Default: disabled. Enable only while
actively iterating via the agent build loop.

State file: /opt/codery/state/mcp-exec.enabled
Job logs:   /var/log/codery-ci-mcp/

Subcommands:
  enable   Allow codery_exec calls
  disable  Refuse codery_exec calls (default)
  status   Print current state

Docs: AGENTS.md → \"MCP host exec (agent build loop)\"
",
        "serve" => "\
codery-ci serve [--port N]

Start the CoderyCI MCP server (HTTP+SSE on /sse). Default port 4040.

OpenCode in the sandbox connects to this endpoint to call infrastructure tools.
An iptables ACCEPT rule is added so Docker bridge networks can reach the port.

Flags:
  --port N  Port to listen on (default: MCP_PORT from /opt/codery/.env or 4040)
",
        "serve-ui" => "\
codery-ci serve-ui [--port N]

Start the deploy-progress terminal UI server. Default port 4041.

Flags:
  --port N  Port to listen on (default: 4041)
",
        "serve-tcp-proxy" => "\
codery-ci serve-tcp-proxy

Run the stable-port TCP proxy (e.g. :2222 → active sandbox sshd). Reads the
active color from the state file on each inbound connection and forwards to
the color-specific host port.
",
        "daemon" => "\
codery-ci daemon

Run the orchestrator daemon (background coordinator mode).
",
        _ => {
            eprintln!(
                "No help for '{}'. Known subcommands: preflight, deploy, validate, \
                 reload-routes, diagnose, build, deploy-preview, cutover, cancel-preview, \
                 mcp-exec, serve, serve-ui, serve-tcp-proxy, daemon.",
                sub
            );
            return;
        }
    };
    print!("{}", text);
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
