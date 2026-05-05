use std::collections::HashMap;
use std::net::SocketAddr;

use anyhow::Context;
use rmcp::{
    ErrorData as McpError,
    ServerHandler,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars,
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use schemars::JsonSchema;

use crate::{caddy, config, deploy, images, preflight, service_def::ServiceDef, state};

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
    service: String,
    color: Option<String>,
    note: Option<String>,
}

#[derive(Serialize)]
struct RoutingTable {
    services: HashMap<String, String>, // service → active color
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
    #[schemars(description = "Absolute path to file inside container, e.g. '/etc/hosts' or '/tmp/opencode.log'")]
    path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PortParam {
    #[schemars(description = "Host TCP port number to check (e.g. 17681)")]
    port: u16,
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
            info.and_then(|i| i.state).and_then(|s| s.running).unwrap_or(false)
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

        let json = serde_json::to_string_pretty(&statuses).map_err(|e| tool_err(e.to_string()))?;
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
        let mut routes: Vec<RouteEntry> = Vec::new();

        for def in &defs {
            let color = state::read_active(&def.service).unwrap_or_else(|_| "blue".to_string());
            services_map.insert(def.service.clone(), color.clone());

            // Named ports with subdomains (sandbox-style).
            for port in &def.ports {
                if let Some(subdomain) = &port.subdomain {
                    let fqdn = if subdomain.contains('.') {
                        subdomain.clone()
                    } else {
                        format!("{}.{}", subdomain, domain)
                    };
                    let host_port = def.port_scheme.host_port(&color, port.container_port);
                    routes.push(RouteEntry {
                        subdomain: fqdn,
                        host_port,
                        container_port: Some(port.container_port),
                        service: def.service.clone(),
                        color: Some(color.clone()),
                        note: None,
                    });
                }
            }

            // Routes file (apps-style).
            if let Some(routes_file) = &def.routes_file {
                if let Ok(data) = std::fs::read_to_string(routes_file) {
                    #[derive(serde::Deserialize)]
                    struct Row { subdomain: String, port: u16 }
                    if let Ok(rows) = serde_json::from_str::<Vec<Row>>(&data) {
                        for row in rows {
                            let fqdn = if row.subdomain.contains('.') {
                                row.subdomain
                            } else {
                                format!("{}.{}", row.subdomain, domain)
                            };
                            let host_port = def.port_scheme.host_port(&color, row.port);
                            routes.push(RouteEntry {
                                subdomain: fqdn,
                                host_port,
                                container_port: Some(row.port),
                                service: def.service.clone(),
                                color: Some(color.clone()),
                                note: None,
                            });
                        }
                    }
                }
            }
        }

        // MCP server itself — host process, no container.
        routes.push(RouteEntry {
            subdomain: config::mcp_host(&domain),
            host_port: config::MCP_PORT,
            container_port: None,
            service: "host".to_string(),
            color: None,
            note: Some("CoderyCI MCP API (this server)".to_string()),
        });

        let table = RoutingTable { services: services_map, routes };
        let json = serde_json::to_string_pretty(&table).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }

    /// List locally available Docker images for a service. Useful before rollback.
    #[tool(description = "List locally cached Docker images for a service (e.g. 'sandbox', 'apps'), newest first")]
    async fn list_images(
        &self,
        Parameters(ServiceKnownParam { service }): Parameters<ServiceKnownParam>,
    ) -> Result<CallToolResult, McpError> {
        if ServiceDef::load(&service).is_err() {
            return Err(tool_err(format!("unknown service '{}' — no service definition found", service)));
        }
        let imgs = images::list_local(&service).await.map_err(|e| tool_err(e.to_string()))?;
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
            return Err(tool_err(format!("unknown service '{}' — no service definition found", service)));
        }

        let active_sha = state::read_active_sha(&service);
        let imgs = images::list_local(&service).await.map_err(|e| tool_err(e.to_string()))?;

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

        tool_ok(format!(
            "Rollback complete. {} is now running sha={}",
            service, rollback_sha.sha
        ))
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
        let sha = state::read_active_sha(&service)
            .ok_or_else(|| tool_err(format!("no active SHA recorded for service '{}' — run a full deploy first", service)))?;

        let docker = bollard::Docker::connect_with_socket_defaults()
            .map_err(|e| tool_err(format!("failed to connect to Docker: {}", e)))?;

