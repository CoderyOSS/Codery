use anyhow::{bail, Context, Result};
use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, NetworkingConfig, RemoveContainerOptions, StartContainerOptions,
};
use bollard::models::{EndpointSettings, HostConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum};
use bollard::network::CreateNetworkOptions;
use std::collections::HashMap;
use std::time::Duration;

use crate::service_def::{HealthCheck, ServiceDef};
use crate::{caddy, config, images, preflight, state, validate};

// ── DeployDeps trait ──────────────────────────────────────────────────────────

trait DeployDeps {
    fn preflight(&self) -> Result<()>;
    fn read_active(&self, service: &str) -> Result<String>;
    fn read_active_sha(&self, service: &str) -> Option<String>;
    fn write_active(&self, service: &str, color: &str) -> Result<()>;
    fn write_active_sha(&self, service: &str, sha: &str) -> Result<()>;
    fn apply_caddy(&self) -> Result<()>;
    async fn ensure_network(&self, network: &str) -> Result<()>;
    async fn validate(&self, def: &ServiceDef, sha: &str, inactive: &str) -> Result<()>;
    async fn start_container(&self, def: &ServiceDef, sha: &str, color: &str) -> Result<()>;
    async fn remove_container_if_exists(&self, name: &str) -> Result<()>;
    async fn stop_container(&self, name: &str) -> Result<()>;
    async fn health_check(&self, def: &ServiceDef, color: &str) -> Result<()>;
    async fn prune_images(&self, service: &str) -> Result<()>;
    fn ensure_nginx_config(&self) -> Result<()>;
}

// ── RealDeps (production implementation) ─────────────────────────────────────

struct RealDeps {
    docker: Docker,
}

impl DeployDeps for RealDeps {
    fn preflight(&self) -> Result<()> {
        preflight::run()
    }
    fn read_active(&self, service: &str) -> Result<String> {
        state::read_active(service)
    }
    fn read_active_sha(&self, service: &str) -> Option<String> {
        state::read_active_sha(service)
    }
    fn write_active(&self, service: &str, color: &str) -> Result<()> {
        state::write_active(service, color)
    }
    fn write_active_sha(&self, service: &str, sha: &str) -> Result<()> {
        state::write_active_sha(service, sha)
    }
    fn apply_caddy(&self) -> Result<()> {
        caddy::apply_all()
    }
    async fn ensure_network(&self, network: &str) -> Result<()> {
        ensure_network(&self.docker, network).await
    }
    async fn validate(&self, def: &ServiceDef, sha: &str, inactive: &str) -> Result<()> {
        validate::check_deploy(def, sha, inactive, &self.docker).await
    }
    async fn start_container(&self, def: &ServiceDef, sha: &str, color: &str) -> Result<()> {
        start_container(&self.docker, def, sha, color).await
    }
    async fn remove_container_if_exists(&self, name: &str) -> Result<()> {
        remove_container_if_exists(&self.docker, name).await
    }
    async fn stop_container(&self, name: &str) -> Result<()> {
        stop_container(&self.docker, name).await
    }
    async fn health_check(&self, def: &ServiceDef, color: &str) -> Result<()> {
        health_check(&self.docker, def, color).await
    }
    async fn prune_images(&self, service: &str) -> Result<()> {
        images::prune(service).await
    }
    fn ensure_nginx_config(&self) -> Result<()> {
        let path = std::path::Path::new(crate::config::NGINX_CONFIG);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, "")?;
            println!("[deploy] Created empty {}", crate::config::NGINX_CONFIG);
        }
        Ok(())
    }
}

/// Entry point called by `main.rs`: load the service definition from YAML
/// and run the full blue/green deploy.
pub async fn run(service: &str, sha: &str) -> Result<()> {
    let def = ServiceDef::load(service)
        .with_context(|| format!("failed to load service definition for '{service}'"))?;
    let docker = Docker::connect_with_socket_defaults()
        .context("failed to connect to Docker socket")?;
    deploy_service(&def, sha, &RealDeps { docker }).await
}

