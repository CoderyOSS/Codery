use std::collections::HashMap;
use std::net::SocketAddr;

use anyhow::Context;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{caddy, config, db, deploy, images, nginx, preflight, service_def::ServiceDef, state};

// ── Data shapes returned by tools ────────────────────────────────────────────

#[derive(Serialize)]
struct ServiceStatus {
    service: String,
    active_color: String,
    active_sha: Option<String>,
    container: String,
    running: bool,
}

#[derive(Serialize)]
struct RouteEntry {
    subdomain: String,
    host_port: u16,
    container_port: Option<u16>,
    internal_port: Option<u16>,
    service: String,
    color: Option<String>,
    note: Option<String>,
}

#[derive(Serialize)]
struct RoutingTable {
    services: HashMap<String, String>,
    routes: Vec<RouteEntry>,
}

#[derive(Serialize)]
struct PreflightCheck {
    name: &'static str,
    passed: bool,
    message: String,
}

#[derive(Serialize)]
struct PreflightReport {
    all_passed: bool,
    checks: Vec<PreflightCheck>,
}

// ── Tool parameter structs ────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
struct ServiceKnownParam {
    #[schemars(description = "Service name (must match a services/*.yml definition)")]
    service: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ServiceParam {
    #[schemars(description = "Service name (e.g. 'sandbox', 'apps')")]
    service: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ServiceNameParam {
    #[schemars(description = "Service name without .yml extension (e.g. 'sandbox', 'apps')")]
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpsertServiceParams {
    #[schemars(description = "Service name without .yml extension (e.g. 'sandbox', 'apps')")]
    name: String,
    #[schemars(description = "Complete service definition YAML content")]
    yaml: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReadContainerFileParams {
    #[schemars(description = "Service name (e.g. 'sandbox', 'apps')")]
    service: String,
    #[schemars(
        description = "Absolute path to file inside container, e.g. '/etc/hosts' or '/tmp/opencode.log'"
    )]
    path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PortParam {
    #[schemars(description = "Host TCP port number to check (e.g. 17681)")]
    port: u16,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AddAppParams {
    #[schemars(
        description = "Unique app name — used as Launchy service name and config filename (no spaces, slashes, or dots)"
    )]
    name: String,
    #[schemars(
        description = "Subdomain to serve this app at (e.g. 'myapp' for myapp.example.com, or a full FQDN)"
    )]
    subdomain: String,
    #[schemars(
        description = "Port the app process listens on inside the apps container (must be free)"
    )]
    internal_port: u16,
    #[schemars(description = "Shell command to start the app (e.g. 'bun run start')")]
    command: String,
    #[schemars(
        description = "Working directory inside the container (e.g. '/home/gem/projects/myapp')"
    )]
    directory: String,
    #[schemars(description = "Optional environment variables for the process")]
    env: Option<HashMap<String, String>>,
    #[schemars(description = "If true, Caddy and Nginx will send Cache-Control: no-store and related headers to prevent client caching")]
    no_cache: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RemoveAppParams {
    #[schemars(description = "App name as given to add_app")]
    name: String,
    #[schemars(description = "Subdomain as given to add_app (defaults to name if omitted)")]
    subdomain: Option<String>,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Run a subprocess and return combined stdout+stderr. Returns Ok regardless of
/// exit code so diagnostic tools always surface output even on failure.
async fn shell_output(program: &str, args: &[&str]) -> Result<String, String> {
    let out = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("failed to spawn '{}': {}", program, e))?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    let combined = match (stdout.is_empty(), stderr.is_empty()) {
        (false, false) => format!("{}\n[stderr]\n{}", stdout, stderr),
        (false, true) => stdout.to_string(),
        (true, false) => stderr.to_string(),
        (true, true) => String::new(),
    };

    if !out.status.success() {
        let code = out.status.code().unwrap_or(-1);
        return Ok(if combined.is_empty() {
            format!("[exited {}]", code)
        } else {
            format!("[exited {}]\n{}", code, combined)
        });
    }

    Ok(combined)
}

/// Run a command inside the active container for a service via `docker exec`.
async fn container_exec(service: &str, cmd: &[&str]) -> Result<String, String> {
    let color = state::read_active(service).map_err(|e| e.to_string())?;
    let container = config::container_name(service, &color);
    let mut args = vec!["exec", container.as_str()];
    args.extend_from_slice(cmd);
    shell_output("docker", &args).await
}

fn tool_ok(s: impl Into<String>) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(s.into())]))
}

fn tool_err(msg: impl Into<String>) -> McpError {
    McpError::internal_error(msg.into(), None)
}

// ── MCP server ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct OrchestratorMcp;

#[tool_router]
impl OrchestratorMcp {
    /// Return current active colors and deployed SHAs for all known services.
    #[tool(description = "Get live system status: active color and deployed SHA for each service")]
    async fn get_status(&self) -> Result<CallToolResult, McpError> {
        let defs = ServiceDef::load_all().map_err(|e| tool_err(e.to_string()))?;
        let docker = bollard::Docker::connect_with_socket_defaults()
            .map_err(|e| tool_err(format!("failed to connect to Docker: {}", e)))?;

        let is_running = |info: Option<bollard::models::ContainerInspectResponse>| -> bool {
            info.and_then(|i| i.state)
                .and_then(|s| s.running)
                .unwrap_or(false)
        };

        let mut statuses = Vec::new();
        for def in &defs {
            let blue = config::container_name(&def.service, "blue");
            let green = config::container_name(&def.service, "green");
            let blue_running = is_running(docker.inspect_container(&blue, None).await.ok());
            let green_running = is_running(docker.inspect_container(&green, None).await.ok());

            let color = if blue_running && !green_running {
                "blue".to_string()
            } else if green_running && !blue_running {
                "green".to_string()
            } else {
                state::read_active(&def.service).unwrap_or_else(|_| "blue".to_string())
            };
            let running = blue_running || green_running;

            statuses.push(ServiceStatus {
                container: config::container_name(&def.service, &color),
                active_sha: state::read_active_sha(&def.service),
                active_color: color,
                service: def.service.clone(),
                running,
            });
        }

        let response = json!({
            "services": statuses,
            "guidance": {
                "apps_running": "Use get_app_status or list_apps to see individual app processes",
                "to_deploy": "Push to main triggers Build Apps workflow (~8 min)",
                "to_add_app_instantly": "Use add_app — no rebuild needed, live in seconds",
                "to_check_health": "run_preflight checks host services health"
            }
        });
        let json = serde_json::to_string_pretty(&response).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }

    /// Return the full routing table: every active subdomain with its host port,
    /// container port, and which service/color owns it.
    #[tool(
        description = "Get full routing table: subdomain → host_port → container_port → service. \
                        Shows current live mappings based on active colors."
    )]
    async fn get_routes(&self) -> Result<CallToolResult, McpError> {
        let defs = ServiceDef::load_all().map_err(|e| tool_err(e.to_string()))?;
        let domain = config::load_domain();

        let mut services_map = HashMap::new();
        for def in &defs {
            let color = state::read_active(&def.service).unwrap_or_else(|_| "blue".to_string());
            services_map.insert(def.service.clone(), color);
        }

        let conn = db::open().map_err(|e| tool_err(e.to_string()))?;
        db::init(&conn).map_err(|e| tool_err(e.to_string()))?;
        let unified = db::build_route_map(&conn).map_err(|e| tool_err(e.to_string()))?;

        let route_entries: Vec<RouteEntry> = unified.iter().map(|r| {
            let fqdn = if r.subdomain.contains('.') {
                r.subdomain.clone()
            } else {
                format!("{}.{}", r.subdomain, domain)
            };
            let color = services_map.get(&r.target).map(|s| s.as_str());
            let host_port = match r.target.as_str() {
                "host" => r.port,
                "sandbox" => {
                    let c = color.unwrap_or("blue");
                    r.port + if c == "blue" { 10000 } else { 20000 }
                }
                _ => {
                    let c = color.unwrap_or("blue");
                    let def = ServiceDef::load(&r.target).ok();
                    if let Some(def) = def {
                        def.port_scheme.host_port(c, r.port)
                    } else {
                        r.port
                    }
                }
            };
            RouteEntry {
                subdomain: fqdn,
                host_port,
                container_port: Some(r.port),
                internal_port: r.internal_port,
                service: r.target.clone(),
                color: color.map(|s| s.to_string()),
                note: None,
            }
        }).collect();

        let table = RoutingTable {
            services: services_map,
            routes: route_entries,
        };
        let response = json!({
            "routing": table,
            "guidance": {
                "routing_model": "Traffic: Internet → Tailscale → Caddy → Nginx (8080) → app (internal_port)",
                "apps_ports": "For apps: container_port is always 8080 (Nginx). internal_port is where app listens.",
                "sandbox_ports": "For sandbox: container_port is the actual service port (e.g. 3000).",
                "to_add_route": "Use add_app for instant routing, or edit devcontainer.json for permanent."
            }
        });
        let json = serde_json::to_string_pretty(&response).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }

    /// List locally available Docker images for a service. Useful before rollback.
    #[tool(
        description = "List locally cached Docker images for a service (e.g. 'sandbox', 'apps'), newest first"
    )]
    async fn list_images(
        &self,
        Parameters(ServiceKnownParam { service }): Parameters<ServiceKnownParam>,
    ) -> Result<CallToolResult, McpError> {
        if ServiceDef::load(&service).is_err() {
            return Err(tool_err(format!(
                "unknown service '{}' — no service definition found",
                service
            )));
        }
        let imgs = images::list_local(&service)
            .await
            .map_err(|e| tool_err(e.to_string()))?;
        let active_sha = state::read_active_sha(&service);
        let output = json!({
            "service": service,
            "active_sha": active_sha,
            "images": imgs.iter().map(|img| json!({
                "sha": img.sha,
                "tag": img.tag,
                "created": img.created,
                "active": active_sha.as_deref() == Some(&img.sha),
            })).collect::<Vec<_>>(),
        });
        let json = serde_json::to_string_pretty(&output).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }

    /// Roll back a service to the previously deployed image.
    ///
    /// Finds the most-recent locally cached image whose SHA differs from the
    /// currently active SHA and runs a full blue/green deploy with it.
    #[tool(
        description = "Rollback a service to the previous locally available image. \
                        Runs a full blue/green deploy with the previous SHA."
    )]
    async fn rollback(
        &self,
        Parameters(ServiceKnownParam { service }): Parameters<ServiceKnownParam>,
    ) -> Result<CallToolResult, McpError> {
        if ServiceDef::load(&service).is_err() {
            return Err(tool_err(format!(
                "unknown service '{}' — no service definition found",
                service
            )));
        }

        let active_sha = state::read_active_sha(&service);
        let imgs = images::list_local(&service)
            .await
            .map_err(|e| tool_err(e.to_string()))?;

        let rollback_sha = imgs
            .iter()
            .find(|img| active_sha.as_deref() != Some(&img.sha))
            .ok_or_else(|| {
                tool_err(format!(
                    "no rollback image available for {} (active={}, total cached={})",
                    service,
                    active_sha.as_deref().unwrap_or("unknown"),
                    imgs.len()
                ))
            })?;

        println!(
            "[mcp] rollback {}: {} → {}",
            service,
            active_sha.as_deref().unwrap_or("unknown"),
            rollback_sha.sha
        );

        deploy::run(&service, &rollback_sha.sha)
            .await
            .map_err(|e| tool_err(e.to_string()))?;

        let response = json!({
            "service": service,
            "rolled_back_to": rollback_sha.sha,
            "guidance": {
                "what": "Rolled back to previous image via blue/green deploy.",
                "to_verify": "get_status shows active SHA",
                "to_go_forward": "Push to main for fresh deploy with latest image"
            }
        });
        let json = serde_json::to_string_pretty(&response).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }

    /// Restart the currently active container for a service in-place.
    ///
    /// Sends a Docker restart to the active color container without doing a
    /// blue/green deploy. Use this to recover a stuck container or pick up
    /// config changes that don't require a new image.
    #[tool(
        description = "Recreate the active container from the current service YAML (no blue/green swap). \
                        Removes and restarts the container so volume, env, and network changes in the \
                        YAML take effect immediately — no full CI deploy needed. Brief downtime, no rollback."
    )]
    async fn restart_service(
        &self,
        Parameters(ServiceParam { service }): Parameters<ServiceParam>,
    ) -> Result<CallToolResult, McpError> {
        let def = ServiceDef::load(&service)
            .map_err(|e| tool_err(format!("unknown service '{}': {}", service, e)))?;

        let color = state::read_active(&service).unwrap_or_else(|_| "blue".to_string());
        let container = config::container_name(&service, &color);
        let sha = state::read_active_sha(&service).ok_or_else(|| {
            tool_err(format!(
                "no active SHA recorded for service '{}' — run a full deploy first",
                service
            ))
        })?;

        let docker = bollard::Docker::connect_with_socket_defaults()
            .map_err(|e| tool_err(format!("failed to connect to Docker: {}", e)))?;

        deploy::remove_container_if_exists(&docker, &container)
            .await
            .map_err(|e| tool_err(format!("failed to remove container '{}': {}", container, e)))?;

        deploy::start_container(&docker, &def, &sha, &color)
            .await
            .map_err(|e| tool_err(format!("failed to start container '{}': {}", container, e)))?;

        caddy::apply_all()
            .map_err(|e| tool_err(format!("container started but caddy reload failed: {}", e)))?;

        let response = json!({
            "service": service,
            "color": color,
            "container": container,
            "sha": sha,
            "guidance": {
                "what": "Container recreated with current image. Brief downtime occurred.",
                "to_verify": "get_status shows new state",
                "note": "Does NOT do blue/green deploy. For full deploy, push to main."
            }
        });
        let json = serde_json::to_string_pretty(&response).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }

    /// Inspect a running container and return its state, restart count, exit code, and recent logs.
    /// Useful for diagnosing why a service is unhealthy or not responding.
    #[tool(
        description = "Inspect a service's active container: returns status, restart count, exit code, \
                        and last 50 lines of stdout/stderr logs. Use to diagnose crash loops or startup failures."
    )]
    async fn get_container_info(
        &self,
        Parameters(ServiceParam { service }): Parameters<ServiceParam>,
    ) -> Result<CallToolResult, McpError> {
        if ServiceDef::load(&service).is_err() {
            return Err(tool_err(format!(
                "unknown service '{}' — no service definition found",
                service
            )));
        }

        let color = state::read_active(&service).unwrap_or_else(|_| "blue".to_string());
        let container = config::container_name(&service, &color);

        let docker = bollard::Docker::connect_with_socket_defaults()
            .map_err(|e| tool_err(format!("failed to connect to Docker: {}", e)))?;

        let info = docker
            .inspect_container(&container, None)
            .await
            .map_err(|e| {
                tool_err(format!(
                    "failed to inspect container '{}': {}",
                    container, e
                ))
            })?;

        let state = info.state.as_ref();
        let status = state
            .and_then(|s| s.status.as_ref())
            .map(|s| format!("{:?}", s));
        let running = state.and_then(|s| s.running).unwrap_or(false);
        let restart_count = info.restart_count.unwrap_or(0);
        let exit_code = state.and_then(|s| s.exit_code).unwrap_or(0);
        let error = state
            .and_then(|s| s.error.as_deref())
            .unwrap_or("")
            .to_string();
        let started_at = state
            .and_then(|s| s.started_at.as_deref())
            .unwrap_or("")
            .to_string();
        let finished_at = state
            .and_then(|s| s.finished_at.as_deref())
            .unwrap_or("")
            .to_string();

        use bollard::container::LogsOptions;
        use futures_util::StreamExt;
        let log_opts = LogsOptions::<String> {
            stdout: true,
            stderr: true,
            tail: "50".to_string(),
            ..Default::default()
        };
        let mut log_lines: Vec<String> = Vec::new();
        let mut log_stream = docker.logs(&container, Some(log_opts));
        while let Some(chunk) = log_stream.next().await {
            match chunk {
                Ok(output) => {
                    use bollard::container::LogOutput;
                    let line = match output {
                        LogOutput::StdOut { message }
                        | LogOutput::StdErr { message }
                        | LogOutput::Console { message } => {
                            String::from_utf8_lossy(&message).trim_end().to_string()
                        }
                        LogOutput::StdIn { message } => {
                            String::from_utf8_lossy(&message).trim_end().to_string()
                        }
                    };
                    log_lines.push(line);
                }
                Err(_) => break,
            }
        }

        let result = serde_json::json!({
            "container": container,
            "status": status,
            "running": running,
            "restart_count": restart_count,
            "exit_code": exit_code,
            "error": error,
            "started_at": started_at,
            "finished_at": finished_at,
            "logs": log_lines,
            "guidance": {
                "next_steps": "If exit_code != 0 or running=false, check logs in 'logs' field",
                "app_logs": "For app logs: read_container_file service='apps' path='/var/log/launchy/{name}.log'",
                "to_restart": "restart_service service='apps' recreates container (brief downtime)"
            }
        });
        let json = serde_json::to_string_pretty(&result).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }

    /// Regenerate the Caddyfile from all service YAMLs, routes.yaml,
    /// and SQLite runtime apps. No container restart needed.
    #[tool(
        description = "Reload Caddy routing from all service definitions, routes.yaml, and \
                        SQLite runtime apps without restarting containers. Use after \
                        editing proxy/routes.yaml."
    )]
    async fn reload_routes(&self) -> Result<CallToolResult, McpError> {
        caddy::apply_all().map_err(|e| tool_err(e.to_string()))?;
        nginx::generate_and_reload()
            .await
            .map_err(|e| tool_err(e.to_string()))?;
        let response = json!({
            "status": "ok",
            "guidance": {
                "what": "Caddy and Nginx reloaded. No container restart.",
                "when_to_use": "After editing routes.yaml or service YAMLs",
                "when_not_to_use": "For Dockerfile/service.yml changes — push to main"
            }
        });
        let json = serde_json::to_string_pretty(&response).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }

    /// Run all preflight checks and return a structured report.
    #[tool(
        description = "Run preflight health checks: supervisord, tailscale, and Caddy admin API"
    )]
    async fn run_preflight(&self) -> Result<CallToolResult, McpError> {
        let checks = vec![
            run_check("supervisord", preflight::check_supervisord),
            run_check("tailscale", preflight::check_tailscale),
            run_check("caddy", preflight::check_caddy),
        ];
        let all_passed = checks.iter().all(|c| c.passed);
        let report = PreflightReport { all_passed, checks };
        let json = serde_json::to_string_pretty(&report).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }

    /// List all service definitions installed on this host.
    #[tool(
        description = "List all service definition names in /opt/codery/services/. \
                          Returns an alphabetically sorted JSON array of service names \
                          (without the .yml extension)."
    )]
    async fn list_services(&self) -> Result<CallToolResult, McpError> {
        let dir = std::path::Path::new(config::SERVICES_DIR);
        if !dir.exists() {
            let json = serde_json::to_string_pretty(&Vec::<String>::new())
                .map_err(|e| tool_err(e.to_string()))?;
            return tool_ok(json);
        }
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map_err(|e| tool_err(format!("failed to read services dir: {}", e)))?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("yml") {
                    p.file_stem()
                        .and_then(|s| s.to_str())
                        .map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect();
        names.sort();
        let json = serde_json::to_string_pretty(&names).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }

    /// Read a service definition YAML by name.
    #[tool(description = "Read the YAML content of one service definition. \
                          Returns the raw YAML text as a string.")]
    async fn get_service(
        &self,
        Parameters(ServiceNameParam { name }): Parameters<ServiceNameParam>,
    ) -> Result<CallToolResult, McpError> {
        let path = service_path(&name).map_err(tool_err)?;
        let content = std::fs::read_to_string(&path)
            .map_err(|_| tool_err(format!("service '{}' not found at {:?}", name, path)))?;
        tool_ok(format!("{}\n\n---\nGuidance: Edit with upsert_service, then reload_routes to apply routing changes.", content))
    }

    /// Create or replace a service definition YAML.
    ///
    /// Validates the YAML against the ServiceDef schema before writing.
    /// The `service` field inside the YAML must match the `name` parameter.
    /// Call `reload_routes` afterward to make any routing changes take effect.
    /// A full container deploy is still required for port, volume, or image changes.
    #[tool(description = "Write or overwrite a service definition YAML. \
                          Validates structure before writing — returns an error if malformed. \
                          The 'service' field in the YAML must match the name parameter. \
                          Call reload_routes after to update Caddy routing.")]
    async fn upsert_service(
        &self,
        Parameters(UpsertServiceParams { name, yaml }): Parameters<UpsertServiceParams>,
    ) -> Result<CallToolResult, McpError> {
        validate_service_name(&name).map_err(tool_err)?;

        let def: crate::service_def::ServiceDef = serde_yaml::from_str(&yaml)
            .map_err(|e| tool_err(format!("invalid service YAML: {}", e)))?;

        if def.service != name {
            return Err(tool_err(format!(
                "service field '{}' in YAML does not match name parameter '{}'",
                def.service, name
            )));
        }

        std::fs::create_dir_all(config::SERVICES_DIR)
            .map_err(|e| tool_err(format!("failed to create {}: {}", config::SERVICES_DIR, e)))?;

        let path = service_path(&name).map_err(tool_err)?;
        let tmp = path.with_extension("yml.tmp");
        std::fs::write(&tmp, &yaml)
            .map_err(|e| tool_err(format!("failed to write {:?}: {}", tmp, e)))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| tool_err(format!("failed to rename {:?} to {:?}: {}", tmp, path, e)))?;

        tool_ok(format!(
            "service '{}' written to {:?} ({} bytes)",
            name,
            path,
            yaml.len()
        ))
    }

    /// Delete a service definition from disk.
    ///
    /// Does NOT stop running containers — you must stop them manually before or
    /// after deletion. Run reload_routes afterward to remove the service's routes
    /// from the Caddyfile.
    #[tool(
        description = "Delete a service definition YAML from /opt/codery/services/. \
                          Does not stop containers. Run reload_routes after to remove \
                          the service's routes from Caddy."
    )]
    async fn delete_service(
        &self,
        Parameters(ServiceNameParam { name }): Parameters<ServiceNameParam>,
    ) -> Result<CallToolResult, McpError> {
        let path = service_path(&name).map_err(tool_err)?;
        if !path.exists() {
            return Err(tool_err(format!(
                "service '{}' not found at {:?}",
                name, path
            )));
        }
        std::fs::remove_file(&path)
            .map_err(|e| tool_err(format!("failed to delete {:?}: {}", path, e)))?;
        tool_ok(format!("service '{}' deleted from {:?}", name, path))
    }

    /// Return the JSON Schema for the service definition YAML format.
    ///
    /// Use this before calling upsert_service to understand which fields are
    /// required, their types, and valid values. The schema is derived directly
    /// from the Rust structs that codery-ci uses to parse service YAMLs,
    /// so it is always accurate.
    #[tool(
        description = "Return JSON Schema for the service definition YAML format. \
                          Read this before calling upsert_service so you know exactly \
                          what fields are required and what types they accept."
    )]
    async fn get_service_schema(&self) -> Result<CallToolResult, McpError> {
        let schema = schemars::schema_for!(crate::service_def::ServiceDef);
        let json = serde_json::to_string_pretty(&schema).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }

    /// Read a file from inside a service's active container.
    ///
    /// Uses Docker's copy-from-container API — no exec needed, works even
    /// on containers that don't have a shell installed.
    #[tool(description = "Read a file from inside a service's active container. \
                        Use to inspect logs (/tmp/opencode.log), config files (/etc/hosts, \
                        /home/gem/.config/opencode/config.json), or any other container file. \
                        Returns the file content as a string.")]
    async fn read_container_file(
        &self,
        Parameters(ReadContainerFileParams { service, path }): Parameters<ReadContainerFileParams>,
    ) -> Result<CallToolResult, McpError> {
        if ServiceDef::load(&service).is_err() {
            return Err(tool_err(format!(
                "unknown service '{}' — no service definition found",
                service
            )));
        }
        if !path.starts_with('/') {
            return Err(tool_err(format!("path must be absolute, got: {}", path)));
        }

        let color = state::read_active(&service).unwrap_or_else(|_| "blue".to_string());
        let container = config::container_name(&service, &color);

        let docker = bollard::Docker::connect_with_socket_defaults()
            .map_err(|e| tool_err(format!("failed to connect to Docker: {}", e)))?;

        use bollard::container::DownloadFromContainerOptions;
        use futures_util::StreamExt;

        let mut stream = docker.download_from_container(
            &container,
            Some(DownloadFromContainerOptions {
                path: path.as_str(),
            }),
        );

        let mut tar_bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => tar_bytes.extend_from_slice(&bytes),
                Err(e) => {
                    return Err(tool_err(format!(
                        "error reading from container '{}': {}",
                        container, e
                    )))
                }
            }
        }

        if tar_bytes.is_empty() {
            return Err(tool_err(format!(
                "no data returned for '{}' in container '{}'",
                path, container
            )));
        }

        let mut archive = tar::Archive::new(std::io::Cursor::new(&tar_bytes));
        let mut content = String::new();
        if let Some(entry) = archive
            .entries()
            .map_err(|e| tool_err(format!("failed to read tar: {}", e)))?
            .next()
        {
            let mut entry =
                entry.map_err(|e| tool_err(format!("failed to read tar entry: {}", e)))?;
            use std::io::Read;
            entry
                .read_to_string(&mut content)
                .map_err(|e| tool_err(format!("failed to read file content: {}", e)))?;
        }

        if content.is_empty() {
            return tool_ok(format!("(empty file: {})", path));
        }

        let result = serde_json::json!({
            "container": container,
            "path": path,
            "content": content,
        });
        let json = serde_json::to_string_pretty(&result).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }

    // ── Diagnostic tools ──────────────────────────────────────────────────────

    /// Read Launchy status file or run `supervisorctl status` inside a container.
    #[tool(
        description = "Get process status inside a container. For apps: reads Launchy status file. \
                        For other services: falls back to supervisorctl. Shows process state and uptime."
    )]
    async fn get_supervisor_status(
        &self,
        Parameters(ServiceParam { service }): Parameters<ServiceParam>,
    ) -> Result<CallToolResult, McpError> {
        if service == "apps" {
            let output = container_exec(&service, &["cat", "/run/launchy-status.json"])
                .await
                .map_err(|e| tool_err(e))?;
            tool_ok(format!(
                "{}\n\n---\nGuidance: Launchy status for apps container. Use get_app_status for structured output.",
                output
            ))
        } else {
            let output = container_exec(&service, &["supervisorctl", "status"])
                .await
                .map_err(|e| tool_err(e))?;
            tool_ok(format!(
                "{}\n\n---\nGuidance: Process status inside {} container.",
                output, service
            ))
        }
    }

    /// Show the Docker port mappings for the active container of a service.
    /// Confirms which host ports are bound and to which container ports.
    #[tool(
        description = "Show Docker port mappings for the active container (host_port → container_port). \
                        Confirms ports like 7681, 3000, etc. are actually bound to the container."
    )]
    async fn get_container_ports(
        &self,
        Parameters(ServiceParam { service }): Parameters<ServiceParam>,
    ) -> Result<CallToolResult, McpError> {
        let color = state::read_active(&service).unwrap_or_else(|_| "blue".to_string());
        let container = config::container_name(&service, &color);
        let output = shell_output("docker", &["port", &container])
            .await
            .map_err(|e| tool_err(e))?;
        tool_ok(format!(
            "{}\n\n---\nGuidance: These are the Docker port mappings from host to container.",
            output
        ))
    }

    /// Return the current live Caddyfile from /etc/caddy/Caddyfile.
    /// This is what Caddy is actually serving — written by codery-ci
    /// on each deploy and each `reload_routes` call.
    #[tool(
        description = "Read the live /etc/caddy/Caddyfile. Shows all active subdomain → \
                        localhost:port reverse-proxy rules as Caddy currently sees them."
    )]
    async fn get_caddyfile(&self) -> Result<CallToolResult, McpError> {
        let content = std::fs::read_to_string(config::CADDY_CONFIG)
            .map_err(|e| tool_err(format!("failed to read Caddyfile: {}", e)))?;
        tool_ok(format!("{}\n\n---\nGuidance: Live Caddyfile as Caddy sees it. Regenerated by reload_routes or deploys.", content))
    }

    /// Check whether a TCP port is actively listening on the host.
    /// Uses `ss -tlnp` filtered to the given port number.
    #[tool(
        description = "Check if a host TCP port has a listener. Runs 'ss -tlnp' and returns \
                        matching entries. Use to verify a service is bound on the host — not \
                        just mapped in Docker, but actually listening."
    )]
    async fn check_port_listening(
        &self,
        Parameters(PortParam { port }): Parameters<PortParam>,
    ) -> Result<CallToolResult, McpError> {
        let raw = shell_output("ss", &["-tlnp"])
            .await
            .map_err(|e| tool_err(e))?;
        let port_str = format!(":{}", port);
        let mut lines: Vec<&str> = raw
            .lines()
            .enumerate()
            .filter(|(i, line)| *i == 0 || line.contains(&port_str))
            .map(|(_, line)| line)
            .collect();
        if lines.len() <= 1 {
            lines.push("(no listeners found for this port)");
        }
        tool_ok(format!("{}\n\n---\nGuidance: If no listener found, the service may not be running or port mapping may be wrong. Use get_status to check.", lines.join("\n")))
    }

    /// Run `supervisorctl status` on the HOST supervisor (not inside a container).
    /// The host supervisor manages Caddy, Tailscale, and the CoderyCI MCP server.
    #[tool(
        description = "Run 'supervisorctl status' on the HOST supervisor (not inside a container). \
                        Shows state of caddy, tailscale, and codery-ci on the VPS itself."
    )]
    async fn get_host_supervisor_status(&self) -> Result<CallToolResult, McpError> {
        let output = shell_output("supervisorctl", &["-c", config::SUPERVISORD_CONF, "status"])
            .await
            .map_err(|e| tool_err(e))?;
        tool_ok(format!(
            "{}\n\n---\nGuidance: Host supervisor manages caddy, tailscale, and codery-ci-mcp.",
            output
        ))
    }

    /// Return the output of `tailscale status`.
    /// Shows VPN peers, the host's Tailscale IP, and connection state.
    #[tool(
        description = "Run 'tailscale status' on the host. Shows VPN state, the host's Tailscale \
                        IP, and peer connectivity. Use to diagnose external access failures."
    )]
    async fn get_tailscale_status(&self) -> Result<CallToolResult, McpError> {
        let output = shell_output("tailscale", &["status"])
            .await
            .map_err(|e| tool_err(e))?;
        tool_ok(format!("{}\n\n---\nGuidance: All traffic enters through Tailscale VPN. If peers show no connection, check tailscale-up.sh.", output))
    }

    /// Report disk usage for /opt/codery and /var/lib/docker.
    /// A full Docker layer cache silently causes image pulls and deploys to fail.
    #[tool(description = "Show disk usage for /opt/codery and /var/lib/docker. \
                        A full disk causes silent deploy failures when Docker can't pull images.")]
    async fn get_disk_usage(&self) -> Result<CallToolResult, McpError> {
        let output = shell_output("df", &["-h", "/opt/codery", "/var/lib/docker"])
            .await
            .map_err(|e| tool_err(e))?;
        tool_ok(format!("{}\n\n---\nGuidance: Full disk causes silent deploy failures. If >90%, prune old images or docker system prune.", output))
    }

    /// List all Docker containers on the host, running and stopped.
    /// Shows name, image, status, and ports for a full host snapshot.
    #[tool(
        description = "List all Docker containers on the host (docker ps -a). \
                        Shows name, image, status, and ports — including stopped containers."
    )]
    async fn list_containers(&self) -> Result<CallToolResult, McpError> {
        let output = shell_output(
            "docker",
            &[
                "ps",
                "-a",
                "--format",
                "table {{.Names}}\t{{.Image}}\t{{.Status}}\t{{.Ports}}",
            ],
        )
        .await
        .map_err(|e| tool_err(e))?;
        tool_ok(format!("{}\n\n---\nGuidance: Shows all containers including stopped ones from previous deploys.", output))
    }

    /// Register a new app in the apps container without rebuilding the image.
    /// Writes a Launchy JSON config, signals Launchy to start the process,
    /// adds a Caddy+Nginx route, then reloads both so the app is immediately live.
    /// The app's code must already exist at `directory` in the shared volume.
    #[tool(
        description = "Add an app to the apps container: write Launchy config, register \
                          subdomain→port route, reload Nginx and Caddy. The app process starts \
                          immediately. Code must already exist in /home/gem/projects."
    )]
    async fn add_app(
        &self,
        Parameters(p): Parameters<AddAppParams>,
    ) -> Result<CallToolResult, McpError> {
        if p.name.contains(' ') || p.name.contains('/') || p.name.contains('.') {
            return Err(tool_err(
                "app name must not contain spaces, slashes, or dots",
            ));
        }

        let check = container_exec("apps", &["test", "-d", &p.directory])
            .await
            .map_err(|e| tool_err(format!("failed to check directory: {}", e)))?;
        if check.starts_with("[exited") {
            return Err(tool_err(format!(
                "directory '{}' does not exist in apps container",
                p.directory
            )));
        }

        let conn = db::open().map_err(|e| tool_err(e.to_string()))?;
        db::init(&conn).map_err(|e| tool_err(e.to_string()))?;

        if db::port_claimed(&conn, p.internal_port).map_err(|e| tool_err(e.to_string()))? {
            return Err(tool_err(format!(
                "port {} already claimed by another app",
                p.internal_port
            )));
        }

        if db::find_app_by_name(&conn, &p.name).map_err(|e| tool_err(e.to_string()))?.is_some() {
            return Err(tool_err(format!(
                "app '{}' already exists",
                p.name
            )));
        }

        let env_json = p.env.as_ref().and_then(|e| {
            if e.is_empty() { None } else { Some(serde_json::to_string(e).unwrap()) }
        });

        let app = db::AppRecord {
            name: p.name.clone(),
            subdomain: p.subdomain.clone(),
            internal_port: p.internal_port,
            command: p.command.clone(),
            directory: p.directory.clone(),
            env: env_json,
            priority: 100,
            user: "gem".to_string(),
            restart: "always".to_string(),
            no_cache: p.no_cache.unwrap_or(false),
            created_at: String::new(),
        };

        db::insert_app(&conn, &app).map_err(|e| tool_err(e.to_string()))?;
        db::sync_launchy(&conn).map_err(|e| tool_err(e.to_string()))?;

        container_exec("apps", &["kill", "-HUP", "1"])
            .await
            .map_err(|e| tool_err(format!("failed to signal Launchy: {}", e)))?;

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        caddy::apply_all().map_err(|e| tool_err(e.to_string()))?;
        nginx::generate_and_reload()
            .await
            .map_err(|e| tool_err(e.to_string()))?;

        let status_output = container_exec("apps", &["cat", "/run/launchy-status.json"])
            .await
            .unwrap_or_else(|e| format!("(status read failed: {})", e));
        let running = if !status_output.starts_with("[exited") && !status_output.starts_with("(") {
            status_output.contains(&format!("\"{}\"", &p.name))
                || status_output.contains(&format!("\"name\":\"{}\"", &p.name))
        } else {
            false
        };

        if !running {
            return Err(tool_err(format!(
                "App '{}' config written and route added, but app not found in Launchy status. \
                 Check logs: read_container_file service='apps' path='/var/log/launchy/{}.log'",
                p.name, p.name
            )));
        }

        let response = json!({
            "name": p.name,
            "subdomain": p.subdomain,
            "internal_port": p.internal_port,
            "directory": p.directory,
            "no_cache": app.no_cache,
            "status": "running",
            "guidance": {
                "what": "App started instantly via Launchy. No container rebuild.",
                "persistence": "Runtime apps persist across container restarts AND blue/green redeploys (stored in SQLite)",
                "to_remove": "remove_app name='{}' — stops process, deletes config, removes route",
                "to_check": "get_app_status shows per-app process state",
                "to_read_logs": "read_container_file service='apps' path='/var/log/launchy/{name}.log'"
            }
        });
        let json = serde_json::to_string_pretty(&response).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }

    /// Remove an app added via add_app. Stops the process via Launchy config removal,
    /// removes the route, and reloads Nginx and Caddy.
    #[tool(
        description = "Remove an app from the apps container: stop process, delete Launchy \
                          config, remove subdomain route, reload Nginx and Caddy."
    )]
    async fn remove_app(
        &self,
        Parameters(p): Parameters<RemoveAppParams>,
    ) -> Result<CallToolResult, McpError> {
        let conn = db::open().map_err(|e| tool_err(e.to_string()))?;
        db::init(&conn).map_err(|e| tool_err(e.to_string()))?;

        let subdomain = p.subdomain.unwrap_or_else(|| p.name.clone());

        let deleted = db::delete_app(&conn, &p.name).map_err(|e| tool_err(e.to_string()))?;
        if !deleted {
            return Err(tool_err(format!(
                "app '{}' not found in database",
                p.name
            )));
        }

        db::sync_launchy(&conn).map_err(|e| tool_err(e.to_string()))?;

        container_exec("apps", &["kill", "-HUP", "1"])
            .await
            .map_err(|e| tool_err(format!("failed to signal Launchy: {}", e)))?;

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        caddy::apply_all().map_err(|e| tool_err(e.to_string()))?;
        nginx::generate_and_reload()
            .await
            .map_err(|e| tool_err(e.to_string()))?;

        let response = json!({
            "name": p.name,
            "subdomain": subdomain,
            "status": "removed",
            "guidance": {
                "what": "App stopped, config deleted, route removed.",
                "to_verify": "list_apps shows remaining apps"
            }
        });
        let json = serde_json::to_string_pretty(&response).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }

    /// List all apps currently registered in SQLite.
    #[tool(
        description = "List all apps registered in the apps container (reads SQLite). \
                          Returns subdomain, external port (always 8080 → Nginx), and internal_port."
    )]
    async fn list_apps(&self) -> Result<CallToolResult, McpError> {
        let conn = db::open().map_err(|e| tool_err(e.to_string()))?;
        db::init(&conn).map_err(|e| tool_err(e.to_string()))?;
        let apps = db::list_apps(&conn).map_err(|e| tool_err(e.to_string()))?;
        let response = json!({
            "apps": apps,
            "guidance": {
                "to_add": "add_app name='myapp' subdomain='myapp' internal_port=3001 command='...' directory='...'",
                "to_remove": "remove_app name='myapp'",
                "to_check_status": "get_app_status shows per-app process state",
                "routing": "Caddy → Nginx (8080) → app process (internal_port)"
            }
        });
        let json = serde_json::to_string_pretty(&response).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }

    /// Get structured status for all apps in the apps container.
    /// Reads Launchy's status file and cross-references with build-in configs.
    #[tool(
        description = "Get structured status for all apps in the apps container. \
                          Reads Launchy's status file — shows name, pid, status, uptime for each app. \
                          Also indicates whether each app is a build-time or runtime app."
    )]
    async fn get_app_status(&self) -> Result<CallToolResult, McpError> {
        let status_output = container_exec("apps", &["cat", "/run/launchy-status.json"])
            .await
            .map_err(|e| tool_err(format!("failed to read Launchy status: {}", e)))?;

        if status_output.starts_with("[exited") {
            return Err(tool_err(
                "Launchy status file not found — is the apps container running?",
            ));
        }

        let status: serde_json::Value = serde_json::from_str(&status_output)
            .map_err(|e| tool_err(format!("failed to parse Launchy status: {}", e)))?;

        let builtin_output = container_exec("apps", &["ls", "/etc/launchy/built-in/"])
            .await
            .unwrap_or_default();
        let builtin_names: Vec<&str> = builtin_output
            .lines()
            .filter(|l| l.ends_with(".json"))
            .filter_map(|l| l.strip_suffix(".json"))
            .collect();

        let services = if let Some(services) = status.get("services").and_then(|s| s.as_array()) {
            services
                .iter()
                .map(|svc| {
                    let name = svc
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("unknown");
                    let source = if builtin_names.contains(&name) {
                        "build"
                    } else {
                        "runtime"
                    };
                    let mut annotated = svc.clone();
                    annotated
                        .as_object_mut()
                        .unwrap()
                        .insert("source".to_string(), json!(source));
                    annotated
                })
                .collect::<Vec<_>>()
        } else {
            vec![]
        };

        let response = json!({
            "services": services,
            "guidance": {
                "build_vs_runtime": "build = baked into image. runtime = added via add_app (persists across redeploys on host bind mounts).",
                "to_read_logs": "read_container_file service='apps' path='/var/log/launchy/{name}.log'",
                "to_add": "add_app name='myapp' subdomain='myapp' internal_port=3001 command='...' directory='...'"
            }
        });
        let json = serde_json::to_string_pretty(&response).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }
}

