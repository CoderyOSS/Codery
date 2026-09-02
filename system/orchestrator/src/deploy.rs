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

// ── Cutover planning (pure decision, no Docker IO) ───────────────────────────

/// Decision returned by `plan_cutover`. `run_cutover` consumes this and
/// executes the appropriate Docker/state actions.
#[derive(Debug, PartialEq)]
pub(crate) enum CutoverPlan {
    /// Promote `sha` to active.
    /// `already_staged=true`  → inactive container verified running this image (preview path);
    ///                          `run_cutover` may cutover immediately.
    /// `already_staged=false` → `run_cutover` must start the inactive color with this image
    ///                          first (or, for `--sha`, skip start only if inactive happens to
    ///                          already run it).
    Promote { sha: String, already_staged: bool },
    /// Active container's image is already the newest available locally.
    NothingNewer { active_tag: String, newest_tag: String },
    /// No local images for this service exist at all.
    NoLocalImages,
    /// A preview record exists but the inactive container is not running the
    /// previewed image — the staged state was lost (container removed / replaced).
    StalePreview { expected_sha: String },
}

/// Pure cutover resolver. No Docker/state IO — fully unit-testable.
///
/// Resolution priority:
/// 1. `sha_opt` (explicit `--sha`) wins over everything.
/// 2. `preview` (deploy-preview record) wins over auto-newest, but only if
///    `preview_staged` (inactive is actually running that image). Otherwise stale.
/// 3. Auto: pick `newest_local`. Equal to active tag → `NothingNewer`. None → `NoLocalImages`.
pub(crate) fn plan_cutover(
    sha_opt: Option<&str>,
    preview: Option<&str>,
    preview_staged: bool,
    newest_local: Option<&images::LocalImage>,
    active_image_tag: &str,
) -> CutoverPlan {
    // 1. Explicit --sha wins over everything. Not pre-verified — run_cutover
    //    will start_inactive (or skip if inactive already happens to run it).
    if let Some(sha) = sha_opt {
        return CutoverPlan::Promote { sha: sha.to_string(), already_staged: false };
    }

    // 2. Staged preview wins over auto-newest. If the inactive container isn't
    //    actually running the previewed image, that's a stale preview — bail
    //    rather than silently falling through to auto-newest.
    if let Some(p) = preview {
        if preview_staged {
            return CutoverPlan::Promote { sha: p.to_string(), already_staged: true };
        }
        return CutoverPlan::StalePreview { expected_sha: p.to_string() };
    }

    // 3. Auto: newest local image by .created (list_local is newest-first).
    match newest_local {
        None => CutoverPlan::NoLocalImages,
        Some(newest) if newest.tag == active_image_tag => CutoverPlan::NothingNewer {
            active_tag: active_image_tag.to_string(),
            newest_tag: newest.tag.clone(),
        },
        Some(newest) => CutoverPlan::Promote {
            sha: newest.sha.clone(),
            already_staged: false,
        },
    }
}

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
    /// Whether the named container exists AND is in running state.
    /// Used by the dead-container safety guards in `deploy_service` and `cutover`.
    async fn container_running(&self, name: &str) -> Result<bool>;
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
    async fn container_running(&self, name: &str) -> Result<bool> {
        let running = self
            .docker
            .inspect_container(name, None)
            .await
            .ok()
            .and_then(|i| i.state)
            .and_then(|s| s.running)
            .unwrap_or(false);
        Ok(running)
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

/// Load service def and start the inactive color without cutting over.
/// Returns the inactive color that was started, so callers can register a
/// preview route pointing at its host port.
pub async fn run_start_inactive(service: &str, sha: &str) -> Result<String> {
    let def = ServiceDef::load(service)
        .with_context(|| format!("failed to load service definition for '{service}'"))?;
    let docker = Docker::connect_with_socket_defaults()
        .context("failed to connect to Docker socket")?;
    let deps = RealDeps { docker };
    deps.preflight()?;
    deps.ensure_network(&def.network).await?;
    let active = deps.read_active(&def.service)?;
    let inactive = config::flip(&active).to_string();
    println!("[deploy] Starting preview: active={} inactive={}", active, inactive);
    start_inactive(&def, sha, &inactive, &deps).await?;
    Ok(inactive)
}

/// Load service def and run cutover, resolving the SHA to promote via
/// `plan_cutover`. Priority: explicit `--sha` → staged preview → newest local.
///
/// Safety guards:
/// - Never stops the active container unless the inactive is verified running
///   the promoted image (Bug B fix, enforced inside `cutover()`).
/// - A stale preview record (inactive no longer running it) bails with an
///   actionable message rather than silently falling back to auto-newest.
pub async fn run_cutover(
    service: &str,
    sha_opt: Option<&str>,
    preview: Option<&str>,
) -> Result<()> {
    let def = ServiceDef::load(service)
        .with_context(|| format!("failed to load service definition for '{service}'"))?;
    let docker = Docker::connect_with_socket_defaults()
        .context("failed to connect to Docker socket")?;
    let deps = RealDeps { docker: docker.clone() };
    let active = deps.read_active(&def.service)?;
    let inactive = config::flip(&active);

    // ── Gather plan_cutover inputs ────────────────────────────────────────────
    // preview_staged: is the inactive container actually running the preview's image?
    let preview_staged = match preview {
        Some(p) => container_runs_image(&docker, &def, inactive, p).await?,
        None => false,
    };

    // newest_local: list_local is already sorted newest-first.
    let newest_local = images::list_local(&def.service).await?.into_iter().next();

    // active_image_tag: full tag of the active container's image, e.g.
    // "ghcr.io/coderyoss/codery:sandbox-abc123". Compared verbatim against
    // LocalImage.tag (which is the same full tag).
    let active_image_tag = active_container_image(&docker, &def.service, &active)
        .await?
        .unwrap_or_default();

    let plan = plan_cutover(
        sha_opt,
        preview,
        preview_staged,
        newest_local.as_ref(),
        &active_image_tag,
    );

    match plan {
        CutoverPlan::Promote { sha, already_staged: true } => {
            // Preview path: inactive verified running this image. Cutover directly.
            println!("[cutover] Promoting staged preview sha={}", sha);
            cutover(&def, &sha, &active, inactive, &deps).await
        }
        CutoverPlan::Promote { sha, already_staged: false } => {
            // Explicit --sha or auto-newest. If inactive isn't already running
            // this image, start it first (handles --sha pointing at an image
            // that hasn't been deploy-previewed yet).
            let already_up = container_runs_image(&docker, &def, inactive, &sha).await?;
            if already_up {
                println!("[cutover] Inactive already running sha={} — skipping start", sha);
            } else {
                println!("[cutover] Starting inactive {} with sha={}", inactive, sha);
                start_inactive(&def, &sha, inactive, &deps).await?;
            }
            cutover(&def, &sha, &active, inactive, &deps).await
        }
        CutoverPlan::NothingNewer { active_tag, newest_tag } => {
            println!(
                "[cutover] nothing newer to cut to (active={active_tag}, newest={newest_tag})"
            );
            Ok(())
        }
        CutoverPlan::NoLocalImages => {
            bail!(
                "no local images found for '{service}'. \
                 Run `codery-ci build {0} <tag>` or `codery-ci deploy {0} <sha>` first.",
                service
            );
        }
        CutoverPlan::StalePreview { expected_sha } => {
            bail!(
                "stale preview for '{service}': inactive container is not running sha \
                 {expected_sha}. Run `codery-ci cancel-preview {0}` to discard it, then \
                 `cutover` again to promote the newest local image — or `deploy-preview \
                 {0} <sha>` to restage.",
                service
            );
        }
    }
}

/// Inspect a container and return its image tag (`Config.image`), e.g.
/// "ghcr.io/coderyoss/codery:sandbox-abc123". Returns None if the container
/// does not exist.
async fn container_image_tag(
    docker: &Docker,
    name: &str,
) -> Result<Option<String>> {
    Ok(docker
        .inspect_container(name, None)
        .await
        .ok()
        .and_then(|i| i.config.and_then(|c| c.image)))
}

/// True iff `color` container for `def` is running the image tagged with `sha`.
/// Compares the full image ref (`def.image_ref(sha)`) against the container's
/// `Config.image`, and verifies `.state.running == true`.
async fn container_runs_image(
    docker: &Docker,
    def: &ServiceDef,
    color: &str,
    sha: &str,
) -> Result<bool> {
    let name = config::container_name(&def.service, color);
    let Some(inspect) = docker.inspect_container(&name, None).await.ok() else {
        return Ok(false);
    };
    let running = inspect.state.as_ref().and_then(|s| s.running).unwrap_or(false);
    if !running {
        return Ok(false);
    }
    let want = def.image_ref(sha);
    let have = inspect.config.as_ref().and_then(|c| c.image.as_deref()).unwrap_or("");
    Ok(have == want)
}

/// Image tag of the active container, or None if it does not exist.
async fn active_container_image(
    docker: &Docker,
    service: &str,
    active: &str,
) -> Result<Option<String>> {
    let name = config::container_name(service, active);
    container_image_tag(docker, &name).await
}

/// Load service def, stop the inactive container, and prune images.
/// No state changes — active color is untouched. Used by `cancel-preview`.
pub async fn run_cancel_inactive(service: &str) -> Result<()> {
    let def = ServiceDef::load(service)
        .with_context(|| format!("failed to load service definition for '{service}'"))?;
    let docker = Docker::connect_with_socket_defaults()
        .context("failed to connect to Docker socket")?;
    let deps = RealDeps { docker };
    let active = deps.read_active(&def.service)?;
    let inactive = config::flip(&active);
    let container = config::container_name(&def.service, inactive);
    println!("[deploy] Cancelling preview: stopping {}", container);
    deps.stop_container(&container).await?;
    deps.remove_container_if_exists(&container).await?;
    deps.prune_images(&def.service).await?;
    println!("[deploy] Preview cancelled for {}", def.service);
    Ok(())
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

    // Idempotency: same SHA already running AND the active container is actually
    // up → no-op. If the SHA matches but the container is dead/removed, fall
    // through and redeploy (Bug A fix — see deploy_redeploys_when_sha_matches_but_container_dead).
    let active_container = config::container_name(&def.service, &active);
    if deps.read_active_sha(&def.service).as_deref() == Some(sha) {
        if deps.container_running(&active_container).await? {
            println!("[deploy] already running sha={} — no-op", sha);
            return Ok(());
        }
        println!(
            "[deploy] sha={} matches state but active container {} is dead — redeploying",
            sha, active_container
        );
    }

    start_inactive(def, sha, inactive, deps).await?;
    cutover(def, sha, &active, inactive, deps).await?;
    Ok(())
}

/// Validate, remove stale inactive container, start new inactive, health check.
/// Does NOT touch state files or Caddy. Caller may register a preview route
/// pointing at the inactive container after this returns Ok(()).
async fn start_inactive<D: DeployDeps>(
    def: &ServiceDef,
    sha: &str,
    inactive: &str,
    deps: &D,
) -> Result<()> {
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
    Ok(())
}

/// Write state, reload Caddy, stop old active container, prune images.
/// `active` is the color that is currently active (will be stopped).
/// `inactive` is the color that will become active.
async fn cutover<D: DeployDeps>(
    def: &ServiceDef,
    sha: &str,
    active: &str,
    inactive: &str,
    deps: &D,
) -> Result<()> {
    // ── Dead-container guard (Bug B fix) ─────────────────────────────────────
    // Refuse to flip state or stop the active container unless the inactive
    // color is actually running. Without this, a state/reality desync would
    // stop the only live container → total outage.
    let inactive_container = config::container_name(&def.service, inactive);
    if !deps.container_running(&inactive_container).await? {
        bail!(
            "cutover refused: inactive container {} is not running. \
             Refusing to stop active container {} without a verified replacement. \
             Run `codery-ci deploy-preview {svc} <sha>` (or `deploy {svc} <sha>`) \
             to start the inactive color first.",
            inactive_container,
            config::container_name(&def.service, active),
            svc = def.service
        );
    }

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
    deps.stop_container(&config::container_name(&def.service, active)).await?;
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
                cmd: def.command.clone(),
                entrypoint: def.entrypoint.clone(),
                user: def.user.clone(),
                working_dir: def.workdir.clone(),
                exposed_ports: Some(exposed_ports),
                host_config: Some(HostConfig {
                    port_bindings: Some(port_bindings),
                    network_mode: Some(def.network.clone()),
                    binds: Some(binds),
                    extra_hosts: if def.extra_hosts.is_empty() { None } else { Some(def.extra_hosts.clone()) },
                    security_opt: if def.allow_privilege_escalation { None } else { Some(vec!["no-new-privileges:true".to_string()]) },
                    init: if def.init { Some(true) } else { None },
                    ipc_mode: def.ipc.clone(),
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
        /// Set of container names currently "running". Models Docker state.
        /// `start_container` inserts; `stop_container`/`remove_container_if_exists` remove.
        running:      RefCell<std::collections::HashSet<String>>,
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
                // Default: the active color container (codery-sandbox-blue) is up.
                running:      RefCell::new(
                    ["codery-sandbox-blue".to_string()].into_iter().collect()
                ),
            }
        }

        fn events(&self) -> Vec<String> {
            self.events.borrow().clone()
        }

        /// Test helper: mark a container as not-running (e.g. simulate the
        /// "active container was removed during debugging" outage scenario).
        fn kill(&self, name: &str) {
            self.running.borrow_mut().remove(name);
        }

        /// Test helper: mark a container as running.
        fn revive(&self, name: &str) {
            self.running.borrow_mut().insert(name.to_string());
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
            let name = format!("codery-{}-{}", def.service, color);
            self.events.borrow_mut().push(format!("start_container:{}", name));
            self.running.borrow_mut().insert(name);
            Ok(())
        }
        async fn remove_container_if_exists(&self, name: &str) -> Result<()> {
            self.events.borrow_mut().push(format!("remove_container:{}", name));
            self.running.borrow_mut().remove(name);
            Ok(())
        }
        async fn stop_container(&self, name: &str) -> Result<()> {
            self.events.borrow_mut().push(format!("stop_container:{}", name));
            self.running.borrow_mut().remove(name);
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
                self.running.borrow_mut().remove(&container);
                anyhow::bail!("mock health check timed out on container port 3000")
            }
        }
        async fn prune_images(&self, service: &str) -> Result<()> {
            self.events.borrow_mut().push(format!("prune_images:{}", service));
            Ok(())
        }
        fn ensure_nginx_config(&self) -> Result<()> { Ok(()) }
        async fn container_running(&self, name: &str) -> Result<bool> {
            Ok(self.running.borrow().contains(name))
        }
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

    // ── start_inactive / cutover split ─────────────────────────────────────────

    #[tokio::test]
    async fn start_inactive_only_does_not_cutover() {
        let def = sandbox_def();
        let deps = MockDeps::new(); // active=blue

        start_inactive(&def, "abc123", "green", &deps).await.unwrap();

        let events = deps.events();
        // Validates, removes stale inactive, starts container, health checks.
        // Critically: no write_active, no apply_caddy, no stop_container, no prune.
        assert_eq!(
            events,
            vec![
                "validate",
                "remove_container:codery-sandbox-green",
                "start_container:codery-sandbox-green",
                "health_check:codery-sandbox-green",
            ]
        );
    }

    #[tokio::test]
    async fn cutover_only_promotes_inactive() {
        let def = sandbox_def();
        let deps = MockDeps::new(); // active=blue
        // cutover is called AFTER deploy-preview has started the inactive color.
        deps.revive("codery-sandbox-green");

        cutover(&def, "abc123", "blue", "green", &deps).await.unwrap();

        let events = deps.events();
        assert_eq!(
            events,
            vec![
                "write_active:sandbox=green",
                "write_active_sha:sandbox=abc123",
                "apply_caddy",
                "stop_container:codery-sandbox-blue",
                "prune_images:sandbox",
            ]
        );
    }

    #[tokio::test]
    async fn start_inactive_then_cutover_matches_full_deploy() {
        let def = sandbox_def();
        let deps = MockDeps::new();

        // Same SHA, same active color as happy_path_deploys_inactive_and_removes_active.
        let combined = async {
            start_inactive(&def, "abc123", "green", &deps).await?;
            cutover(&def, "abc123", "blue", "green", &deps).await
        };
        combined.await.unwrap();

        let events = deps.events();
        // Compare against the happy-path expectation (minus preflight+ensure_network
        // which live in deploy_service, not the split functions).
        assert_eq!(
            events,
            vec![
                "validate",
                "remove_container:codery-sandbox-green",
                "start_container:codery-sandbox-green",
                "health_check:codery-sandbox-green",
                "write_active:sandbox=green",
                "write_active_sha:sandbox=abc123",
                "apply_caddy",
                "stop_container:codery-sandbox-blue",
                "prune_images:sandbox",
            ]
        );
    }

    #[tokio::test]
    async fn start_inactive_health_failure_does_not_cutover() {
        let def = sandbox_def();
        let deps = MockDeps { health_ok: false, ..MockDeps::new() };

        let result = start_inactive(&def, "abc123", "green", &deps).await;
        assert!(result.is_err());

        let events = deps.events();
        assert!(
            !events.iter().any(|e| e.starts_with("write_active:")),
            "no cutover side-effects on health failure\nevents: {events:?}"
        );
        assert!(
            !events.contains(&"apply_caddy".to_string()),
            "no caddy reload on health failure\nevents: {events:?}"
        );
    }

    // ── Dead-container safety guards (Bug A & Bug B) ──────────────────────────

    /// Bug A: `deploy <sha>` used to no-op whenever the recorded SHA matched,
    /// even if the active container was dead/removed. Recovery required
    /// manually corrupting the state file. Now: SHA match + dead container
    /// triggers a real redeploy.
    #[tokio::test]
    async fn deploy_redeploys_when_sha_matches_but_container_dead() {
        let def = sandbox_def();
        let deps = MockDeps {
            active_sha: RefCell::new(Some("abc123".to_string())),
            ..MockDeps::new()
        };
        // Simulate the outage scenario: active container was removed during
        // debugging but state SHA still records "abc123".
        deps.kill("codery-sandbox-blue");

        deploy_service(&def, "abc123", &deps).await.unwrap();

        let events = deps.events();
        assert!(
            events.contains(&"start_container:codery-sandbox-green".to_string()),
            "dead active container with matching SHA must trigger a real redeploy\n\
             events: {events:?}"
        );
        assert!(
            events.iter().any(|e| e.starts_with("write_active:")),
            "cutover must run after redeploy\nevents: {events:?}"
        );
    }

    /// Bug B: `cutover` used to flip state and stop the active container on the
    /// assumption the inactive color was already live. When state/reality
    /// desynced, this stopped the only running container → total outage.
    /// Now: cutover refuses if the inactive color isn't actually running.
    #[tokio::test]
    async fn cutover_refuses_when_inactive_not_running() {
        let def = sandbox_def();
        let deps = MockDeps::new(); // active=blue; green NOT in running set

        let result = cutover(&def, "abc123", "blue", "green", &deps).await;
        assert!(result.is_err(), "cutover must refuse when inactive is not running");

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("codery-sandbox-green") && err.contains("not running"),
            "error must name the dead container\n got: {err}"
        );

        let events = deps.events();
        assert!(
            !events.iter().any(|e| e.starts_with("write_active:")),
            "no state mutation on refusal\nevents: {events:?}"
        );
        assert!(
            !events.contains(&"stop_container:codery-sandbox-blue".to_string()),
            "must NOT stop the active container when inactive is dead\nevents: {events:?}"
        );
        assert!(
            !events.contains(&"apply_caddy".to_string()),
            "no caddy reload on refusal\nevents: {events:?}"
        );
    }

    // ── plan_cutover (pure decision fn) ───────────────────────────────────────

    fn local(sha: &str, created: i64) -> images::LocalImage {
        // tag follows the registry convention `<service>-<sha>`; the service prefix
        // is irrelevant to plan_cutover — only the tag-equality comparison matters.
        let tag = format!("sandbox-{}", sha);
        images::LocalImage { sha: sha.to_string(), tag, created }
    }

    #[test]
    fn plan_explicit_sha_wins_over_preview_and_newest() {
        let p = local("preview-sha", 100);
        let n = local("newer-sha", 200);
        let plan = plan_cutover(
            Some("explicit-sha"),
            Some("preview-sha"),
            true,
            Some(&n),
            "sandbox-old",
        );
        let _ = &p; // silence unused warning; kept for readability
        assert_eq!(
            plan,
            CutoverPlan::Promote { sha: "explicit-sha".into(), already_staged: false }
        );
    }

    #[test]
    fn plan_explicit_sha_wins_when_only_preview_present() {
        let plan = plan_cutover(Some("x"), Some("preview-sha"), true, None, "sandbox-old");
        assert_eq!(
            plan,
            CutoverPlan::Promote { sha: "x".into(), already_staged: false }
        );
    }

    #[test]
    fn plan_preview_honored_when_staged() {
        let plan = plan_cutover(None, Some("preview-sha"), true, None, "sandbox-old");
        assert_eq!(
            plan,
            CutoverPlan::Promote { sha: "preview-sha".into(), already_staged: true }
        );
    }

    #[test]
    fn plan_preview_priority_over_newer_local() {
        // User staged an OLDER preview, then built something newer.
        // plan_cutover must still pick the staged preview (explicit staging wins).
        let newest = local("newer-sha", 999);
        let plan = plan_cutover(None, Some("preview-sha"), true, Some(&newest), "sandbox-old");
        assert_eq!(
            plan,
            CutoverPlan::Promote { sha: "preview-sha".into(), already_staged: true }
        );
    }

    #[test]
    fn plan_stale_preview_when_not_staged() {
        let plan = plan_cutover(None, Some("preview-sha"), false, None, "sandbox-old");
        assert_eq!(
            plan,
            CutoverPlan::StalePreview { expected_sha: "preview-sha".into() }
        );
    }

    #[test]
    fn plan_stale_preview_even_when_newer_local_exists() {
        // Stale preview must NOT silently fall through to auto-newest.
        // Operator should cancel-preview first, then cutover.
        let newest = local("newer-sha", 999);
        let plan = plan_cutover(None, Some("preview-sha"), false, Some(&newest), "sandbox-old");
        assert_eq!(
            plan,
            CutoverPlan::StalePreview { expected_sha: "preview-sha".into() }
        );
    }

    #[test]
    fn plan_auto_picks_newest_local() {
        let newest = local("newer-sha", 200);
        let plan = plan_cutover(None, None, false, Some(&newest), "sandbox-old");
        assert_eq!(
            plan,
            CutoverPlan::Promote { sha: "newer-sha".into(), already_staged: false }
        );
    }

    #[test]
    fn plan_auto_nothing_newer_when_tags_equal() {
        let newest = local("current", 200);
        // Active container's image tag is the same as the newest local tag.
        let plan = plan_cutover(None, None, false, Some(&newest), "sandbox-current");
        assert_eq!(
            plan,
            CutoverPlan::NothingNewer {
                active_tag: "sandbox-current".into(),
                newest_tag: "sandbox-current".into(),
            }
        );
    }

    #[test]
    fn plan_auto_no_local_images() {
        let plan = plan_cutover(None, None, false, None, "sandbox-old");
        assert_eq!(plan, CutoverPlan::NoLocalImages);
    }
}