        deploy::remove_container_if_exists(&docker, &container)
            .await
            .map_err(|e| tool_err(format!("failed to remove container '{}': {}", container, e)))?;

        deploy::start_container(&docker, &def, &sha, &color)
            .await
            .map_err(|e| tool_err(format!("failed to start container '{}': {}", container, e)))?;

        caddy::apply_all().map_err(|e| tool_err(format!("container started but caddy reload failed: {}", e)))?;

        tool_ok(format!(
            "recreated {} ({} — container: {}, sha: {})",
            service, color, container, &sha[..12]
        ))
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
            return Err(tool_err(format!("unknown service '{}' — no service definition found", service)));
        }

        let color = state::read_active(&service).unwrap_or_else(|_| "blue".to_string());
        let container = config::container_name(&service, &color);

        let docker = bollard::Docker::connect_with_socket_defaults()
            .map_err(|e| tool_err(format!("failed to connect to Docker: {}", e)))?;

        let info = docker
            .inspect_container(&container, None)
            .await
            .map_err(|e| tool_err(format!("failed to inspect container '{}': {}", container, e)))?;

        let state = info.state.as_ref();
        let status = state.and_then(|s| s.status.as_ref()).map(|s| format!("{:?}", s));
        let running = state.and_then(|s| s.running).unwrap_or(false);
        let restart_count = info.restart_count.unwrap_or(0);
        let exit_code = state.and_then(|s| s.exit_code).unwrap_or(0);
        let error = state.and_then(|s| s.error.as_deref()).unwrap_or("").to_string();
        let started_at = state.and_then(|s| s.started_at.as_deref()).unwrap_or("").to_string();
        let finished_at = state.and_then(|s| s.finished_at.as_deref()).unwrap_or("").to_string();

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
                        LogOutput::StdOut { message } | LogOutput::StdErr { message } | LogOutput::Console { message } => {
                            String::from_utf8_lossy(&message).trim_end().to_string()
                        }
                        LogOutput::StdIn { message } => String::from_utf8_lossy(&message).trim_end().to_string(),
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
        });
        let json = serde_json::to_string_pretty(&result).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }

    /// Regenerate the Caddyfile from all service YAMLs and reload Caddy.
    /// No container restart needed.
    #[tool(
        description = "Reload Caddy routing from all service definitions and route JSON files \
                        without restarting containers. Use after editing proxy/apps-routes.json."
    )]
    async fn reload_routes(&self) -> Result<CallToolResult, McpError> {
        caddy::apply_all().map_err(|e| tool_err(e.to_string()))?;
        tool_ok("Routes reloaded from all service definitions".to_string())
    }

    /// Run all preflight checks and return a structured report.
    #[tool(description = "Run preflight health checks: supervisord, tailscale, and Caddy admin API")]
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
    #[tool(            description = "List all service definition names in /opt/codery/services/. \
                          Returns an alphabetically sorted JSON array of service names \
                          (without the .yml extension).")]
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
        tool_ok(content)
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
    #[tool(            description = "Delete a service definition YAML from /opt/codery/services/. \
                          Does not stop containers. Run reload_routes after to remove \
                          the service's routes from Caddy.")]
    async fn delete_service(
        &self,
        Parameters(ServiceNameParam { name }): Parameters<ServiceNameParam>,
    ) -> Result<CallToolResult, McpError> {
        let path = service_path(&name).map_err(tool_err)?;
        if !path.exists() {
            return Err(tool_err(format!("service '{}' not found at {:?}", name, path)));
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
    #[tool(description = "Return JSON Schema for the service definition YAML format. \
                          Read this before calling upsert_service so you know exactly \
                          what fields are required and what types they accept.")]
    async fn get_service_schema(&self) -> Result<CallToolResult, McpError> {
        let schema = schemars::schema_for!(crate::service_def::ServiceDef);
        let json = serde_json::to_string_pretty(&schema).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }

    /// Read a file from inside a service's active container.
    ///
    /// Uses Docker's copy-from-container API — no exec needed, works even
    /// on containers that don't have a shell installed.
    #[tool(
        description = "Read a file from inside a service's active container. \
                        Use to inspect logs (/tmp/opencode.log), config files (/etc/hosts, \
                        /home/gem/.config/opencode/config.json), or any other container file. \
                        Returns the file content as a string."
    )]
    async fn read_container_file(
        &self,
        Parameters(ReadContainerFileParams { service, path }): Parameters<ReadContainerFileParams>,
    ) -> Result<CallToolResult, McpError> {
        if ServiceDef::load(&service).is_err() {
            return Err(tool_err(format!("unknown service '{}' — no service definition found", service)));
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
            Some(DownloadFromContainerOptions { path: path.as_str() }),
        );

        let mut tar_bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => tar_bytes.extend_from_slice(&bytes),
                Err(e) => return Err(tool_err(format!("error reading from container '{}': {}", container, e))),
            }
        }

        if tar_bytes.is_empty() {
            return Err(tool_err(format!("no data returned for '{}' in container '{}'", path, container)));
        }

        let mut archive = tar::Archive::new(std::io::Cursor::new(&tar_bytes));
        let mut content = String::new();
        for entry in archive.entries().map_err(|e| tool_err(format!("failed to read tar: {}", e)))? {
            let mut entry = entry.map_err(|e| tool_err(format!("failed to read tar entry: {}", e)))?;
            use std::io::Read;
            entry.read_to_string(&mut content)
                .map_err(|e| tool_err(format!("failed to read file content: {}", e)))?;
            break;
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

    /// Run `supervisorctl status` inside the active container for a service.
    /// Shows whether each managed process (opencode, code-server, ttyd, etc.)
    /// is RUNNING, STOPPED, FATAL, or EXITED, with uptime or exit info.
    #[tool(
        description = "Run 'supervisorctl status' inside the active container for a service. \
                        Shows process state (RUNNING/STOPPED/FATAL) and uptime for each program \
                        managed by supervisord inside the container."
    )]
    async fn get_supervisor_status(
        &self,
        Parameters(ServiceParam { service }): Parameters<ServiceParam>,
    ) -> Result<CallToolResult, McpError> {
        let output = container_exec(&service, &["supervisorctl", "status"])
            .await
            .map_err(|e| tool_err(e))?;
        tool_ok(output)
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
        let color = state::read_active(&service)
            .unwrap_or_else(|_| "blue".to_string());
        let container = config::container_name(&service, &color);
        let output = shell_output("docker", &["port", &container])
            .await
            .map_err(|e| tool_err(e))?;
        tool_ok(output)
    }

    /// Return the current live Caddyfile from /etc/caddy/Caddyfile.
    /// This is what Caddy is actually serving — written by codery-ci
    /// on each deploy and each `reload_routes` call.
    #[tool(
        description = "Read the live /etc/caddy/Caddyfile. Shows all active subdomain → \
                        localhost:port reverse-proxy rules as Caddy currently sees them."
    )]
    async fn get_caddyfile(&self) -> Result<CallToolResult, McpError> {
        std::fs::read_to_string(config::CADDY_CONFIG)
            .map(|s| tool_ok(s).unwrap())
            .map_err(|e| tool_err(format!("failed to read Caddyfile: {}", e)))
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
        tool_ok(lines.join("\n"))
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
        tool_ok(output)
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
        tool_ok(output)
    }

    /// Report disk usage for /opt/codery and /var/lib/docker.
    /// A full Docker layer cache silently causes image pulls and deploys to fail.
    #[tool(
        description = "Show disk usage for /opt/codery and /var/lib/docker. \
                        A full disk causes silent deploy failures when Docker can't pull images."
    )]
    async fn get_disk_usage(&self) -> Result<CallToolResult, McpError> {
        let output = shell_output("df", &["-h", "/opt/codery", "/var/lib/docker"])
            .await
            .map_err(|e| tool_err(e))?;
        tool_ok(output)
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
            &["ps", "-a", "--format", "table {{.Names}}\t{{.Image}}\t{{.Status}}\t{{.Ports}}"],
        )
        .await
        .map_err(|e| tool_err(e))?;
        tool_ok(output)
    }
}