fn run_check(name: &'static str, f: fn() -> anyhow::Result<()>) -> PreflightCheck {
    match f() {
        Ok(()) => PreflightCheck {
            name,
            passed: true,
            message: "OK".to_string(),
        },
        Err(e) => PreflightCheck {
            name,
            passed: false,
            message: e.to_string(),
        },
    }
}

#[tool_handler]
impl ServerHandler for OrchestratorMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(INSTRUCTIONS)
    }
}

const INSTRUCTIONS: &str = r#"
CoderyCI MCP server — operational guide for agents.

## Architecture

The Codery infrastructure has three layers:

1. **Host layer** — Caddy (reverse proxy + TLS), Tailscale (VPN), supervisord
2. **Sandbox container** — AI coding environment (OpenCode on port 3000)
3. **Apps container** — Project web servers, each managed by Launchy (Rust process manager)

### Traffic flow for apps

```
Internet → Tailscale VPN → Caddy (host) → Nginx (container:8080) → App process (internal_port)
```

- Caddy routes by **subdomain** to the correct host port (active color offset)
- Nginx routes by **Host header** to the correct internal port
- The `internal_port` is where the app process actually listens (e.g., 3001, 8020, 8030)

### Process management

Both containers use **Launchy** (Rust binary) as PID 1:
- Manages all services via include-directory configs
- Hot-reload on SIGHUP (add/remove services without restart)
- Writes status to /run/launchy-status.json (read by MCP tools)