async fn deploy_service<D: DeployDeps>(def: &ServiceDef, sha: &str, deps: &D) -> Result<()> {
    println!(
        "[deploy] Starting {service} blue/green deploy for sha={sha}",
        service = def.service,
        sha = sha
    );

    deps.preflight()?;
    deps.ensure_network(&def.network).await?;

    let active = deps.read_active(&def.service)?;
    let inactive = config::flip(&active);
    println!("[deploy] active={} inactive={}", active, inactive);

    // Idempotency: same SHA already running → no-op.
    if deps.read_active_sha(&def.service).as_deref() == Some(sha) {
        println!("[deploy] already running sha={} — no-op", sha);
        return Ok(());
    }

    // ── Validate everything before touching Docker ────────────────────────────
    deps.validate(def, sha, inactive).await?;

    // ── Deploy inactive color ─────────────────────────────────────────────────
    deps.remove_container_if_exists(&config::container_name(&def.service, inactive)).await?;
    deps.ensure_nginx_config()?;
    deps.start_container(def, sha, inactive).await?;
    println!("[deploy] Started {}", config::container_name(&def.service, inactive));

    // ── Health check ──────────────────────────────────────────────────────────
    deps.health_check(def, inactive).await?;
    println!("[deploy] Health check passed");

    // ── Cutover (no automated rollback from this point forward) ───────────────
    println!(
        "[deploy] CUTOVER BEGIN: {service} active={active} → inactive={inactive} \
         (operator must investigate on failure)",
        service = def.service,
        active = active,
        inactive = inactive
    );
    // Write state BEFORE calling apply_caddy so Caddy reads the new active color.
    deps.write_active(&def.service, inactive)?;
    deps.write_active_sha(&def.service, sha)?;
    println!(
        "[deploy] State updated: {} is now {} (sha={})",
        def.service, inactive, sha
    );
    deps.apply_caddy()?;

    // ── Cleanup ───────────────────────────────────────────────────────────────
    deps.stop_container(&config::container_name(&def.service, &active)).await?;
    println!("[deploy] Stopped old active container codery-{}-{}", def.service, active);

    deps.prune_images(&def.service).await?;

    println!("[deploy] {} deploy complete. Active={}", def.service, inactive);
    Ok(())
}

// ── Container lifecycle ───────────────────────────────────────────────────────

pub(crate) async fn start_container(docker: &Docker, def: &ServiceDef, sha: &str, color: &str) -> Result<()> {
    let name = config::container_name(&def.service, color);
    let image = def.image_ref(sha);

    // Load raw .env lines and apply any overrides declared in the YAML.
    let raw_env = load_env_file()?;
    let container_env = def.resolved_env(&raw_env);

    // Parse env into a map for bind-path substitution.
    let env_map: HashMap<String, String> = raw_env
        .iter()
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect();

    let mappings = def.port_mappings(color);
    let port_bindings = build_port_bindings(&mappings);
    let exposed_ports = build_exposed_ports(&mappings);
    let binds = def.resolved_binds(&env_map)?;

    let networking_config: Option<NetworkingConfig<String>> = if def.network_aliases.is_empty() {
        None
    } else {
        let mut ep = EndpointSettings::default();
        ep.aliases = Some(def.network_aliases.clone());
        let mut endpoints = std::collections::HashMap::new();
        endpoints.insert(def.network.clone(), ep);
        Some(NetworkingConfig { endpoints_config: endpoints })
    };

    docker
        .create_container(
            Some(CreateContainerOptions { name: &name, platform: None }),
            Config {
                image: Some(image),
                env: Some(container_env),
                exposed_ports: Some(exposed_ports),
                host_config: Some(HostConfig {
                    port_bindings: Some(port_bindings),
                    network_mode: Some(def.network.clone()),
                    binds: Some(binds),
                    extra_hosts: if def.extra_hosts.is_empty() { None } else { Some(def.extra_hosts.clone()) },
                    security_opt: Some(vec!["no-new-privileges:true".to_string()]),
                    restart_policy: Some(RestartPolicy {
                        name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                        maximum_retry_count: None,
                    }),
                    ..Default::default()
                }),
                networking_config,
                ..Default::default()
            },
        )
        .await
        .with_context(|| format!("failed to create container {}", name))?;

    docker
        .start_container(&name, None::<StartContainerOptions<String>>)
        .await
        .with_context(|| format!("failed to start container {}", name))?;

    Ok(())
}

