# SSH + Internal Proxy + devcontainer.json Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire sandbox→apps SSH (no user credentials), add Nginx inside apps for internal routing (killing per-app host port allocation), and make `.devcontainer/devcontainer.json` the single source of truth for all process/route config.

**Architecture:** Sandbox generates an ED25519 keypair at startup into the shared `/home/gem/projects` volume; apps sshd reads the public key on-demand via `AuthorizedKeysCommand` — SSH just works with no user action. Apps container gains Nginx on port 8080 (already in the mapped port range); Caddy routes all app subdomains to Nginx, which routes by Host header to per-app processes. `.devcontainer/devcontainer.json` drives: sandbox's launchy.json (generated at CI build time), apps supervisord confs (baked into image), and apps-routes.json (synced to VPS).

**Tech Stack:** Rust/bollard (orchestrator), Nginx (internal proxy), supervisord (apps process manager), Python 3 (code-gen scripts), bash (container entrypoints), GitHub Actions (CI).

---

## File Map

**Created:**
- `.devcontainer/devcontainer.json` — source of truth for all sandbox + apps config
- `containers/sandbox/scripts/gen-launchy.py` — extracts sandbox services → `launchy.json`
- `containers/apps/scripts/gen-supervisor-conf.py` — extracts apps → supervisord `.conf` files
- `containers/apps/scripts/gen-apps-routes.py` — extracts apps → `proxy/apps-routes.json`
- `containers/apps/sshd_config` — sshd config with AuthorizedKeysCommand (no password auth)
- `containers/apps/scripts/sandbox-key-lookup` — AuthorizedKeysCommand script (reads shared-volume pubkey)
- `containers/apps/nginx.conf` — Nginx main config (port 8080, no default 80)
- `containers/apps/docker-entrypoint.d/50-start-sshd.sh` — generate SSH host keys + start sshd
- `containers/apps/docker-entrypoint.d/60-start-nginx.sh` — write default conf if missing + start nginx
- `system/orchestrator/src/nginx.rs` — generate Nginx virtual-host config; exec reload in active container

**Modified:**
- `containers/sandbox/docker-entrypoint.d/50-gen-ssh-key.sh` — NEW file: generate keypair + write SSH client config
- `containers/apps/Dockerfile` — add `openssh-server`, `nginx`, create `gem` user (uid 1000), copy new files
- `containers/apps/service.yml` — add `network_aliases: [apps]`, add Nginx bind-mount volume
- `system/orchestrator/src/config.rs` — add `NGINX_CONFIG` constant
- `system/orchestrator/src/service_def.rs` — add `network_aliases: Vec<String>` to `ServiceDef`
- `system/orchestrator/src/deploy.rs` — pass `networking_config` with aliases to `create_container`; create empty nginx.conf on host if missing
- `system/orchestrator/src/caddy.rs` — add `internal_port: Option<u16>` to `AppRoute`
- `system/orchestrator/src/main.rs` — `reload-routes` calls `nginx::generate_and_reload()`
- `system/orchestrator/src/mcp.rs` — `reload_routes` tool also calls `nginx::generate_and_reload()`
- `.github/workflows/deploy-sandbox.yml` — run `gen-launchy.py` before build; add devcontainer.json trigger
- `.github/workflows/deploy-apps.yml` — run gen scripts before build; sync apps-routes.json; add triggers
- `.github/workflows/sync-routes.yml` — after route sync, call `codery-ci reload-routes` (already does; nginx reload is now part of that)
- `AGENTS.md` — document SSH, devcontainer.json, two-speed update model
- `CLAUDE.md` — same

**Deleted:**
- `containers/sandbox/launchy.json` — replaced by devcontainer.json (generated artifact)

---

## Task 1: Create devcontainer.json (source of truth)

**Files:**
- Create: `.devcontainer/devcontainer.json`
- Delete: `containers/sandbox/launchy.json`

- [ ] **Step 1: Create `.devcontainer/devcontainer.json`**

Migrates the current `containers/sandbox/launchy.json` services into the new schema and adds an empty `apps` array.

```bash
mkdir -p /tmp/Codery/.devcontainer
```

Write `.devcontainer/devcontainer.json`:

```json
{
  "name": "Codery",
  "customizations": {
    "codery": {
      "sandbox": {
        "services": [
          {
            "name": "opencode",
            "command": ["opencode", "serve", "--hostname", "0.0.0.0", "--port", "3000"],
            "user": "gem",
            "directory": "/home/gem/projects",
            "env": { "HOME": "/home/gem" },
            "restart": "always"
          },
          {
            "name": "tmux",
            "command": ["bash", "-c", "tmux new-session -d -s main 2>/dev/null || true; exec sleep infinity"],
            "user": "gem",
            "directory": "/home/gem/projects",
            "env": { "HOME": "/home/gem" },
            "restart": "always"
          }
        ]
      },
      "apps": []
    }
  }
}
```

- [ ] **Step 2: Create `containers/sandbox/scripts/gen-launchy.py`**

```python
#!/usr/bin/env python3
"""Generate containers/sandbox/launchy.json from .devcontainer/devcontainer.json.
Usage: python3 gen-launchy.py [output_path]
Run from repo root.
"""
import json, sys

with open('.devcontainer/devcontainer.json') as f:
    dc = json.load(f)

services = dc['customizations']['codery']['sandbox']['services']
output = sys.argv[1] if len(sys.argv) > 1 else 'containers/sandbox/launchy.json'

with open(output, 'w') as f:
    json.dump({'services': services}, f, indent=2)
    f.write('\n')

print(f'[gen-launchy] Wrote {len(services)} service(s) to {output}')
```

- [ ] **Step 3: Verify the script produces the same output as the current launchy.json**

Run from `/tmp/Codery`:
```bash
cd /tmp/Codery
python3 containers/sandbox/scripts/gen-launchy.py /tmp/launchy-generated.json
diff containers/sandbox/launchy.json /tmp/launchy-generated.json
```
Expected: no diff output (files identical).