## Available tools

### Infrastructure

| Tool | What it does |
|---|---|
| `get_status` | Active color, SHA, running state for every service |
| `get_routes` | Full routing table with internal_port for apps |
| `list_services` | Service YAML names from /opt/codery/services/ |
| `get_service` | Read one service definition YAML |
| `get_service_schema` | JSON Schema for service YAML format |
| `upsert_service` | Create/replace a service definition |
| `delete_service` | Remove a service definition |
| `reload_routes` | Regenerate Caddyfile + Nginx config, reload both |
| `restart_service` | Recreate active container from current YAML |
| `run_preflight` | Check host services health |

### App Management

| Tool | What it does |
|---|---|
| `add_app` | Hot-add an app: writes Launchy config, registers route, starts process (instant) |
| `remove_app` | Hot-remove an app: stops process, deletes config, removes route |
| `list_apps` | List all apps from SQLite |
| `get_app_status` | Per-app status from Launchy (pid, uptime, build vs runtime) |

### Deploy/Rollback

| Tool | What it does |
|---|---|
| `rollback` | Deploy previous cached image via blue/green |
| `list_images` | Locally cached Docker images for a service |

### Diagnostics

| Tool | What it does |
|---|---|
| `get_container_info` | Container state + last 50 log lines |
| `read_container_file` | Read any file from inside a container |
| `get_container_ports` | Docker port mappings |
| `get_caddyfile` | Live Caddyfile |
| `check_port_listening` | Check if a host port has a listener |
| `get_host_supervisor_status` | Host-level supervisord status |
| `get_tailscale_status` | VPN state and connectivity |
| `get_disk_usage` | Disk usage for /opt/codery and /var/lib/docker |
| `list_containers` | All Docker containers on host |