async fn health_check(docker: &Docker, def: &ServiceDef, color: &str) -> Result<()> {
    match &def.health_check {
        HealthCheck::Tcp { timeout_secs, interval_secs, .. } => {
            let container_port = def.health_container_port()?;
            let container = config::container_name(&def.service, color);
            println!(
                "[deploy] Health checking TCP port {} (inside container {})...",
                container_port, container
            );
            if !wait_for_tcp_in_container(&container, container_port, *timeout_secs, *interval_secs).await {
                remove_container_if_exists(docker, &container).await?;
                bail!(
                    "{} health check timed out on container port {}",
                    def.service,
                    container_port
                );
            }
        }
        HealthCheck::Docker { timeout_secs } => {
            let name = config::container_name(&def.service, color);
            println!(
                "[deploy] Waiting for Docker HEALTHCHECK to pass (up to {}s)...",
                timeout_secs
            );
            if !wait_for_docker_healthy(docker, &name, *timeout_secs).await? {
                remove_container_if_exists(docker, &name).await?;
                bail!(
                    "{} health check timed out — container did not reach 'healthy' state",
                    def.service
                );
            }
        }
    }
    Ok(())
}

/// Poll health without cleanup on failure. The caller decides what to do on error.
/// Used by the rollback handler where cleanup logic differs from the deploy path.
pub(crate) async fn poll_health(docker: &Docker, def: &ServiceDef, color: &str) -> Result<()> {
    match &def.health_check {
        HealthCheck::Tcp { timeout_secs, interval_secs, .. } => {
            let container_port = def.health_container_port()?;
            let container = config::container_name(&def.service, color);
            println!(
                "[ui] Health checking TCP port {} (inside container {})...",
                container_port, container
            );
            if !wait_for_tcp_in_container(&container, container_port, *timeout_secs, *interval_secs).await {
                anyhow::bail!(
                    "{} health check timed out on container port {}",
                    def.service,
                    container_port
                );
            }
        }
        HealthCheck::Docker { timeout_secs } => {
            let name = config::container_name(&def.service, color);
            println!(
                "[ui] Waiting for Docker HEALTHCHECK to pass (up to {}s)...",
                timeout_secs
            );
            if !wait_for_docker_healthy(docker, &name, *timeout_secs).await? {
                anyhow::bail!(
                    "{} health check timed out — container did not reach 'healthy' state",
                    def.service
                );
            }
        }
    }
    Ok(())
}

pub(crate) async fn remove_container_if_exists(docker: &Docker, name: &str) -> Result<()> {
    match docker
        .remove_container(
            name,
            Some(RemoveContainerOptions { force: true, ..Default::default() }),
        )
        .await
    {
        Ok(_) => {
            println!("[deploy] Removed container {}", name);
            Ok(())
        }
        Err(bollard::errors::Error::DockerResponseServerError { status_code: 404, .. }) => {
            Ok(()) // Didn't exist — fine
        }
        Err(e) => Err(e).with_context(|| format!("failed to remove container {}", name)),
    }
}

/// Stop a container gracefully. No-op if container does not exist (404) or is already stopped (304).
/// Does NOT remove the container — caller is responsible for that.
pub(crate) async fn stop_container(docker: &Docker, name: &str) -> Result<()> {
    use bollard::container::StopContainerOptions;
    match docker.stop_container(name, None::<StopContainerOptions>).await {
        Ok(_) => {
            println!("[deploy] Stopped container {}", name);
            Ok(())
        }
        Err(bollard::errors::Error::DockerResponseServerError { status_code: 404, .. }) => {
            Ok(()) // Didn't exist — fine
        }
        Err(bollard::errors::Error::DockerResponseServerError { status_code: 304, .. }) => {
            Ok(()) // Already stopped — fine
        }
        Err(e) => Err(e).with_context(|| format!("failed to stop container {}", name)),
    }
}

