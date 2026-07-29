//! `codery-ci diagnose` — mismatch detector.
//!
//! Cross-checks three sources of truth and reports discrepancies with the
//! exact command needed to fix each one:
//!
//! 1. State file (`/opt/codery/state/{service}.color`) — what routing uses.
//! 2. Docker container reality — what is actually running.
//! 3. Preview table (`codery.db previews`) — uncut-over `deploy-preview`s.
//!
//! Also verifies each route target host port has a TCP listener.
//!
//! Exit code 1 if any issue found, 0 if all healthy.

use std::collections::HashSet;

use anyhow::{Context, Result};
use bollard::Docker;
use serde::Serialize;

use crate::{config, db, service_def::ServiceDef, state};

// ── Report types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// State file and running container agree.
    Ok,
    /// State file points to one color, but the other color is the only one running.
    Mismatch,
    /// Neither color container is running.
    Dead,
    /// A preview deploy is pending cutover.
    Stale,
    /// Both colors are up (transient — usually mid-deploy).
    Info,
}

#[derive(Debug, Serialize)]
pub struct ServiceIssue {
    pub service: String,
    pub severity: Severity,
    /// Color recorded in `/opt/codery/state/{service}.color`.
    pub state_color: String,
    /// Color of the running container, if exactly one is running.
    pub running_color: Option<String>,
    pub running_container: Option<String>,
    /// Image tag from Docker inspect (`Config.Image`).
    pub image: Option<String>,
    pub fix: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RouteIssue {
    pub subdomain: String,
    pub host_port: u16,
    pub listening: bool,
    pub fix: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct PreviewIssue {
    pub service: String,
    pub sha: String,
    pub color: String,
    pub fix: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DiagnoseReport {
    pub services: Vec<ServiceIssue>,
    /// Only includes routes whose host port has no listener.
    pub routes: Vec<RouteIssue>,
    pub previews: Vec<PreviewIssue>,
    pub all_healthy: bool,
    pub issue_count: usize,
}

impl DiagnoseReport {
    /// Render the report as human-readable CLI output.
    pub fn format_human(&self) -> String {
        let mut out = String::new();

        for svc in &self.services {
            let tag = match svc.severity {
                Severity::Ok => "OK",
                Severity::Mismatch => "MISMATCH",
                Severity::Dead => "DEAD",
                Severity::Stale => "STALE",
                Severity::Info => "INFO",
            };
            out.push_str(&format!(
                "[{}] {:<8} state={}",
                svc.service,
                tag,
                svc.state_color
            ));
            if let Some(c) = &svc.running_color {
                out.push_str(&format!("  running={}", c));
            }
            if let Some(name) = &svc.running_container {
                out.push_str(&format!("  container={}", name));
            }
            if let Some(img) = &svc.image {
                out.push_str(&format!("\n[{}]   image   {}", svc.service, img));
            }
            for fix in &svc.fix {
                out.push_str(&format!("\n[{}]   fix     {}", svc.service, fix));
            }
            out.push('\n');
        }

        for r in &self.routes {
            out.push_str(&format!(
                "[routes] {:<40} → :{}  NO LISTENER\n",
                r.subdomain, r.host_port
            ));
            for fix in &r.fix {
                out.push_str(&format!("[routes]   fix     {}\n", fix));
            }
        }

        for p in &self.previews {
            out.push_str(&format!(
                "[previews] {:<12} sha={} color={}\n",
                p.service, p.sha, p.color
            ));
            for fix in &p.fix {
                out.push_str(&format!("[previews]   {}\n", fix));
            }
        }

        if self.all_healthy {
            out.push_str("\nAll healthy. No issues found.\n");
        } else {
            out.push_str(&format!("\n{} issue(s) found.\n", self.issue_count));
            out.push_str("Run with --json for machine-readable output.\n");
        }

        out
    }
}

// ── Severity classification (pure — testable without Docker) ─────────────────

/// Classify a service given its state-file color and the colors of containers
/// that are currently running.
///
/// - Empty `running` slice → Dead.
/// - Both colors present → Info.
/// - Exactly one running and it matches `state_color` → Ok.
/// - Exactly one running and it does NOT match `state_color` → Mismatch.
pub fn classify_service(state_color: &str, running: &[&str]) -> Severity {
    match running.len() {
        0 => Severity::Dead,
        1 => {
            if running[0] == state_color {
                Severity::Ok
            } else {
                Severity::Mismatch
            }
        }
        _ => Severity::Info,
    }
}

/// Shell commands an operator should run to fix a service issue.
pub fn service_fix(service: &str, severity: &Severity) -> Vec<String> {
    match severity {
        Severity::Ok | Severity::Info => Vec::new(),
        Severity::Mismatch => vec![format!("codery-ci cutover {}", service)],
        Severity::Dead => vec![
            format!("restart_service service='{}'  # via MCP", service),
            format!("codery-ci deploy {} <sha>      # full deploy", service),
        ],
        Severity::Stale => vec![
            format!("codery-ci cutover {}", service),
            format!("codery-ci cancel-preview {}", service),
        ],
    }
}

/// Shell commands to fix a route whose target port has no listener.
/// Routes inherit their fix from the source service's color mismatch.
pub fn route_fix(subdomain: &str, service: &str) -> Vec<String> {
    if service == "host" {
        vec![format!(
            "# host-layer route {} — check supervisord (run_preflight)",
            subdomain
        )]
    } else {
        vec![format!("codery-ci cutover {}", service)]
    }
}

// ── `ss -tlnp` parsing ───────────────────────────────────────────────────────

/// Parse `ss -tlnp` output and return the set of TCP ports currently
/// listening on any address.
///
/// Recognises both IPv4 (`0.0.0.0:13000`) and IPv6 (`[::]:13000`) forms.
pub fn parse_listening_ports(ss_output: &str) -> HashSet<u16> {
    let mut ports = HashSet::new();
    for line in ss_output.lines().skip(1) {
        // The local-address column looks like one of:
        //   `0.0.0.0:13000`
        //   `127.0.0.1:8080`
        //   `[::]:8080`
        //   `[::1]:8080`
        // We grab the digits immediately preceding the next whitespace
        // after the last ':' in the address column.
        if let Some(addr_end) = line.find(' ') {
            let addr = &line[..addr_end];
            if let Some(idx) = addr.rfind(':') {
                let port_str = &addr[idx + 1..];
                if let Ok(p) = port_str.parse::<u16>() {
                    ports.insert(p);
                }
            }
        }
    }
    ports
}

// ── Host-port computation (mirrors caddy.rs / mcp.rs get_routes) ─────────────

/// Compute the host port a route targets given the service's recorded color.
///
/// - `target == "host"` → route.port is already a host port.
/// - `target == "sandbox"` → blue offset 10000, green offset 20000.
/// - Other services → use their `port_scheme` from the service YAML.
pub fn route_host_port(target: &str, container_port: u16, state_color: &str) -> u16 {
    if target == "host" {
        return container_port;
    }
    if target == "sandbox" {
        let offset = if state_color == "blue" { 10000 } else { 20000 };
        return container_port + offset;
    }
    if let Ok(def) = ServiceDef::load(target) {
        return def.port_scheme.host_port(state_color, container_port);
    }
    // Unknown service — assume no offset.
    container_port
}

// ── Full diagnostic run (performs I/O) ───────────────────────────────────────

/// Snapshot of a single color's Docker state for a service.
struct ContainerState {
    running: bool,
    image: Option<String>,
}

/// Inspect a container by name, returning its running state and image tag
/// (or `None` if the container does not exist).
async fn inspect_container(docker: &Docker, name: &str) -> Option<ContainerState> {
    let info = docker.inspect_container(name, None).await.ok()?;
    let running = info
        .state
        .as_ref()
        .and_then(|s| s.running)
        .unwrap_or(false);
    let image = info
        .config
        .as_ref()
        .and_then(|c| c.image.clone())
        .filter(|s| !s.is_empty());
    Some(ContainerState { running, image })
}

/// Run the full diagnostic. Connects to Docker, reads the state file,
/// queries the previews table, and shells out to `ss -tlnp` once.
pub async fn run() -> Result<DiagnoseReport> {
    let defs = ServiceDef::load_all().context("failed to load service definitions")?;
    let docker = Docker::connect_with_socket_defaults()
        .context("failed to connect to Docker socket")?;

    // ── Service checks ───────────────────────────────────────────────────────
    let mut services: Vec<ServiceIssue> = Vec::new();
    let mut state_colors: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for def in &defs {
        let service = &def.service;
        let state_color = state::read_active(service).unwrap_or_else(|_| "blue".to_string());
        state_colors.insert(service.clone(), state_color.clone());

        let blue = config::container_name(service, "blue");
        let green = config::container_name(service, "green");

        let blue_state = inspect_container(&docker, &blue).await;
        let green_state = inspect_container(&docker, &green).await;

        let mut running_colors: Vec<&str> = Vec::new();
        if let Some(s) = &blue_state {
            if s.running {
                running_colors.push("blue");
            }
        }
        if let Some(s) = &green_state {
            if s.running {
                running_colors.push("green");
            }
        }

        let severity = classify_service(&state_color, &running_colors);
        let (running_container, image) = match severity {
            Severity::Ok | Severity::Mismatch => {
                let c = running_colors[0];
                let img = if c == "blue" {
                    blue_state.as_ref().and_then(|s| s.image.clone())
                } else {
                    green_state.as_ref().and_then(|s| s.image.clone())
                };
                (Some(config::container_name(service, c)), img)
            }
            _ => (None, None),
        };

        services.push(ServiceIssue {
            service: service.clone(),
            severity: severity.clone(),
            state_color: state_color.clone(),
            running_color: running_colors.first().map(|s| s.to_string()),
            running_container,
            image,
            fix: service_fix(service, &severity),
        });
    }

    // ── Route checks ─────────────────────────────────────────────────────────
    let conn = db::open().context("failed to open codery.db")?;
    db::init(&conn).context("failed to init codery.db")?;
    let unified = db::build_route_map(&conn).context("failed to build route map")?;
    let domain = config::load_domain();

    let ss_output = shell_ss().await.unwrap_or_default();
    let listening = parse_listening_ports(&ss_output);

    let mut routes: Vec<RouteIssue> = Vec::new();
    for r in &unified {
        let color = state_colors.get(&r.target).map(|s| s.as_str()).unwrap_or("blue");
        let host_port = route_host_port(&r.target, r.port, color);
        if listening.contains(&host_port) {
            continue;
        }
        let fqdn = if r.subdomain.contains('.') {
            r.subdomain.clone()
        } else {
            format!("{}.{}", r.subdomain, domain)
        };
        routes.push(RouteIssue {
            subdomain: fqdn,
            host_port,
            listening: false,
            fix: route_fix(&r.subdomain, &r.target),
        });
    }

    // ── Preview checks ───────────────────────────────────────────────────────
    let previews_db = db::list_previews(&conn).context("failed to list previews")?;
    let previews: Vec<PreviewIssue> = previews_db
        .iter()
        .map(|p| PreviewIssue {
            service: p.service.clone(),
            sha: p.sha.clone(),
            color: p.color.clone(),
            fix: vec![
                format!("codery-ci cutover {}", p.service),
                format!("codery-ci cancel-preview {}", p.service),
            ],
        })
        .collect();

    // ── Aggregate ────────────────────────────────────────────────────────────
    let svc_unhealthy = services.iter().any(|s| matches!(
        s.severity,
        Severity::Mismatch | Severity::Dead | Severity::Stale
    ));
    let all_healthy = !svc_unhealthy && routes.is_empty() && previews.is_empty();
    let issue_count = services
        .iter()
        .filter(|s| !matches!(s.severity, Severity::Ok | Severity::Info))
        .count()
        + routes.len()
        + previews.len();

    Ok(DiagnoseReport {
        services,
        routes,
        previews,
        all_healthy,
        issue_count,
    })
}

/// Run `ss -tlnp` and return combined stdout+stderr. Returns empty string on
/// failure (diagnose should still produce partial output).
async fn shell_ss() -> Option<String> {
    let out = tokio::process::Command::new("ss")
        .args(["-tlnp"])
        .output()
        .await
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stdout.is_empty() && !stderr.is_empty() {
        Some(stderr.to_string())
    } else {
        Some(stdout.to_string())
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_ok_when_state_matches_running() {
        assert_eq!(classify_service("blue", &["blue"]), Severity::Ok);
        assert_eq!(classify_service("green", &["green"]), Severity::Ok);
    }

    #[test]
    fn classify_mismatch_when_state_disagrees_with_running() {
        assert_eq!(classify_service("green", &["blue"]), Severity::Mismatch);
        assert_eq!(classify_service("blue", &["green"]), Severity::Mismatch);
    }

    #[test]
    fn classify_dead_when_nothing_running() {
        assert_eq!(classify_service("blue", &[]), Severity::Dead);
        assert_eq!(classify_service("green", &[]), Severity::Dead);
    }

    #[test]
    fn classify_info_when_both_running() {
        assert_eq!(
            classify_service("blue", &["blue", "green"]),
            Severity::Info
        );
        assert_eq!(
            classify_service("green", &["blue", "green"]),
            Severity::Info
        );
    }

    #[test]
    fn service_fix_mismatch_suggests_cutover() {
        assert_eq!(
            service_fix("sandbox", &Severity::Mismatch),
            vec!["codery-ci cutover sandbox"]
        );
    }

    #[test]
    fn service_fix_dead_lists_recovery_options() {
        let fixes = service_fix("sandbox", &Severity::Dead);
        assert!(fixes.iter().any(|f| f.contains("restart_service")));
        assert!(fixes.iter().any(|f| f.contains("codery-ci deploy")));
    }

    #[test]
    fn service_fix_stale_offers_cutover_and_cancel() {
        let fixes = service_fix("sandbox", &Severity::Stale);
        assert!(fixes.iter().any(|f| f.contains("cutover")));
        assert!(fixes.iter().any(|f| f.contains("cancel-preview")));
    }

    #[test]
    fn service_fix_ok_is_empty() {
        assert!(service_fix("sandbox", &Severity::Ok).is_empty());
        assert!(service_fix("sandbox", &Severity::Info).is_empty());
    }

    #[test]
    fn route_fix_non_host_suggests_cutover() {
        let fixes = route_fix("opencode.example.com", "sandbox");
        assert_eq!(fixes, vec!["codery-ci cutover sandbox"]);
    }

    #[test]
    fn route_fix_host_mentions_preflight() {
        let fixes = route_fix("mcp.example.com", "host");
        assert!(fixes.iter().any(|f| f.contains("run_preflight")));
    }

    #[test]
    fn parse_ss_ipv4_listeners() {
        let ss = "State   Recv-Q Send-Q  Local Address:Port  Peer Address:Port\n\
                  LISTEN  0      4096    0.0.0.0:13000       0.0.0.0:*\n\
                  LISTEN  0      4096    127.0.0.1:8080      0.0.0.0:*\n";
        let ports = parse_listening_ports(ss);
        assert!(ports.contains(&13000));
        assert!(ports.contains(&8080));
        assert_eq!(ports.len(), 2);
    }

    #[test]
    fn parse_ss_ipv6_listeners() {
        let ss = "State   Recv-Q Send-Q  Local Address:Port  Peer Address:Port\n\
                  LISTEN  0      4096    [::]:8080           [::]:*\n\
                  LISTEN  0      4096    [::1]:9090          [::]:*\n";
        let ports = parse_listening_ports(ss);
        assert!(ports.contains(&8080));
        assert!(ports.contains(&9090));
    }

    #[test]
    fn parse_ss_skips_header_and_unparseable() {
        let ss = "State   Recv-Q Send-Q  Local Address:Port  Peer Address:Port\n\
                  LISTEN  0      4096    garbage             stuff\n\
                  LISTEN  0      4096    0.0.0.0:4040        0.0.0.0:*\n";
        let ports = parse_listening_ports(ss);
        assert_eq!(ports.len(), 1);
        assert!(ports.contains(&4040));
    }

    #[test]
    fn route_host_port_for_host_target_is_passthrough() {
        assert_eq!(route_host_port("host", 4040, "blue"), 4040);
        assert_eq!(route_host_port("host", 4040, "green"), 4040);
    }

    #[test]
    fn route_host_port_for_sandbox_applies_offset() {
        assert_eq!(route_host_port("sandbox", 3000, "blue"), 13000);
        assert_eq!(route_host_port("sandbox", 3000, "green"), 23000);
        assert_eq!(route_host_port("sandbox", 22, "blue"), 10022);
        assert_eq!(route_host_port("sandbox", 22, "green"), 20022);
    }

    #[test]
    fn diagnose_report_format_human_lists_each_issue() {
        let report = DiagnoseReport {
            services: vec![ServiceIssue {
                service: "sandbox".to_string(),
                severity: Severity::Mismatch,
                state_color: "green".to_string(),
                running_color: Some("blue".to_string()),
                running_container: Some("codery-sandbox-blue".to_string()),
                image: Some("sandbox-nixos-v1".to_string()),
                fix: vec!["codery-ci cutover sandbox".to_string()],
            }],
            routes: vec![RouteIssue {
                subdomain: "opencode.example.com".to_string(),
                host_port: 23000,
                listening: false,
                fix: vec!["codery-ci cutover sandbox".to_string()],
            }],
            previews: vec![PreviewIssue {
                service: "sandbox".to_string(),
                sha: "sandbox-nixos-v1".to_string(),
                color: "blue".to_string(),
                fix: vec![
                    "codery-ci cutover sandbox".to_string(),
                    "codery-ci cancel-preview sandbox".to_string(),
                ],
            }],
            all_healthy: false,
            issue_count: 3,
        };
        let out = report.format_human();
        assert!(out.contains("[sandbox] MISMATCH"));
        assert!(out.contains("state=green"));
        assert!(out.contains("running=blue"));
        assert!(out.contains("image   sandbox-nixos-v1"));
        assert!(out.contains("fix     codery-ci cutover sandbox"));
        assert!(out.contains("[routes] opencode.example.com"));
        assert!(out.contains("NO LISTENER"));
        assert!(out.contains("[previews] sandbox"));
        assert!(out.contains("3 issue(s) found."));
    }

    #[test]
    fn diagnose_report_format_human_healthy() {
        let report = DiagnoseReport {
            services: vec![ServiceIssue {
                service: "sandbox".to_string(),
                severity: Severity::Ok,
                state_color: "blue".to_string(),
                running_color: Some("blue".to_string()),
                running_container: Some("codery-sandbox-blue".to_string()),
                image: None,
                fix: vec![],
            }],
            routes: vec![],
            previews: vec![],
            all_healthy: true,
            issue_count: 0,
        };
        let out = report.format_human();
        assert!(out.contains("[sandbox] OK"));
        assert!(out.contains("All healthy."));
    }
}