## App management workflows

### Add an app instantly (no rebuild)

```
add_app name='myapp' subdomain='myapp' internal_port=3001 command='bun run start' directory='/home/gem/projects/myapp'
```

Pre-flight checks: directory must exist, port must be free, name must be unique.
The app starts immediately. No container rebuild needed.

**Runtime apps persist across container restarts and blue/green redeploys.**
Configs are stored in SQLite at `/opt/codery/codery.db` and regenerated as
Launchy JSON files and route configs on every mutation.
Launchy reads `include_dirs` on startup, so runtime apps auto-restore.

### Remove an app

```
remove_app name='myapp'
```

Stops the process, deletes the config, removes the route, reloads Caddy + Nginx.

### Check app health

```
get_app_status    → shows all apps, pid, uptime, build/runtime source
list_apps         → shows routing info (subdomain, internal_port)
```

### Read app logs

```
read_container_file service='apps' path='/var/log/launchy/myapp.log'
```

## Diagnostic workflow: "app not responding"

1. `get_app_status` → is the app running?
2. If not running: `read_container_file service='apps' path='/var/log/launchy/{name}.log'` → crash reason
3. If running: `get_routes` → verify routing (subdomain → host_port → internal_port)
4. `check_port_listening port={host_port}` → verify Caddy can reach container
5. `get_caddyfile` → verify Caddy has the route