- [ ] **Step 4: Delete `containers/sandbox/launchy.json` from git**

```bash
cd /tmp/Codery
git rm containers/sandbox/launchy.json
```

- [ ] **Step 5: Add `launchy.json` to sandbox `.gitignore` (generated artifact)**

Create `containers/sandbox/.gitignore`:
```
launchy.json
```

- [ ] **Step 6: Commit**

```bash
cd /tmp/Codery
git add .devcontainer/devcontainer.json \
        containers/sandbox/scripts/gen-launchy.py \
        containers/sandbox/.gitignore
git commit -m "feat: add devcontainer.json as source of truth, gen-launchy.py script"
```

---

## Task 2: Update sandbox CI to generate launchy.json from devcontainer.json

**Files:**
- Modify: `.github/workflows/deploy-sandbox.yml`
- Modify: `examples/Dockerfile.sandbox` (add `RUN` that generates launchy.json, or COPY generated file)

The sandbox Dockerfile builds from the repo context. CI must run `gen-launchy.py` before the Docker build so the generated `launchy.json` exists in the context.

- [ ] **Step 1: Read the current sandbox Dockerfile to understand where launchy.json is copied**

```bash
grep -n "launchy\|COPY\|launchy.json" /tmp/Codery/containers/sandbox/Dockerfile.base
grep -n "launchy\|COPY" /tmp/Codery/examples/Dockerfile.sandbox
```

Expected: a `COPY containers/sandbox/launchy.json /etc/launchy.json` line somewhere.

- [ ] **Step 2: Add gen step to `deploy-sandbox.yml`**

In `.github/workflows/deploy-sandbox.yml`, add a step BEFORE "Build and push sandbox image":

```yaml
      - name: Generate launchy.json from devcontainer.json
        run: python3 containers/sandbox/scripts/gen-launchy.py containers/sandbox/launchy.json
```

- [ ] **Step 3: Add `.devcontainer/devcontainer.json` to the workflow trigger paths**

In the `on.push.paths` list add:
```yaml
      - '.devcontainer/devcontainer.json'
```

- [ ] **Step 4: Test locally that the script works from the repo root**

```bash
cd /tmp/Codery
python3 containers/sandbox/scripts/gen-launchy.py /tmp/test-launchy.json
cat /tmp/test-launchy.json
```
Expected: valid JSON with `services` array containing opencode and tmux entries.

- [ ] **Step 5: Commit**

```bash
cd /tmp/Codery
git add .github/workflows/deploy-sandbox.yml
git commit -m "feat(ci): generate launchy.json from devcontainer.json before sandbox build"
```

---

## Task 3: Add network_aliases support to orchestrator

Apps container needs Docker network alias `apps` so `ssh gem@apps` resolves inside the sandbox. This requires the orchestrator to pass the alias when creating the container.

**Files:**
- Modify: `system/orchestrator/src/service_def.rs` (add field to `ServiceDef`)
- Modify: `system/orchestrator/src/deploy.rs` (pass alias to Docker)

- [ ] **Step 1: Write a failing unit test for network_aliases parsing**

In `system/orchestrator/src/service_def.rs`, in the existing `#[cfg(test)]` block, add:

```rust
#[test]
fn parse_network_aliases() {
    let yaml = r#"
service: apps
image: ghcr.io/coderyoss/codery:apps-{sha}
port_scheme:
  blue_offset: 0
  green_offset: 10000
port_range:
  container_start: 8000
  container_end: 9000
health_check:
  type: docker
  timeout_secs: 90
volumes: []
network: codery-net
network_aliases:
  - apps
  - myalias
"#;
    let def: ServiceDef = serde_yaml::from_str(yaml).expect("parse failed");
    assert_eq!(def.network_aliases, vec!["apps", "myalias"]);
}

#[test]
fn parse_network_aliases_default_empty() {
    let yaml = r#"
service: sandbox
image: ghcr.io/coderyoss/codery:sandbox-{sha}
port_scheme:
  blue_offset: 10000
  green_offset: 20000
health_check:
  type: docker
  timeout_secs: 60
volumes: []
network: codery-net
"#;
    let def: ServiceDef = serde_yaml::from_str(yaml).expect("parse failed");
    assert!(def.network_aliases.is_empty());
}
```

- [ ] **Step 2: Run the test to confirm it fails**

```bash
cd /tmp/Codery/system/orchestrator
cargo test parse_network_aliases 2>&1 | tail -5
```
Expected: `FAILED` — `no field named 'network_aliases'`.

- [ ] **Step 3: Add `network_aliases` field to `ServiceDef`**

In `system/orchestrator/src/service_def.rs`, add after the `extra_hosts` field (line ~39):

```rust
    /// Docker network aliases for this container.
    /// Use ["apps"] so `ssh gem@apps` resolves inside codery-net.
    #[serde(default)]
    pub network_aliases: Vec<String>,
```

- [ ] **Step 4: Run the test to confirm it passes**

```bash
cd /tmp/Codery/system/orchestrator
cargo test parse_network_aliases 2>&1 | tail -5
```
Expected: `test parse_network_aliases ... ok`, `test parse_network_aliases_default_empty ... ok`.

- [ ] **Step 5: Pass network aliases to Docker `create_container` in `deploy.rs`**

In `system/orchestrator/src/deploy.rs`:

Add to the imports at the top:
```rust
use bollard::models::{EndpointSettings, HostConfig, NetworkingConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum};
```
(Replace the existing `HostConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum` import line.)

In the `start_container` function (line ~173), add `networking_config` to the `Config` struct passed to `create_container`:

```rust
    let networking_config: Option<NetworkingConfig<String>> = if def.network_aliases.is_empty() {
        None
    } else {
        let mut ep = EndpointSettings::default();
        ep.aliases = Some(def.network_aliases.clone());
        let mut endpoints = std::collections::HashMap::new();
        endpoints.insert(def.network.clone(), ep);
        Some(NetworkingConfig { endpoints_config: Some(endpoints) })
    };

    docker
        .create_container(
            Some(CreateContainerOptions { name: &name, platform: None }),
            Config {
                image: Some(image),
                env: Some(container_env),
                exposed_ports: Some(exposed_ports),
                networking_config,
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
                ..Default::default()
            },
        )
        .await
        .with_context(|| format!("failed to create container {}", name))?;
```

