use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use schemars::JsonSchema;

use crate::config;

// ── YAML schema types ─────────────────────────────────────────────────────────

/// Top-level service declaration loaded from /opt/codery/services/{name}.yml.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ServiceDef {
    pub service: String,
    /// Image template. `{sha}` is substituted at deploy time.
    /// Example: "ghcr.io/coderyoss/codery:sandbox-{sha}"
    pub image: String,
    /// Port formula: host_port = container_port + offset(color)
    pub port_scheme: PortScheme,
    /// Discrete named container ports (sandbox-style services).
    #[serde(default)]
    pub ports: Vec<NamedPort>,
    /// Bulk port range for Docker binding (apps-style services).
    pub port_range: Option<PortRange>,
    /// Path to a JSON file with extra Caddy routes (apps-routes.json pattern).
    pub routes_file: Option<String>,
    pub volumes: Vec<VolumeMount>,
    /// Keys that must exist in /opt/codery/.env before deploy is attempted.
    #[serde(default)]
    pub required_env: Vec<String>,
    /// Env vars to override inside the container (beyond what .env provides).
    pub env_overrides: Option<HashMap<String, String>>,
    pub health_check: HealthCheck,
    pub network: String,
    /// Extra /etc/hosts entries injected into the container.
    /// Use "host.docker.internal:host-gateway" to resolve the Docker host.
    #[serde(default)]
    pub extra_hosts: Vec<String>,
}

/// host_port = container_port + offset
/// Both sandbox and apps use this; they just have different offset values.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PortScheme {
    pub blue_offset: u16,
    pub green_offset: u16,
}

impl PortScheme {
    pub fn host_port(&self, color: &str, container_port: u16) -> u16 {
        let offset = if color == "blue" { self.blue_offset } else { self.green_offset };
        container_port + offset
    }
}

/// A named container port with optional public subdomain.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct NamedPort {
    pub name: String,
    pub container_port: u16,
    /// If present, Caddy will route this subdomain to the computed host port.
    pub subdomain: Option<String>,
    /// If present, the codery-ci TCP proxy will listen on this fixed port
    /// and forward connections to the color-specific host port for this entry.
    /// Use for raw TCP services (e.g. SSH) that Caddy cannot proxy.
    pub fixed_port: Option<u16>,
}