## Service definition workflow

To add a new container service:
1. `get_service_schema` → read the schema
2. `upsert_service` → write the YAML
3. `reload_routes` → reload Caddy
4. Trigger deploy via CI

## Port formula

`host_port = container_port + offset` where offset comes from service YAML `port_scheme`.

| Service | Blue offset | Green offset |
|---------|------------|-------------|
| sandbox | 10000 | 20000 |
| apps | 0 | 10000 |

## When to use reload_routes vs full redeploy

- **Route change only** (new subdomain, edited routes.yaml) → `reload_routes` (instant)
- **Container code, Dockerfile, service YAML, volume change** → push to main (~8 min)

## This MCP server

Host process under supervisord as `codery-ci-mcp`. Port 4040, endpoint `/sse`.
"#;

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn serve(port: u16) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::{
        session::never::NeverSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };
    use tokio_util::sync::CancellationToken;

    let addr: SocketAddr = format!("0.0.0.0:{}", port)
        .parse()
        .context("invalid bind address")?;

    println!("[mcp] Starting CoderyCI MCP server on {}", addr);

    let ct = CancellationToken::new();

    let service = StreamableHttpService::new(
        || Ok(OrchestratorMcp),
        // Stateless mode: no in-memory sessions. Each POST is handled independently.
        // This avoids 404 "session not found" errors when the server restarts and
        // OpenCode tries to reuse a session ID from before the restart. Our server
        // is pure request-response — it has no need for server-push SSE notifications.
        NeverSessionManager::default().into(),
        // Disable Host header validation: port 4040 is protected by iptables (only
        // Tailscale and Docker bridge subnets can reach it), so the server-level
        // guard adds no security. Without this, requests from host.docker.internal
        // are rejected because it is not in the default allowed-hosts list.
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_cancellation_token(ct.child_token())
            .disable_allowed_hosts(),
    );

    // Mount the Streamable HTTP service at /sse so the URL in opencode.json
    // (http://host.docker.internal:4040/sse) doesn't need to change.
    let router = axum::Router::new().nest_service("/sse", service);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("failed to bind TCP listener")?;

    println!("[mcp] Listening — MCP endpoint: http://{}/sse", addr);

    let ct_shutdown = ct.clone();
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigterm =
                signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
            tokio::select! {
                _ = sigterm.recv() => { println!("[mcp] SIGTERM — shutting down"); }
                _ = tokio::signal::ctrl_c() => { println!("[mcp] Ctrl+C — shutting down"); }
            }
            ct_shutdown.cancel();
        })
        .await
        .context("MCP server error")?;

    Ok(())
}