fn run_check(name: &'static str, f: fn() -> anyhow::Result<()>) -> PreflightCheck {
    match f() {
        Ok(()) => PreflightCheck { name, passed: true, message: "OK".to_string() },
        Err(e) => PreflightCheck { name, passed: false, message: e.to_string() },
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

## What this server controls

The Codery infrastructure runs two main containers (sandbox, apps) and the host layer
(Caddy reverse proxy, Tailscale VPN, supervisord). This MCP server lets you inspect
and operate the system without SSH access.

## Available tools

| Tool | What it does |
|---|---|
| `get_status` | Active color and deployed SHA for every service |
| `get_routes` | Full routing table: subdomain → host port → container port → service |
| `list_services` | List all service definitions installed on this host |
| `get_service` | Read one service definition YAML by name |
| `get_service_schema` | JSON Schema for the service YAML format — read before calling upsert_service |
| `upsert_service` | Create or replace a service definition (validates YAML before writing) |
| `delete_service` | Delete a service definition (does not stop containers) |
| `list_images` | Locally cached Docker images for a service (use before rollback) |
| `rollback` | Deploy the previous cached image via full blue/green deploy |
| `restart_service` | Recreate the active container from current service YAML — applies volume/env changes without a full deploy |
| `get_container_info` | Inspect container state, restart count, exit code, and last 50 log lines |
| `read_container_file` | Read a file from inside a container (logs, config, /etc/hosts, etc.) |
| `get_supervisor_status` | `supervisorctl status` inside the active container — per-process state |
| `get_container_ports` | Docker port mappings for the active container (host → container) |
| `get_caddyfile` | Live /etc/caddy/Caddyfile — what Caddy is actually serving right now |
| `check_port_listening` | Verify a host port has an active listener (`ss -tlnp`) |
| `get_host_supervisor_status` | `supervisorctl status` on the HOST — caddy, tailscale, mcp |
| `get_tailscale_status` | VPN state, Tailscale IP, peer connectivity |
| `get_disk_usage` | Disk usage for /opt/codery and /var/lib/docker — catches silent deploy failures |
| `list_containers` | All Docker containers on the host (running + stopped) |
| `reload_routes` | Regenerate Caddyfile from all service YAMLs + route JSON files, reload Caddy in-place |
| `run_preflight` | Check supervisord, Tailscale, and Caddy admin API health |

## Service definition workflow

To add a new service via MCP:
1. `get_service_schema` — read the schema so you know what's valid
2. `upsert_service` — write the YAML (validated before writing)
3. `reload_routes` — reload Caddy to pick up any new routes
4. Trigger a deploy via CI (`gh workflow run deploy-newservice.yml`) for the container itself

To modify routing only (no container change):
1. `upsert_service` — update the service YAML with new subdomain/port
2. `reload_routes` — reload Caddy; takes effect immediately, no container restart

To remove a service:
1. `get_status` — confirm active container and color
2. Stop the container: SSH to host and run `docker stop codery-<service>-<color>`
3. `delete_service` — remove the YAML from disk
4. `reload_routes` — regenerate Caddyfile without the removed service

## Port formula

`host_port = container_port + offset` where offset is `blue_offset` or `green_offset`
from the service YAML. Example — sandbox blue: `3000 + 10000 = 13000`.

## When to use reload_routes vs full redeploy

- Route JSON change only (new app subdomain, no code change) → `reload_routes`
- Container code, Dockerfile, supervisor config, or service YAML change → full deploy via CI

## The MCP server itself

This server is a host process (not a container). It runs under supervisord as `codery-ci-mcp`.
It listens on port 4040 and is served at `mcp.<domain>/sse`. The root path `/`
returns 404 by design — only `/sse` is a valid endpoint.
"#;

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn serve(port: u16) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
        session::never::NeverSessionManager,
    };
    use tokio_util::sync::CancellationToken;

    let addr: SocketAddr = format!("0.0.0.0:{}", port)
        .parse()
        .context("invalid bind address")?;

    println!("[mcp] Starting CoderyCI MCP server on {}", addr);

    let ct = CancellationToken::new();

    let service = StreamableHttpService::new(
        || Ok(OrchestratorMcp::default()),
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
            use tokio::signal::unix::{SignalKind, signal};
            let mut sigterm = signal(SignalKind::terminate())
                .expect("failed to install SIGTERM handler");
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
    if !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
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
        assert_ne!(def.service, name, "precondition: service field differs from name");
        let err_msg = format!(
            "service field '{}' in YAML does not match name parameter '{}'",
            def.service, name
        );
        assert!(err_msg.contains("sandbox"));
        assert!(err_msg.contains("apps"));
    }
}