- [ ] **Step 6: Confirm the project compiles**

```bash
cd /tmp/Codery/system/orchestrator
cargo build 2>&1 | tail -10
```
Expected: `Compiling codery-ci ...` then `Finished`.

- [ ] **Step 7: Commit**

```bash
cd /tmp/Codery
git add system/orchestrator/src/service_def.rs system/orchestrator/src/deploy.rs
git commit -m "feat(orchestrator): add network_aliases field to ServiceDef + pass to Docker"
```

---

## Task 4: Apps container — gem user + sshd

Apps container currently has no `gem` user and no sshd. Sandbox SSHes as `gem`. This task adds both.

**Files:**
- Modify: `containers/apps/Dockerfile`
- Create: `containers/apps/sshd_config`
- Create: `containers/apps/scripts/sandbox-key-lookup`
- Create: `containers/apps/docker-entrypoint.d/50-start-sshd.sh`

- [ ] **Step 1: Create `containers/apps/sshd_config`**

```
Port 22
Protocol 2
HostKey /etc/ssh/ssh_host_ed25519_key
HostKey /etc/ssh/ssh_host_rsa_key

PermitRootLogin no
AuthenticationMethods publickey
PubkeyAuthentication yes

# Read sandbox public key from the shared projects volume on each connection.
# The key is generated by the sandbox container at startup.
AuthorizedKeysCommand /usr/local/bin/sandbox-key-lookup %u
AuthorizedKeysCommandUser nobody

PasswordAuthentication no
ChallengeResponseAuthentication no
UsePAM no

PrintMotd no
AcceptEnv LANG LC_*
Subsystem sftp /usr/lib/openssh/sftp-server
```

- [ ] **Step 2: Create `containers/apps/scripts/sandbox-key-lookup`**

```bash
#!/bin/bash
# Called by sshd AuthorizedKeysCommand with the connecting username as $1.
# Reads sandbox's public key from the shared volume.
# Returns empty (no authorized keys) if file not yet written.
cat /home/gem/projects/.codery/sandbox-key.pub 2>/dev/null
```

- [ ] **Step 3: Create `containers/apps/docker-entrypoint.d/50-start-sshd.sh`**

```bash
#!/bin/bash
# Generate SSH host keys (fast no-op if already present) then start sshd.
ssh-keygen -A -q
/usr/sbin/sshd -f /etc/ssh/sshd_config \
  || { echo "[apps] WARNING: sshd failed to start (exit $?)"; exit 0; }
echo "[apps] sshd started on port 22"
```

- [ ] **Step 4: Update `containers/apps/Dockerfile` — add gem user, openssh-server, new scripts**

Add to the `apt-get install` line:
```
    openssh-server \
```

After the existing `RUN` blocks but before the supervisor COPY, add:
```dockerfile
# Create gem user (uid 1000) — same uid as sandbox, owns the shared projects volume.
RUN useradd -m -u 1000 -s /bin/bash gem

# SSH server config
COPY containers/apps/sshd_config /etc/ssh/sshd_config
COPY containers/apps/scripts/sandbox-key-lookup /usr/local/bin/sandbox-key-lookup
RUN chmod +x /usr/local/bin/sandbox-key-lookup

RUN mkdir -p /run/sshd
```

Also extend the entrypoint copy (the existing `COPY containers/apps/docker-entrypoint.d/ /docker-entrypoint.d/` already covers new files if added to that directory).

- [ ] **Step 5: Verify the Dockerfile builds locally (smoke test)**

```bash
cd /tmp/Codery
docker build -f containers/apps/Dockerfile -t codery-apps-test . 2>&1 | tail -20
```
Expected: `Successfully built ...` (no errors).

- [ ] **Step 6: Verify gem user exists and sshd is installed**

```bash
docker run --rm codery-apps-test id gem
# Expected: uid=1000(gem) gid=1000(gem) groups=1000(gem)

docker run --rm codery-apps-test which sshd
# Expected: /usr/sbin/sshd
```

- [ ] **Step 7: Commit**

```bash
cd /tmp/Codery
git add containers/apps/Dockerfile \
        containers/apps/sshd_config \
        containers/apps/scripts/sandbox-key-lookup \
        containers/apps/docker-entrypoint.d/50-start-sshd.sh
git commit -m "feat(apps): add gem user, sshd with AuthorizedKeysCommand for sandbox SSH"
```

---

## Task 5: Sandbox — SSH keypair generation at startup

At container startup, sandbox generates an ED25519 keypair into the shared volume. Apps sshd reads the public key on-demand. No user action needed.

**Files:**
- Create: `containers/sandbox/docker-entrypoint.d/50-gen-ssh-key.sh`

- [ ] **Step 1: Create `containers/sandbox/docker-entrypoint.d/50-gen-ssh-key.sh`**

```bash
#!/bin/bash
# Generate a fresh ED25519 keypair for sandbox→apps SSH.
# Private key: /home/gem/projects/.codery/sandbox-key (shared volume, 600)
# Public key:  /home/gem/projects/.codery/sandbox-key.pub (shared volume, 644)
# SSH config:  /home/gem/.ssh/config (adds Host apps block)
#
# Regenerated on every sandbox startup so the key rotates with the container.

KEY_DIR="/home/gem/projects/.codery"
KEY="$KEY_DIR/sandbox-key"

mkdir -p "$KEY_DIR"
chown gem:gem "$KEY_DIR"
chmod 700 "$KEY_DIR"

# Always regenerate — apps sshd reads .pub on-demand, no stale auth issues.
rm -f "$KEY" "$KEY.pub"
ssh-keygen -t ed25519 -f "$KEY" -N "" -C "codery-sandbox" -q
chown gem:gem "$KEY" "$KEY.pub"
chmod 600 "$KEY"
chmod 644 "$KEY.pub"

echo "[sandbox] Generated sandbox→apps SSH keypair in $KEY_DIR"

# Write SSH client config for gem so `ssh gem@apps` works with no flags.
SSH_CONFIG_DIR="/home/gem/.ssh"
mkdir -p "$SSH_CONFIG_DIR"
chown gem:gem "$SSH_CONFIG_DIR"
chmod 700 "$SSH_CONFIG_DIR"

cat > "$SSH_CONFIG_DIR/config" <<'EOF'
Host apps
    HostName apps
    User gem
    IdentityFile /home/gem/projects/.codery/sandbox-key
    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
    LogLevel ERROR
EOF
chown gem:gem "$SSH_CONFIG_DIR/config"
chmod 600 "$SSH_CONFIG_DIR/config"

echo "[sandbox] SSH client config written — use: ssh gem@apps"
```