async fn ensure_network(docker: &Docker, network: &str) -> Result<()> {
    match docker
        .create_network(CreateNetworkOptions {
            name: network,
            driver: "bridge",
            ..Default::default()
        })
        .await
    {
        Ok(_) => println!("[deploy] Created network {}", network),
        Err(bollard::errors::Error::DockerResponseServerError { status_code: 409, .. }) => {
            // Already exists — fine
        }
        Err(e) => return Err(e).context("failed to create/verify network"),
    }
    Ok(())
}

// ── Health check helpers ──────────────────────────────────────────────────────

/// Poll a TCP port from *inside* the container via `docker exec`.
///
/// Connecting from the host would probe Docker's userspace proxy, which
/// accepts TCP connections immediately — even before any service inside the
/// container is actually listening. Checking from inside the container avoids
/// this false-positive by going directly to the process binding the port.
async fn wait_for_tcp_in_container(
    container: &str,
    container_port: u16,
    timeout_secs: u64,
    interval_secs: u64,
) -> bool {
    let attempts = timeout_secs / interval_secs.max(1);
    // bash /dev/tcp is a built-in that performs a TCP connect without any
    // external tool. Exit 0 means something is listening; non-zero means not.
    let cmd = format!(
        "exec 3<>/dev/tcp/127.0.0.1/{port} 2>/dev/null && exec 3>&-",
        port = container_port
    );
    for _ in 0..attempts {
        let result = tokio::process::Command::new("docker")
            .args(["exec", container, "bash", "-c", &cmd])
            .status()
            .await;
        match result {
            Ok(status) if status.success() => return true,
            _ => {}
        }
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
    }
    false
}

/// Poll a container's Docker HEALTHCHECK status until healthy or timeout.
async fn wait_for_docker_healthy(docker: &Docker, name: &str, timeout_secs: u64) -> Result<bool> {
    use bollard::models::HealthStatusEnum;

    for i in 0..timeout_secs {
        let info = docker
            .inspect_container(name, None)
            .await
            .with_context(|| format!("failed to inspect container {}", name))?;

        let health = info.state.and_then(|s| s.health);
        let status = health.as_ref().and_then(|h| h.status.clone());

        match status {
            Some(HealthStatusEnum::HEALTHY) => return Ok(true),
            Some(HealthStatusEnum::UNHEALTHY) => {
                println!("[deploy] Container {} is Unhealthy after {}s", name, i);
                if let Some(log_entries) = health.and_then(|h| h.log) {
                    if let Some(last) = log_entries.last() {
                        println!("[deploy] Healthcheck output: {:?}", last.output);
                    }
                }
                return Ok(false);
            }
            _ => {}
        }

        if i % 10 == 0 {
            println!("[deploy] Waiting for {} to become healthy... ({}s)", name, i);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Ok(false)
}

// ── Port binding helpers ──────────────────────────────────────────────────────

fn build_port_bindings(ports: &[(u16, u16)]) -> HashMap<String, Option<Vec<PortBinding>>> {
    let mut map = HashMap::new();
    for (host, container) in ports {
        map.insert(
            format!("{}/tcp", container),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some(host.to_string()),
            }]),
        );
    }
    map
}

fn build_exposed_ports(ports: &[(u16, u16)]) -> HashMap<String, HashMap<(), ()>> {
    ports
        .iter()
        .map(|(_, container)| (format!("{}/tcp", container), HashMap::new()))
        .collect()
}

// ── Env file loader ───────────────────────────────────────────────────────────