/// Describes a range of container ports that Docker binds in bulk.
/// Used for the apps container where many web servers share 8000–9000.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PortRange {
    pub container_start: u16,
    /// Inclusive upper bound.
    pub container_end: u16,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum VolumeMount {
    /// Named Docker volume (e.g. codery_opencode-data).
    Named { name: String, container: String },
    /// Bind mount from a host path. `host` may contain `${VAR}` placeholders
    /// resolved from /opt/codery/.env at deploy time.
    Bind {
        host: String,
        container: String,
        #[serde(default)]
        readonly: bool,
    },
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HealthCheck {
    /// TCP connect to the host port computed from a named port.
    Tcp {
        /// Name from the `ports[]` list.
        port: String,
        timeout_secs: u64,
        interval_secs: u64,
    },
    /// Wait for the Docker HEALTHCHECK to report "healthy".
    Docker { timeout_secs: u64 },
}

// ── Loading ───────────────────────────────────────────────────────────────────

impl ServiceDef {
    /// Load a service definition from /opt/codery/services/{service}.yml.
    pub fn load(service: &str) -> Result<Self> {
        let path = format!("{}/{}.yml", config::SERVICES_DIR, service);
        let data = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read service definition at {path}"))?;
        serde_yaml::from_str(&data)
            .with_context(|| format!("failed to parse {path}"))
    }

    /// Load all service definitions from /opt/codery/services/*.yml.
    /// Returns an empty vec (not an error) if the directory doesn't exist.
    pub fn load_all() -> Result<Vec<Self>> {
        let dir = config::SERVICES_DIR;
        if !Path::new(dir).exists() {
            return Ok(vec![]);
        }
        let mut defs = Vec::new();
        for entry in std::fs::read_dir(dir)
            .with_context(|| format!("failed to read {dir}"))?
        {
            let entry = entry.context("failed to read directory entry")?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("yml") {
                let data = std::fs::read_to_string(&path)
                    .with_context(|| format!("failed to read {:?}", path))?;
                let def: ServiceDef = serde_yaml::from_str(&data)
                    .with_context(|| format!("failed to parse {:?}", path))?;
                defs.push(def);
            }
        }
        Ok(defs)
    }
}

// ── Derived helpers ───────────────────────────────────────────────────────────

impl ServiceDef {
    /// Resolve the Docker image reference by substituting `{sha}`.
    pub fn image_ref(&self, sha: &str) -> String {
        self.image.replace("{sha}", sha)
    }

    /// Return `(host_port, container_port)` pairs for Docker port bindings.
    ///
    /// - For named-port services (sandbox): one pair per `ports[]` entry.
    /// - For range services (apps): one pair per port in `container_start..=container_end`.
    pub fn port_mappings(&self, color: &str) -> Vec<(u16, u16)> {
        let mut mappings = Vec::new();

        // Named ports
        for p in &self.ports {
            let host = self.port_scheme.host_port(color, p.container_port);
            mappings.push((host, p.container_port));
        }

        // Bulk range
        if let Some(ref r) = self.port_range {
            for c in r.container_start..=r.container_end {
                let host = self.port_scheme.host_port(color, c);
                mappings.push((host, c));
            }
        }

        mappings
    }

    /// Resolve the health-check host port for a given color.
    ///
    /// For `HealthCheck::Tcp`, looks up the named port and applies the scheme.
    /// For `HealthCheck::Docker`, returns 0 (unused).
    /// Used in tests to verify port calculation logic.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn health_port(&self, color: &str) -> Result<u16> {
        match &self.health_check {
            HealthCheck::Tcp { port: name, .. } => {
                let p = self
                    .ports
                    .iter()
                    .find(|p| &p.name == name)
                    .with_context(|| {
                        format!(
                            "health_check.port '{}' not found in ports[] for service '{}'",
                            name, self.service
                        )
                    })?;
                Ok(self.port_scheme.host_port(color, p.container_port))
            }
            HealthCheck::Docker { .. } => Ok(0),
        }
    }

    /// For `HealthCheck::Tcp`, returns the *container-side* port (not the host port).
    /// This is the port to probe from *inside* the container.
    pub fn health_container_port(&self) -> Result<u16> {
        match &self.health_check {
            HealthCheck::Tcp { port: name, .. } => {
                let p = self
                    .ports
                    .iter()
                    .find(|p| &p.name == name)
                    .with_context(|| {
                        format!(
                            "health_check.port '{}' not found in ports[] for service '{}'",
                            name, self.service
                        )
                    })?;
                Ok(p.container_port)
            }
            HealthCheck::Docker { .. } => Ok(0),
        }
    }

    /// Resolve volume bind strings for the bollard API.
    ///
    /// Named volumes → `"name:/container"`.
    /// Bind mounts → `"/host:/container"` or `"/host:/container:ro"`.
    /// `${VAR}` placeholders in host paths are substituted from `env`.
    pub fn resolved_binds(&self, env: &HashMap<String, String>) -> Result<Vec<String>> {
        let mut binds = Vec::new();
        for vol in &self.volumes {
            match vol {
                VolumeMount::Named { name, container } => {
                    binds.push(format!("{name}:{container}"));
                }
                VolumeMount::Bind { host, container, readonly } => {
                    let resolved = substitute_env(host, env)?;
                    let ro_suffix = if *readonly { ":ro" } else { "" };
                    binds.push(format!("{resolved}:{container}{ro_suffix}"));
                }
            }
        }
        Ok(binds)
    }

    /// Build the container environment from the raw `.env` lines.
    ///
    /// Starts with all lines from `.env`, then applies `env_overrides` on top.
    pub fn resolved_env(&self, raw_env: &[String]) -> Vec<String> {
        let mut env: Vec<String> = raw_env.to_vec();
        if let Some(ref overrides) = self.env_overrides {
            for (k, v) in overrides {
                // Remove any existing value for this key, then append the override.
                env.retain(|line| !line.starts_with(&format!("{k}=")));
                env.push(format!("{k}={v}"));
            }
        }
        env
    }
}