- [ ] **Step 2: Make the script executable**

```bash
chmod +x /tmp/Codery/containers/sandbox/docker-entrypoint.d/50-gen-ssh-key.sh
```

- [ ] **Step 3: Test locally (with a dummy shared volume)**

```bash
mkdir -p /tmp/test-projects
docker run --rm \
  -v /tmp/test-projects:/home/gem/projects \
  --entrypoint bash \
  $(docker images codery-sandbox-base -q | head -1) \
  -c "bash /docker-entrypoint.d/50-gen-ssh-key.sh && cat /home/gem/projects/.codery/sandbox-key.pub"
```
Expected: `ssh-ed25519 AAAA... codery-sandbox` (one-line public key).

If sandbox base image isn't available locally, skip manual test and verify in CI.

- [ ] **Step 4: Commit**

```bash
cd /tmp/Codery
git add containers/sandbox/docker-entrypoint.d/50-gen-ssh-key.sh
git commit -m "feat(sandbox): generate ED25519 keypair at startup for sandbox→apps SSH"
```

---

## Task 6: Apps container — Nginx internal proxy

Nginx runs inside apps on port 8080. Config is bind-mounted from the host so routes update without image rebuild.

**Files:**
- Modify: `containers/apps/Dockerfile`
- Create: `containers/apps/nginx.conf`
- Create: `containers/apps/docker-entrypoint.d/60-start-nginx.sh`

- [ ] **Step 1: Create `containers/apps/nginx.conf`**

Replaces the default nginx.conf. Moves all serving to port 8080 (port 80 is not used).

```nginx
user www-data;
worker_processes auto;
pid /run/nginx.pid;

events {
    worker_connections 1024;
}

http {
    sendfile on;
    tcp_nopush on;
    include /etc/nginx/mime.types;
    default_type application/octet-stream;

    # Log to stdout/stderr for Docker log collection.
    access_log /dev/stdout;
    error_log /dev/stderr warn;

    # Virtual host configs are generated by codery-ci and bind-mounted here.
    include /etc/nginx/conf.d/*.conf;
}
```

- [ ] **Step 2: Create `containers/apps/docker-entrypoint.d/60-start-nginx.sh`**

Creates a fallback config if none is mounted (first-boot before any apps deployed), then starts nginx.

```bash
#!/bin/bash
# Start Nginx internal reverse proxy on port 8080.
# The real config is bind-mounted at /etc/nginx/conf.d/apps.conf by the orchestrator.
# If not yet mounted, write a fallback so Nginx starts cleanly.

CONF=/etc/nginx/conf.d/apps.conf

if [ ! -f "$CONF" ]; then
    cat > "$CONF" <<'EOF'
# Placeholder — no apps deployed yet. codery-ci reload-routes generates this file.
server {
    listen 8080 default_server;
    return 503 "No apps configured";
}
EOF
    echo "[apps] Wrote placeholder Nginx config (no apps deployed yet)"
fi

nginx -t -q || { echo "[apps] ERROR: Nginx config test failed"; exit 1; }
nginx -g 'daemon off;' &
echo "[apps] Nginx started on port 8080 (pid $!)"
```

- [ ] **Step 3: Update `containers/apps/Dockerfile` — add nginx**

Add `nginx \` to the `apt-get install` list.

Add after the sshd section:
```dockerfile
# Nginx internal reverse proxy (routes by Host header on port 8080).
COPY containers/apps/nginx.conf /etc/nginx/nginx.conf
RUN mkdir -p /etc/nginx/conf.d
```

- [ ] **Step 4: Build and verify Nginx starts**

```bash
cd /tmp/Codery
docker build -f containers/apps/Dockerfile -t codery-apps-test . 2>&1 | tail -5

docker run --rm codery-apps-test nginx -v
# Expected: nginx version: nginx/1.x.x
```

- [ ] **Step 5: Commit**

```bash
cd /tmp/Codery
git add containers/apps/Dockerfile \
        containers/apps/nginx.conf \
        containers/apps/docker-entrypoint.d/60-start-nginx.sh
git commit -m "feat(apps): add Nginx internal proxy on port 8080"
```

---

## Task 7: Orchestrator — nginx.rs module + AppRoute extension

The orchestrator generates the Nginx virtual-host config from `apps-routes.json` (which gains `internal_port`) and exec-reloads Nginx in the active apps container after route changes.

**Files:**
- Modify: `system/orchestrator/src/config.rs`
- Modify: `system/orchestrator/src/caddy.rs`
- Create: `system/orchestrator/src/nginx.rs`
- Modify: `system/orchestrator/src/main.rs`
- Modify: `system/orchestrator/src/mcp.rs`

- [ ] **Step 1: Add `NGINX_CONFIG` constant to `config.rs`**

In `system/orchestrator/src/config.rs`, after the `APPS_ROUTES` constant:

```rust
/// Nginx virtual-host config generated by codery-ci and bind-mounted into the apps container.
pub const NGINX_CONFIG: &str = "/opt/codery/proxy/apps-nginx.conf";
```

- [ ] **Step 2: Add `internal_port` to `AppRoute` in `caddy.rs`**

In `system/orchestrator/src/caddy.rs`, update `AppRoute`:

```rust
#[derive(Deserialize)]
pub struct AppRoute {
    pub subdomain: String,
    pub port: u16,
    /// Internal port Nginx proxies to inside the apps container.
    /// If absent, this route is Caddy-only (no Nginx block generated).
    pub internal_port: Option<u16>,
}
```

- [ ] **Step 3: Write unit tests for `nginx.rs` (they will fail — no module yet)**

Create `system/orchestrator/src/nginx.rs` with tests first:

```rust
use anyhow::{Context, Result};
use bollard::Docker;
use bollard::exec::{CreateExecOptions, StartExecResults};
use futures_util::StreamExt;
use std::fs;

