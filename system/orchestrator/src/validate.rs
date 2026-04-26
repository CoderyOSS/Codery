use std::collections::{HashMap, HashSet};
use std::net::TcpListener;
use std::path::Path;

use anyhow::{Context, Result};
use bollard::Docker;
use bollard::volume::CreateVolumeOptions;

use crate::service_def::{ServiceDef, VolumeMount};
use crate::{config, images, state};

/// Run all pre-deploy validation checks against `def` before touching any container.
///
/// All failures are collected into one error message so the operator sees every
/// problem at once — not just the first. If this returns `Ok(())`, it is safe
/// to proceed with the deployment.
///
/// Checks:
/// 1. Required env vars present in /opt/codery/.env
/// 2. Bind-mount host paths exist (named volumes skipped)
/// 3. Named Docker volumes exist or can be created (idempotent)
/// 4. Image is pullable from GHCR
/// 5. Host ports for `inactive` color are not owned by foreign processes
pub async fn check_deploy(
    def: &ServiceDef,
    sha: &str,
    inactive: &str,
    docker: &Docker,
) -> Result<()> {
    let mut errors: Vec<String> = Vec::new();

    // ── 1. Required env vars ──────────────────────────────────────────────────
    let env_map = load_env_map();
    for key in &def.required_env {
        if !env_map.contains_key(key.as_str()) {
            errors.push(format!(
                "required env var '{}' not found in {}",
                key,
                config::ENV_FILE
            ));
        }
    }

    // ── 2. Bind mount host paths ──────────────────────────────────────────────
    for vol in &def.volumes {
        if let VolumeMount::Bind { host, .. } = vol {
            match substitute_env(host, &env_map) {
                Err(e) => errors.push(format!("volume host path substitution failed: {e}")),
                Ok(resolved) => {
                    if !Path::new(&resolved).exists() {
                        errors.push(format!("bind-mount host path does not exist: {resolved}"));
                    }
                }
            }
        }
    }

    // ── 3. Named Docker volumes ───────────────────────────────────────────────
    for vol in &def.volumes {
        if let VolumeMount::Named { name, .. } = vol {
            if let Err(e) = ensure_named_volume(docker, name).await {
                errors.push(format!("named volume '{}': {e}", name));
            }
        }
    }

    // ── 4. Image pullability ──────────────────────────────────────────────────
    if let Err(e) = images::pull(&def.service, sha).await {
        errors.push(format!("image pull failed: {e}"));
    }

    // ── 5. Host port conflicts ────────────────────────────────────────────────
    // Ports belonging to the currently active color will be vacated when the
    // old container stops, so skip them. Only flag ports that are bound by
    // something else entirely.
    let active = state::read_active(&def.service).unwrap_or_else(|_| "blue".to_string());
    let active_ports: HashSet<u16> = def
        .port_mappings(&active)
        .into_iter()
        .map(|(host, _)| host)
        .collect();

    for (host_port, _container_port) in def.port_mappings(inactive) {
        if active_ports.contains(&host_port) {
            // Will be vacated by the cutover — skip.
            continue;
        }
        match TcpListener::bind(("0.0.0.0", host_port)) {
            Ok(_) => {} // Port is free.
            Err(_) => {
                errors.push(format!(
                    "host port {} is already in use by a foreign process",
                    host_port
                ));
            }
        }
    }

    // ── Collect all failures ──────────────────────────────────────────────────
    if errors.is_empty() {
        println!(
            "[validate] All pre-deploy checks passed for service='{}' sha='{}'",
            def.service, sha
        );
        Ok(())
    } else {
        let msg = format!(
            "[validate] {} pre-deploy check(s) failed for service='{}':\n{}",
            errors.len(),
            def.service,
            errors
                .iter()
                .enumerate()
                .map(|(i, e)| format!("  {}. {}", i + 1, e))
                .collect::<Vec<_>>()
                .join("\n")
        );
        Err(anyhow::anyhow!("{}", msg))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parse /opt/codery/.env into a key→value map. Missing file returns empty map.
fn load_env_map() -> HashMap<String, String> {
    let content = match std::fs::read_to_string(config::ENV_FILE) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

/// Substitute `${VAR}` placeholders from `env`. Returns error on missing vars.
fn substitute_env(s: &str, env: &HashMap<String, String>) -> Result<String> {
    let mut result = s.to_string();
    while let Some(start) = result.find("${") {
        let end = result[start..]
            .find('}')
            .map(|i| start + i)
            .with_context(|| format!("unclosed '${{' in '{s}'"))?;
        let var_name = &result[start + 2..end].to_string();
        let value = env.get(var_name.as_str()).with_context(|| {
            format!("env var '{var_name}' required by volume mount '{s}' not found in .env")
        })?;
        result.replace_range(start..=end, value);
    }
    Ok(result)
}

/// Ensure a named Docker volume exists, creating it if absent.
/// Creating a volume is idempotent and safe — it is not a state change.
async fn ensure_named_volume(docker: &Docker, name: &str) -> Result<()> {
    match docker.inspect_volume(name).await {
        Ok(_) => Ok(()),
        Err(bollard::errors::Error::DockerResponseServerError { status_code: 404, .. }) => {
            println!("[validate] Creating missing named volume: {}", name);
            docker
                .create_volume(CreateVolumeOptions {
                    name,
                    driver: "local",
                    ..Default::default()
                })
                .await
                .with_context(|| format!("failed to create Docker volume '{name}'"))?;
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("failed to inspect Docker volume '{name}'")),
    }
}