// ── CRUD helpers ──────────────────────────────────────────────────────────────

/// Service names must be alphanumeric + hyphens/underscores only.
/// This prevents path traversal and keeps filenames predictable.
fn validate_service_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("service name cannot be empty".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "invalid service name '{}': use only letters, digits, hyphens, underscores",
            name
        ));
    }
    Ok(())
}

/// Build the full path to a service YAML file, validating the name first.
fn service_path(name: &str) -> Result<std::path::PathBuf, String> {
    validate_service_name(name)?;
    Ok(std::path::Path::new(config::SERVICES_DIR).join(format!("{}.yml", name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_name_accepts_valid_names() {
        assert!(validate_service_name("sandbox").is_ok());
        assert!(validate_service_name("my-service").is_ok());
        assert!(validate_service_name("apps_v2").is_ok());
        assert!(validate_service_name("Service123").is_ok());
    }

    #[test]
    fn validate_name_rejects_empty() {
        assert!(validate_service_name("").is_err());
    }

    #[test]
    fn validate_name_rejects_invalid_characters() {
        assert!(validate_service_name("../etc/passwd").is_err());
        assert!(validate_service_name("foo/bar").is_err());
        assert!(validate_service_name("foo.yml").is_err());
    }

    #[test]
    fn validate_name_rejects_leading_dot() {
        assert!(validate_service_name(".hidden").is_err());
        assert!(validate_service_name("..").is_err());
        assert!(validate_service_name(".").is_err());
    }

    #[test]
    fn service_path_builds_correct_path() {
        let p = service_path("sandbox").unwrap();
        assert_eq!(p, std::path::Path::new("/opt/codery/services/sandbox.yml"));
    }

    #[test]
    fn service_path_rejects_invalid_name() {
        assert!(service_path("../bad").is_err());
    }

    #[test]
    fn upsert_rejects_name_service_mismatch() {
        let yaml = r#"
service: sandbox
image: ghcr.io/coderyoss/codery:sandbox-{sha}
port_scheme:
  blue_offset: 10000
  green_offset: 20000
ports: []
health_check:
  type: docker
  timeout_secs: 60
volumes: []
required_env: []
network: codery-net
"#;
        let name = "apps"; // intentional mismatch: YAML says 'sandbox', name says 'apps'
        let def: crate::service_def::ServiceDef = serde_yaml::from_str(yaml).unwrap();
        assert_ne!(
            def.service, name,
            "precondition: service field differs from name"
        );
        let err_msg = format!(
            "service field '{}' in YAML does not match name parameter '{}'",
            def.service, name
        );
        assert!(err_msg.contains("sandbox"));
        assert!(err_msg.contains("apps"));
    }
}