use crate::caddy::AppRoute;
use crate::{config, state};

// ── Public API ────────────────────────────────────────────────────────────────

/// Generate Nginx config from apps-routes.json and reload Nginx in the active apps container.
/// Called by `reload-routes` and the `reload_routes` MCP tool after Caddy is updated.
pub async fn generate_and_reload() -> Result<()> {
    let routes = load_routes()?;
    let domain = config::load_domain();
    let content = generate_config(&routes, &domain);

    if let Some(parent) = std::path::Path::new(config::NGINX_CONFIG).parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create dir for {}", config::NGINX_CONFIG))?;
    }
    fs::write(config::NGINX_CONFIG, &content)
        .with_context(|| format!("failed to write {}", config::NGINX_CONFIG))?;
    println!("[nginx] Config written ({} server block(s))", routes.iter().filter(|r| r.internal_port.is_some()).count());

    reload_in_active_container().await
}

// ── Config generation ─────────────────────────────────────────────────────────

fn load_routes() -> Result<Vec<AppRoute>> {
    let path = config::APPS_ROUTES;
    if !std::path::Path::new(path).exists() {
        return Ok(vec![]);
    }
    let data = fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path))?;
    serde_json::from_str(&data)
        .with_context(|| format!("failed to parse {}", path))
}

pub(crate) fn generate_config(routes: &[AppRoute], domain: &str) -> String {
    let blocks: Vec<String> = routes
        .iter()
        .filter_map(|r| {
            let internal_port = r.internal_port?;
            let fqdn = if r.subdomain.contains('.') {
                r.subdomain.clone()
            } else {
                format!("{}.{}", r.subdomain, domain)
            };
            Some(format!(
                "server {{\n    listen 8080;\n    server_name {fqdn};\n    location / {{\n        proxy_pass http://127.0.0.1:{internal_port};\n        proxy_set_header Host $host;\n        proxy_set_header X-Real-IP $remote_addr;\n        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;\n    }}\n}}\n"
            ))
        })
        .collect();

    if blocks.is_empty() {
        return String::new();
    }

    let mut out = blocks.join("\n");
    out.push_str("\nserver {\n    listen 8080 default_server;\n    return 404;\n}\n");
    out
}

// ── Container reload ──────────────────────────────────────────────────────────

async fn reload_in_active_container() -> Result<()> {
    let color = match state::read_active("apps") {
        Ok(c) => c,
        Err(_) => {
            println!("[nginx] No active apps state — skipping reload");
            return Ok(());
        }
    };
    let container = config::container_name("apps", &color);

    let docker = Docker::connect_with_socket_defaults()
        .context("failed to connect to Docker")?;

    let running = docker.inspect_container(&container, None).await
        .ok()
        .and_then(|i| i.state)
        .and_then(|s| s.running)
        .unwrap_or(false);

    if !running {
        println!("[nginx] Container {} not running — skipping reload", container);
        return Ok(());
    }

    let exec = docker.create_exec(
        &container,
        CreateExecOptions {
            cmd: Some(vec!["nginx", "-s", "reload"]),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        },
    ).await.with_context(|| format!("failed to create exec in {}", container))?;

    if let StartExecResults::Attached { mut output, .. } = docker.start_exec(&exec.id, None).await? {
        while let Some(msg) = output.next().await {
            match msg? {
                bollard::container::LogOutput::StdErr { message } => {
                    let text = String::from_utf8_lossy(&message);
                    if !text.trim().is_empty() {
                        eprint!("[nginx] {}", text);
                    }
                }
                _ => {}
            }
        }
    }

    println!("[nginx] Reloaded Nginx in {}", container);
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn route(subdomain: &str, internal_port: Option<u16>) -> AppRoute {
        AppRoute { subdomain: subdomain.to_string(), port: 8080, internal_port }
    }

    #[test]
    fn generate_config_with_two_apps() {
        let routes = vec![route("myapp", Some(3001)), route("otherapp", Some(3002))];
        let cfg = generate_config(&routes, "example.com");
        assert!(cfg.contains("server_name myapp.example.com;"));
        assert!(cfg.contains("proxy_pass http://127.0.0.1:3001;"));
        assert!(cfg.contains("server_name otherapp.example.com;"));
        assert!(cfg.contains("proxy_pass http://127.0.0.1:3002;"));
        assert!(cfg.contains("listen 8080 default_server;"));
    }

    #[test]
    fn generate_config_skips_routes_without_internal_port() {
        let routes = vec![route("myapp", None)];
        let cfg = generate_config(&routes, "example.com");
        assert!(cfg.is_empty());
    }

    #[test]
    fn generate_config_empty_returns_empty_string() {
        let cfg = generate_config(&[], "example.com");
        assert!(cfg.is_empty());
    }

    #[test]
    fn generate_config_fqdn_subdomain_passthrough() {
        let routes = vec![route("myapp.custom.com", Some(3001))];
        let cfg = generate_config(&routes, "example.com");
        // Full FQDNs are used as-is, not appended with domain.
        assert!(cfg.contains("server_name myapp.custom.com;"));
        assert!(!cfg.contains("myapp.custom.com.example.com"));
    }
}
```

- [ ] **Step 4: Run the tests (they should now pass since module is written)**

```bash
cd /tmp/Codery/system/orchestrator
cargo test nginx:: 2>&1 | tail -10
```
Expected: 4 tests pass.

- [ ] **Step 5: Register `nginx` module in `main.rs`**

In `system/orchestrator/src/main.rs`, add at the top with the other `mod` declarations:
```rust
mod nginx;
```

- [ ] **Step 6: Update `reload-routes` in `main.rs` to also reload Nginx**

```rust
        Some("reload-routes") => {
            caddy::apply_all()?;
            tokio::runtime::Runtime::new()?.block_on(nginx::generate_and_reload())?;
            println!("[routes] Reloaded Caddyfile and Nginx from all service definitions");
        }