/// Parse /opt/codery/.env into Vec<String> of "KEY=VALUE" for container env.
pub fn load_env_file() -> Result<Vec<String>> {
    let content = std::fs::read_to_string(config::ENV_FILE)
        .with_context(|| format!("failed to read {}", config::ENV_FILE))?;

    Ok(content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    // ── Shared test fixture ───────────────────────────────────────────────────

    fn sandbox_def() -> ServiceDef {
        serde_yaml::from_str(r#"
service: sandbox
image: ghcr.io/coderyoss/codery:sandbox-{sha}
port_scheme:
  blue_offset: 10000
  green_offset: 20000
ports:
  - name: opencode
    container_port: 3000
    subdomain: opencode
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

    // ── MockDeps ──────────────────────────────────────────────────────────────

    struct MockDeps {
        events:       RefCell<Vec<String>>,
        active_color: RefCell<String>,
        active_sha:   RefCell<Option<String>>,
        health_ok:    bool,
        validate_ok:  bool,
        preflight_ok: bool,
    }

    impl MockDeps {
        fn new() -> Self {
            Self {
                events:       RefCell::new(Vec::new()),
                active_color: RefCell::new("blue".to_string()),
                active_sha:   RefCell::new(None),
                health_ok:    true,
                validate_ok:  true,
                preflight_ok: true,
            }
        }

        fn events(&self) -> Vec<String> {
            self.events.borrow().clone()
        }
    }

    impl DeployDeps for MockDeps {
        fn preflight(&self) -> Result<()> {
            self.events.borrow_mut().push("preflight".into());
            if self.preflight_ok { Ok(()) } else { anyhow::bail!("mock preflight failed") }
        }
        fn read_active(&self, _service: &str) -> Result<String> {
            Ok(self.active_color.borrow().clone())
        }
        fn read_active_sha(&self, _service: &str) -> Option<String> {
            self.active_sha.borrow().clone()
        }
        fn write_active(&self, service: &str, color: &str) -> Result<()> {
            self.events.borrow_mut().push(format!("write_active:{}={}", service, color));
            *self.active_color.borrow_mut() = color.to_string();
            Ok(())
        }
        fn write_active_sha(&self, service: &str, sha: &str) -> Result<()> {
            self.events.borrow_mut().push(format!("write_active_sha:{}={}", service, sha));
            *self.active_sha.borrow_mut() = Some(sha.to_string());
            Ok(())
        }
        fn apply_caddy(&self) -> Result<()> {
            self.events.borrow_mut().push("apply_caddy".into());
            Ok(())
        }
        async fn ensure_network(&self, network: &str) -> Result<()> {
            self.events.borrow_mut().push(format!("ensure_network:{}", network));
            Ok(())
        }
        async fn validate(&self, _def: &ServiceDef, _sha: &str, _inactive: &str) -> Result<()> {
            self.events.borrow_mut().push("validate".into());
            if self.validate_ok { Ok(()) } else { anyhow::bail!("mock validate failed") }
        }
        async fn start_container(&self, def: &ServiceDef, _sha: &str, color: &str) -> Result<()> {
            self.events.borrow_mut().push(
                format!("start_container:codery-{}-{}", def.service, color)
            );
            Ok(())
        }
        async fn remove_container_if_exists(&self, name: &str) -> Result<()> {
            self.events.borrow_mut().push(format!("remove_container:{}", name));
            Ok(())
        }
        async fn stop_container(&self, name: &str) -> Result<()> {
            self.events.borrow_mut().push(format!("stop_container:{}", name));
            Ok(())
        }
        async fn health_check(&self, def: &ServiceDef, color: &str) -> Result<()> {
            let container = format!("codery-{}-{}", def.service, color);
            self.events.borrow_mut().push(format!("health_check:{}", container));
            if self.health_ok {
                Ok(())
            } else {
                // Mirror real behavior: remove the new container before bailing.
                self.events.borrow_mut().push(format!("remove_container:{}", container));
                anyhow::bail!("mock health check timed out on container port 3000")
            }
        }
        async fn prune_images(&self, service: &str) -> Result<()> {
            self.events.borrow_mut().push(format!("prune_images:{}", service));
            Ok(())
        }
        fn ensure_nginx_config(&self) -> Result<()> { Ok(()) }
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn happy_path_deploys_inactive_and_removes_active() {
        let def = sandbox_def();
        let deps = MockDeps::new(); // active=blue, health passes, validate passes

        deploy_service(&def, "abc123", &deps).await.unwrap();

        assert_eq!(
            deps.events(),
            vec![
                "preflight",
                "ensure_network:codery-net",
                "validate",
                "remove_container:codery-sandbox-green", // clear the inactive slot
                "start_container:codery-sandbox-green",
                "health_check:codery-sandbox-green",
                "write_active:sandbox=green",
                "write_active_sha:sandbox=abc123",
                "apply_caddy",
                "stop_container:codery-sandbox-blue",    // now: stop old active
                "prune_images:sandbox",
            ]
        );
    }

    #[tokio::test]
    async fn state_written_before_caddy_reloaded() {
        let def = sandbox_def();
        let deps = MockDeps::new();

        deploy_service(&def, "abc123", &deps).await.unwrap();

        let events = deps.events();
        let write_pos = events
            .iter()
            .position(|e| e.starts_with("write_active:"))
            .expect("write_active not found in event log");
        let caddy_pos = events
            .iter()
            .position(|e| e == "apply_caddy")
            .expect("apply_caddy not found in event log");

        assert!(
            write_pos < caddy_pos,
            "state must be written before Caddy is reloaded — \
             write_active at {write_pos}, apply_caddy at {caddy_pos}\n\
             events: {events:?}"
        );

        let write_sha_pos = events
            .iter()
            .position(|e| e.starts_with("write_active_sha:"))
            .expect("write_active_sha not found in event log");
        assert!(
            write_sha_pos < caddy_pos,
            "write_active_sha must be written before Caddy is reloaded — \
             write_active_sha at {write_sha_pos}, apply_caddy at {caddy_pos}\n\
             events: {events:?}"
        );
    }

    #[tokio::test]
    async fn health_check_failure_removes_new_container_and_aborts() {
        let def = sandbox_def();
        let deps = MockDeps { health_ok: false, ..MockDeps::new() };

        let result = deploy_service(&def, "abc123", &deps).await;
        assert!(result.is_err(), "deploy should fail when health check fails");

        let events = deps.events();

        // Cleanup is the health_check implementation's responsibility:
        // the real health_check free function calls remove_container_if_exists
        // before bailing, and MockDeps mirrors this by appending the remove event
        // itself. deploy_service does not call remove_container_if_exists on failure.
        assert!(
            events.contains(&"remove_container:codery-sandbox-green".to_string()),
            "new container should be removed on health failure\nevents: {events:?}"
        );

        // Cutover must not have started.
        assert!(
            !events.iter().any(|e| e.starts_with("write_active:")),
            "write_active must not appear after health failure\nevents: {events:?}"
        );
        assert!(
            !events.contains(&"apply_caddy".to_string()),
            "apply_caddy must not appear after health failure\nevents: {events:?}"
        );
        assert!(
            !events.contains(&"prune_images:sandbox".to_string()),
            "prune_images must not appear after health failure\nevents: {events:?}"
        );
    }

    #[tokio::test]
    async fn validate_failure_aborts_before_container_ops() {
        let def = sandbox_def();
        let deps = MockDeps { validate_ok: false, ..MockDeps::new() };

        let result = deploy_service(&def, "abc123", &deps).await;
        assert!(result.is_err(), "deploy should fail when validation fails");

        let events = deps.events();

        // Nothing after validation should appear.
        assert!(
            !events.contains(&"start_container:codery-sandbox-green".to_string()),
            "start_container must not appear after validate failure\nevents: {events:?}"
        );
        assert!(
            !events.contains(&"remove_container:codery-sandbox-green".to_string()),
            "remove_container must not appear after validate failure\nevents: {events:?}"
        );
        assert!(
            !events.iter().any(|e| e.starts_with("write_active:")),
            "write_active must not appear after validate failure\nevents: {events:?}"
        );
        assert!(
            !events.contains(&"apply_caddy".to_string()),
            "apply_caddy must not appear after validate failure\nevents: {events:?}"
        );
    }

    #[tokio::test]
    async fn same_sha_is_a_noop() {
        let def = sandbox_def();
        // Pre-set active_sha to the SHA we are about to deploy.
        let deps = MockDeps {
            active_sha: RefCell::new(Some("abc123".to_string())),
            ..MockDeps::new()
        };

        let result = deploy_service(&def, "abc123", &deps).await;
        assert!(result.is_ok(), "deploy should succeed as a no-op");

        let events = deps.events();

        // Only preflight and ensure_network run before the idempotency check.
        assert_eq!(
            events,
            vec!["preflight", "ensure_network:codery-net"],
            "no container operations should occur when SHA is already active\nevents: {events:?}"
        );
    }
}