/// Substitute `${VAR}` placeholders in a string using values from `env`.
fn substitute_env(s: &str, env: &HashMap<String, String>) -> Result<String> {
    let mut result = s.to_string();
    // Walk through all ${VAR} occurrences.
    while let Some(start) = result.find("${") {
        let end = result[start..]
            .find('}')
            .map(|i| start + i)
            .with_context(|| format!("unclosed '${{' in '{s}'"))?;
        let var_name = &result[start + 2..end];
        let value = env.get(var_name).with_context(|| {
            format!("env var '{var_name}' required by volume mount '{s}' not found in .env")
        })?;
        result.replace_range(start..=end, value);
    }
    Ok(result)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox_def() -> ServiceDef {
        serde_yaml::from_str(
            r#"
service: sandbox
image: ghcr.io/coderyoss/codery:sandbox-{sha}
port_scheme:
  blue_offset: 10000
  green_offset: 20000
ports:
  - name: opencode
    container_port: 3000
    subdomain: opencode
  - name: vscode
    container_port: 7000
    subdomain: vscode
  - name: ttyd
    container_port: 7681
    subdomain: cli
health_check:
  type: tcp
  port: opencode
  timeout_secs: 60
  interval_secs: 2
volumes:
  - type: bind
    host: /opt/codery/projects
    container: /home/gem/projects
  - type: named
    name: codery_opencode-data
    container: /home/gem/.local/share/opencode
required_env: []
network: codery-net
"#,
        )
        .unwrap()
    }

    fn apps_def() -> ServiceDef {
        serde_yaml::from_str(
            r#"
service: apps
image: ghcr.io/coderyoss/codery:apps-{sha}
port_scheme:
  blue_offset: 0
  green_offset: 10000
port_range:
  container_start: 8000
  container_end: 9000
routes_file: /opt/codery/proxy/apps-routes.json
health_check:
  type: docker
  timeout_secs: 90
volumes:
  - type: bind
    host: /opt/codery/github-app.pem
    container: /run/secrets/github-app.pem
    readonly: true
required_env: []
network: codery-net
"#,
        )
        .unwrap()
    }

    // Matches the old hardcoded sandbox_host_port() values
    #[test]
    fn sandbox_port_formula_blue() {
        let def = sandbox_def();
        assert_eq!(def.port_scheme.host_port("blue", 3000), 13000);
        assert_eq!(def.port_scheme.host_port("blue", 7000), 17000);
        assert_eq!(def.port_scheme.host_port("blue", 7681), 17681);
    }

    #[test]
    fn sandbox_port_formula_green() {
        let def = sandbox_def();
        assert_eq!(def.port_scheme.host_port("green", 3000), 23000);
        assert_eq!(def.port_scheme.host_port("green", 7000), 27000);
        assert_eq!(def.port_scheme.host_port("green", 7681), 27681);
    }

    #[test]
    fn sandbox_port_mappings_blue() {
        let def = sandbox_def();
        let mappings = def.port_mappings("blue");
        assert!(mappings.contains(&(13000, 3000)));
        assert!(mappings.contains(&(17000, 7000)));
        assert!(mappings.contains(&(17681, 7681)));
        assert_eq!(mappings.len(), 3);
    }

    #[test]
    fn sandbox_port_mappings_green() {
        let def = sandbox_def();
        let mappings = def.port_mappings("green");
        assert!(mappings.contains(&(23000, 3000)));
        assert!(mappings.contains(&(27000, 7000)));
        assert!(mappings.contains(&(27681, 7681)));
    }

    #[test]
    fn sandbox_health_port_blue() {
        let def = sandbox_def();
        assert_eq!(def.health_port("blue").unwrap(), 13000);
    }

    #[test]
    fn sandbox_health_port_green() {
        let def = sandbox_def();
        assert_eq!(def.health_port("green").unwrap(), 23000);
    }

    // Matches the old hardcoded apps_ports() values
    #[test]
    fn apps_blue_port_range() {
        let def = apps_def();
        let mappings = def.port_mappings("blue");
        assert_eq!(mappings.len(), 1001);
        assert!(mappings.contains(&(8000, 8000)));
        assert!(mappings.contains(&(9000, 9000)));
    }

    #[test]
    fn apps_green_port_range() {
        let def = apps_def();
        let mappings = def.port_mappings("green");
        assert_eq!(mappings.len(), 1001);
        assert!(mappings.contains(&(18000, 8000)));
        assert!(mappings.contains(&(19000, 9000)));
    }

    #[test]
    fn image_ref_substitution() {
        let def = sandbox_def();
        assert_eq!(
            def.image_ref("abc123"),
            "ghcr.io/coderyoss/codery:sandbox-abc123"
        );
    }

    #[test]
    fn resolved_binds_named_and_bind() {
        let def = sandbox_def();
        let env = HashMap::new();
        let binds = def.resolved_binds(&env).unwrap();
        assert!(binds.contains(&"/opt/codery/projects:/home/gem/projects".to_string()));
        assert!(binds.contains(&"codery_opencode-data:/home/gem/.local/share/opencode".to_string()));
    }

    #[test]
    fn resolved_binds_with_env_var() {
        let def: ServiceDef = serde_yaml::from_str(r#"
service: apps
image: ghcr.io/coderyoss/codery:apps-{sha}
port_scheme:
  blue_offset: 0
  green_offset: 10000
port_range:
  container_start: 8000
  container_end: 8000
health_check:
  type: docker
  timeout_secs: 90
volumes:
  - type: bind
    host: "${SSH_DIR}"
    container: /home/gem/.ssh
    readonly: true
required_env: []
network: codery-net
"#).unwrap();
        let mut env = HashMap::new();
        env.insert("SSH_DIR".to_string(), "/home/deploy/.ssh".to_string());
        let binds = def.resolved_binds(&env).unwrap();
        assert_eq!(binds[0], "/home/deploy/.ssh:/home/gem/.ssh:ro");
    }

    #[test]
    fn resolved_env_applies_overrides() {
        let def: ServiceDef = serde_yaml::from_str(r#"
service: apps
image: ghcr.io/coderyoss/codery:apps-{sha}
port_scheme:
  blue_offset: 0
  green_offset: 10000
port_range:
  container_start: 8000
  container_end: 8000
health_check:
  type: docker
  timeout_secs: 90
volumes: []
required_env: []
env_overrides:
  GITHUB_APP_PRIVATE_KEY_PATH: /run/secrets/github-app.pem
network: codery-net
"#).unwrap();
        let raw = vec![
            "GITHUB_APP_PRIVATE_KEY_PATH=/old/path".to_string(),
            "OTHER=value".to_string(),
        ];
        let resolved = def.resolved_env(&raw);
        assert!(!resolved.contains(&"GITHUB_APP_PRIVATE_KEY_PATH=/old/path".to_string()));
        assert!(resolved.contains(&"GITHUB_APP_PRIVATE_KEY_PATH=/run/secrets/github-app.pem".to_string()));
        assert!(resolved.contains(&"OTHER=value".to_string()));
    }

    #[test]
    fn health_check_bad_port_name_returns_error() {
        let mut def = sandbox_def();
        def.health_check = HealthCheck::Tcp {
            port: "nonexistent".to_string(),
            timeout_secs: 60,
            interval_secs: 2,
        };
        assert!(def.health_port("blue").is_err());
    }

    #[test]
    fn named_port_fixed_port_deserializes() {
        let def: ServiceDef = serde_yaml::from_str(r#"
service: sandbox
image: ghcr.io/test/x:sandbox-{sha}
port_scheme:
  blue_offset: 10000
  green_offset: 20000
ports:
  - name: ssh
    container_port: 22
    fixed_port: 2222
  - name: web
    container_port: 3000
health_check:
  type: docker
  timeout_secs: 30
volumes: []
required_env: []
network: test-net
"#).unwrap();
        let ssh = def.ports.iter().find(|p| p.name == "ssh").unwrap();
        assert_eq!(ssh.fixed_port, Some(2222));
        let web = def.ports.iter().find(|p| p.name == "web").unwrap();
        assert_eq!(web.fixed_port, None);
    }
}