```

Note: `reload-routes` currently runs synchronously. `nginx::generate_and_reload` is async. Wrap in `tokio::runtime::Runtime::new()?.block_on(...)` or convert main to async (check existing pattern).

If `main` is already `#[tokio::main]`, just use `.await`:
```rust
        Some("reload-routes") => {
            caddy::apply_all()?;
            nginx::generate_and_reload().await?;
            println!("[routes] Reloaded Caddyfile and Nginx from all service definitions");
        }
```

Check which applies:
```bash
head -5 /tmp/Codery/system/orchestrator/src/main.rs
```

- [ ] **Step 7: Update `reload_routes` MCP tool in `mcp.rs`**

In `system/orchestrator/src/mcp.rs`, find `reload_routes` (line ~472) and update:

```rust
    async fn reload_routes(&self) -> Result<CallToolResult, McpError> {
        caddy::apply_all().map_err(|e| tool_err(e.to_string()))?;
        nginx::generate_and_reload().await.map_err(|e| tool_err(e.to_string()))?;
        tool_ok("Routes reloaded — Caddy and Nginx updated".to_string())
    }
```

Add `use crate::nginx;` at the top of `mcp.rs` imports.

- [ ] **Step 8: Confirm full build + all tests pass**

```bash
cd /tmp/Codery/system/orchestrator
cargo test 2>&1 | tail -15
```
Expected: all existing tests pass plus 4 new nginx tests.

- [ ] **Step 9: Commit**

```bash
cd /tmp/Codery
git add system/orchestrator/src/config.rs \
        system/orchestrator/src/caddy.rs \
        system/orchestrator/src/nginx.rs \
        system/orchestrator/src/main.rs \
        system/orchestrator/src/mcp.rs
git commit -m "feat(orchestrator): add nginx.rs, AppRoute.internal_port, reload Nginx on reload-routes"
```

---

## Task 8: Update apps service.yml — network alias + Nginx bind-mount

**Files:**
- Modify: `containers/apps/service.yml`

- [ ] **Step 1: Add `network_aliases` and Nginx bind-mount to `containers/apps/service.yml`**

```yaml
service: apps
image: ghcr.io/CoderyOSS/codery:apps-{sha}

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
    host: "${SSH_DIR}"
    container: /home/gem/.ssh
    readonly: true
  - type: bind
    host: /opt/codery/github-app.pem
    container: /run/secrets/github-app.pem
    readonly: true
  - type: bind
    host: /opt/codery/projects
    container: /home/gem/projects
  - type: bind
    host: /opt/codery/proxy/apps-nginx.conf
    container: /etc/nginx/conf.d/apps.conf
    readonly: true

env_overrides:
  GITHUB_APP_PRIVATE_KEY_PATH: /run/secrets/github-app.pem

required_env:
  - GHCR_USERNAME
  - GHCR_TOKEN
  - SSH_DIR

network: codery-net

# Docker alias so `ssh gem@apps` resolves from sandbox inside codery-net.
network_aliases:
  - apps
```

- [ ] **Step 2: Ensure orchestrator creates the file if missing before deploy**

In `system/orchestrator/src/deploy.rs`, in `deploy_service` (after `deps.preflight()?`), add a host-side file creation guard. This belongs in `RealDeps::validate` or better: as a new step before `start_container`.

In the `deploy_service` function, after the preflight call:

```rust
    // Ensure Nginx config exists on host (bind-mount requires the file to pre-exist).
    // deploy.rs can only do this in the real-deps path; test deps skip it.
    // Place this logic in `start_container` via the deps trait if needed for testability.
```

Since `deploy_service` uses the `DeployDeps` trait for testability, add a new method:

In the `DeployDeps` trait:
```rust
    fn ensure_nginx_config(&self) -> Result<()>;
```

In `RealDeps` impl:
```rust
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
```

In the mock `TestDeps` (in the test section):
```rust
    fn ensure_nginx_config(&self) -> Result<()> { Ok(()) }
```

Call it in `deploy_service` right before `deps.start_container(...)`:
```rust
    deps.ensure_nginx_config()?;
    deps.start_container(def, sha, inactive).await?;
```

- [ ] **Step 3: Confirm tests still pass**

```bash
cd /tmp/Codery/system/orchestrator
cargo test 2>&1 | tail -10
```

- [ ] **Step 4: Commit**

```bash
cd /tmp/Codery
git add containers/apps/service.yml system/orchestrator/src/deploy.rs
git commit -m "feat(apps): add network_aliases, Nginx bind-mount to service.yml; ensure nginx.conf exists pre-deploy"
```

---

## Task 9: Apps — gen scripts + deploy-apps.yml CI update

Generates supervisord confs (baked into image) and apps-routes.json (synced to VPS) from devcontainer.json.

**Files:**
- Create: `containers/apps/scripts/gen-supervisor-conf.py`
- Create: `containers/apps/scripts/gen-apps-routes.py`
- Modify: `.github/workflows/deploy-apps.yml`

- [ ] **Step 1: Create `containers/apps/scripts/gen-supervisor-conf.py`**

```python
#!/usr/bin/env python3
"""Generate supervisord conf files for each app in .devcontainer/devcontainer.json.
Usage: python3 gen-supervisor-conf.py [output_dir]
Run from repo root.
"""
import json, os, sys

with open('.devcontainer/devcontainer.json') as f:
    dc = json.load(f)

apps = dc['customizations']['codery'].get('apps', [])
out_dir = sys.argv[1] if len(sys.argv) > 1 else 'containers/apps/supervisor/projects.d'
os.makedirs(out_dir, exist_ok=True)

# Remove old generated confs.
for name in os.listdir(out_dir):
    if name.endswith('.conf'):
        os.remove(os.path.join(out_dir, name))

for app in apps:
    name = app['name']
    env_str = ','.join(f'{k}="{v}"' for k, v in app.get('env', {}).items())
    env_line = f'environment={env_str}' if env_str else ''
    conf = f"""[program:{name}]
command={app['command']}
directory={app['directory']}
autostart=true
autorestart=true
stdout_logfile=/var/log/supervisor/{name}.log
stderr_logfile=/var/log/supervisor/{name}.log
{env_line}
""".strip() + '\n'
    path = os.path.join(out_dir, f'{name}.conf')
    with open(path, 'w') as f:
        f.write(conf)
    print(f'[gen-supervisor] Wrote {path}')

if not apps:
    print('[gen-supervisor] No apps defined — projects.d is empty')
```

- [ ] **Step 2: Create `containers/apps/scripts/gen-apps-routes.py`**

```python
#!/usr/bin/env python3
"""Generate proxy/apps-routes.json from .devcontainer/devcontainer.json.
Usage: python3 gen-apps-routes.py [output_path]
Run from repo root.
"""
import json, sys

with open('.devcontainer/devcontainer.json') as f:
    dc = json.load(f)

apps = dc['customizations']['codery'].get('apps', [])
routes = [
    {
        "subdomain": app['subdomain'],
        "port": 8080,
        "internal_port": app['internal_port']
    }
    for app in apps
]

output = sys.argv[1] if len(sys.argv) > 1 else 'proxy/apps-routes.json'
with open(output, 'w') as f:
    json.dump(routes, f, indent=2)
    f.write('\n')

print(f'[gen-apps-routes] Wrote {len(routes)} route(s) to {output}')
```

- [ ] **Step 3: Verify the scripts work against current (empty) devcontainer.json**

```bash
cd /tmp/Codery
python3 containers/apps/scripts/gen-supervisor-conf.py /tmp/test-projects.d
ls /tmp/test-projects.d
# Expected: empty directory (no apps defined)

python3 containers/apps/scripts/gen-apps-routes.py /tmp/test-routes.json
cat /tmp/test-routes.json
# Expected: []
```

- [ ] **Step 4: Update `deploy-apps.yml`**

Add these three things to the workflow:

**a) Trigger path:**
```yaml
on:
  push:
    branches: [main]
    paths:
      - 'containers/apps/**'
      - '.devcontainer/devcontainer.json'
  workflow_dispatch:
```

**b) Gen steps before the Docker build step:**
```yaml
      - name: Generate supervisord confs from devcontainer.json
        run: python3 containers/apps/scripts/gen-supervisor-conf.py containers/apps/supervisor/projects.d

      - name: Generate apps-routes.json from devcontainer.json
        run: python3 containers/apps/scripts/gen-apps-routes.py proxy/apps-routes.json
```

**c) After the build-and-push job, a deploy job that syncs apps-routes.json + calls reload-routes:**

Add a `deploy` job (mirror of `deploy-sandbox.yml`'s deploy job):

```yaml
  deploy:
    name: Deploy apps to VPS
    needs: build-and-push
    runs-on: ubuntu-latest
    environment: production

    steps:
      - name: Checkout
        uses: actions/checkout@v4

      - name: Generate apps-routes.json from devcontainer.json
        run: python3 containers/apps/scripts/gen-apps-routes.py proxy/apps-routes.json

      - name: Deploy apps via codery-ci
        env:
          DEPLOY_HOST: ${{ secrets.DEPLOY_HOST }}
        run: |
          trap 'rm -f /tmp/deploy_key' EXIT
          printf '%s\n' "${{ secrets.DEPLOY_SSH_KEY }}" > /tmp/deploy_key
          chmod 600 /tmp/deploy_key
          SSH="ssh -i /tmp/deploy_key -o StrictHostKeyChecking=accept-new deploy@${DEPLOY_HOST}"
          SCP="scp -i /tmp/deploy_key -o StrictHostKeyChecking=accept-new"

          $SSH "sudo mkdir -p /opt/codery/services /opt/codery/proxy"
          cat containers/apps/service.yml | $SSH "sudo tee /opt/codery/services/apps.yml"
          $SCP proxy/apps-routes.json deploy@${DEPLOY_HOST}:/opt/codery/proxy/apps-routes.json

          $SSH "sudo sed -i '/^GHCR_/d' /opt/codery/.env 2>/dev/null || true"
          $SSH "printf 'GHCR_USERNAME=${{ github.actor }}\nGHCR_TOKEN=${{ secrets.GITHUB_TOKEN }}\n' | sudo tee -a /opt/codery/.env" > /dev/null

          $SSH "sudo /opt/codery/codery-ci deploy apps ${{ github.sha }} 2>&1"
```

- [ ] **Step 5: Commit**

```bash
cd /tmp/Codery
git add containers/apps/scripts/gen-supervisor-conf.py \
        containers/apps/scripts/gen-apps-routes.py \
        .github/workflows/deploy-apps.yml
git commit -m "feat(ci): generate apps configs from devcontainer.json, add deploy job to deploy-apps.yml"
```

---

## Task 10: Update AGENTS.md and CLAUDE.md

**Files:**
- Modify: `AGENTS.md`
- Modify: `CLAUDE.md`

- [ ] **Step 1: Add SSH section to `AGENTS.md`**

After the "Your Capabilities" section (or wherever the environment details live), add:

```markdown
## SSH to Apps Container

From sandbox, SSH directly into the apps container with no password or key setup:

```bash
ssh gem@apps
```

Works because:
- `apps` resolves via Docker network alias (`codery-net`)
- Sandbox generates a keypair at startup; apps sshd reads it automatically
- Only works from inside the sandbox container (Docker network boundary = security)

Once SSH'd in, you can run app processes, inspect logs, or debug:
```bash
ssh gem@apps "supervisorctl -c /etc/supervisor/projects.conf status"
ssh gem@apps "tail -f /var/log/supervisor/myapp.log"
```
```

- [ ] **Step 2: Add devcontainer.json section to `AGENTS.md`**

```markdown
## Adding a New App

Apps are declared in `.devcontainer/devcontainer.json` under `customizations.codery.apps`.

### To add an app:

1. Edit `.devcontainer/devcontainer.json`:
```json
{
  "customizations": {
    "codery": {
      "apps": [
        {
          "name": "myapp",
          "subdomain": "myapp",
          "internal_port": 3001,
          "command": "bun run start",
          "directory": "/home/gem/projects/myapp",
          "env": {}
        }
      ]
    }
  }
}
```

2. Push to `main` — triggers `Build Apps` workflow (~8 min):
   - Bakes supervisord conf into image (process management)
   - Syncs `apps-routes.json` to VPS (subdomain routing)
   - Orchestrator reloads Caddy + Nginx (routes take effect)

3. Your app is live at `myapp.{DOMAIN_NAME}` once deploy completes.

**Field reference:**
- `name` — supervisord program name (no spaces)
- `subdomain` — DNS subdomain (or full FQDN)
- `internal_port` — port your app process listens on inside the container (8000–9000 range, or any unused port)
- `command` — start command (run from `directory`)
- `directory` — working directory
- `env` — extra env vars (on top of `.env`)
```

- [ ] **Step 3: Update `CLAUDE.md` with SSH + devcontainer.json documentation**

In the "Container Roles" section, update Apps to mention gem user and sshd. Add a new section "Adding a New App" parallel to "Adding a new container service":

```markdown
### Adding a new web app to the apps container

Apps are declared in `.devcontainer/devcontainer.json`. Pushing that file triggers a full apps image rebuild + deploy.

**Append to `customizations.codery.apps` array:**
```json
{
  "name": "myapp",
  "subdomain": "myapp",
  "internal_port": 3001,
  "command": "bun run start",
  "directory": "/home/gem/projects/myapp",
  "env": {}
}
```

CI generates from this:
- `containers/apps/supervisor/projects.d/myapp.conf` — baked into apps image, supervisord starts the process
- `proxy/apps-routes.json` — `{"subdomain":"myapp","port":8080,"internal_port":3001}` — synced to VPS
- `/opt/codery/proxy/apps-nginx.conf` — Nginx routes `myapp.{DOMAIN} → localhost:3001` inside apps container

**Two-speed updates:**
- New app (process needed): edit `devcontainer.json` → push → full deploy (~8 min)
- Route-only change: edit `proxy/apps-routes.json` directly → push → Sync Routes (~30s, no rebuild)

### SSH from sandbox to apps

```bash
ssh gem@apps    # from inside sandbox — works with no flags, no credentials
```

The sandbox generates a fresh keypair at startup. Apps sshd reads the public key from the shared volume on each connection attempt. Security boundary: Docker network (`codery-net`).
```

- [ ] **Step 4: Commit**

```bash
cd /tmp/Codery
git add AGENTS.md CLAUDE.md
git commit -m "docs: document SSH sandbox→apps, devcontainer.json app workflow"
```

---

## Task 11: End-to-end validation

- [ ] **Step 1: Rebuild and push the Codery orchestrator binary**

The orchestrator code changed (nginx.rs, network_aliases). Tag and deploy:

```bash
cd /tmp/Codery
git tag codery-ci-v0.1.$(date +%Y%m%d%H%M)
git push origin master --tags
# Wait for release workflow, then trigger build-orchestrator.yml
gh workflow run build-orchestrator.yml --ref master
```

- [ ] **Step 2: Rebuild and push the apps image**

```bash
gh workflow run deploy-apps.yml --ref master
```

- [ ] **Step 3: Rebuild and push the sandbox image**

```bash
gh workflow run deploy-sandbox.yml --ref master
```

- [ ] **Step 4: Test SSH from sandbox**

Once both containers are running:
```bash
# From inside the sandbox container (via OpenCode terminal or ttyd):
ssh gem@apps echo "SSH works"
# Expected: "SSH works" printed

ssh gem@apps supervisorctl -c /etc/supervisor/supervisord.conf status
# Expected: process list (nginx, sshd at minimum)
```

- [ ] **Step 5: Test adding an app end-to-end**

Add a test app to `.devcontainer/devcontainer.json`:
```json
{
  "name": "hello",
  "subdomain": "hello",
  "internal_port": 8001,
  "command": "bash -c 'while true; do echo -e \"HTTP/1.1 200 OK\\r\\nContent-Length: 5\\r\\n\\r\\nhello\" | nc -l 8001; done'",
  "directory": "/home/gem/projects",
  "env": {}
}
```

Push, wait for deploy, then:
```bash
curl -H "Host: hello.$(cat /run/tailscale.ip)" http://localhost:8080/
# Expected: "hello"
```

Remove test app after verification.

---

## Self-Review

**Spec coverage check:**
- ✅ SSH sandbox→apps with no user credentials (Tasks 4, 5, 8)
- ✅ `ssh gem@apps` works via Docker network alias (Tasks 3, 8)
- ✅ Internal Nginx proxy in apps container on port 8080 (Task 6)
- ✅ Nginx config bind-mounted + reloaded by orchestrator (Tasks 7, 8)
- ✅ `internal_port` field in AppRoute drives Nginx routing (Task 7)
- ✅ devcontainer.json as source of truth (Task 1)
- ✅ launchy.json generated from devcontainer.json (Task 2)
- ✅ supervisord confs generated from devcontainer.json (Task 9)
- ✅ apps-routes.json generated from devcontainer.json (Task 9)
- ✅ deploy-apps.yml trigger on devcontainer.json change (Task 9)
- ✅ AGENTS.md + CLAUDE.md updated (Task 10)
- ✅ ensure_nginx_config creates host file pre-deploy (Task 8)
- ✅ Orchestrator rebuilt and deployed (Task 11)
