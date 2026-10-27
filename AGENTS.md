# Codery — Infrastructure Reference

This file is loaded automatically by Claude Code and compatible agents (as AGENTS.md). Read it before making
any changes to this repo.

---

## What This Repo Is

Codery is the complete infrastructure for a VPS-hosted developer environment. It manages:

- **Paseo container** — the AI coding environment (paseo.sh daemon + OpenCode CLI)
- **Apps container** — project web servers (Bun/Node apps)
- **OpenDesign container** — design workspace (upstream image + pinned wrapper)
- **Playwright container** — dedicated browser runtime (the only place Chromium runs)
- **Host layer** — Caddy (reverse proxy + TLS), Tailscale (the perimeter), supervisord
- **CoderyCI** — Rust binary that runs on the host: blue/green deployments, routing
  generation, health checks, host metrics, diagnostics, MCP control plane

All external traffic enters through Tailscale, is routed by Caddy, and hits the active color
of whichever container serves that subdomain. Nothing is publicly exposed.

The authoritative architecture reference is [ARCHITECTURE.md](ARCHITECTURE.md).

---

## Common Operations — Quick Reference

| Situation | Command / Tool |
|-----------|----------------|
| **Deploy a change (default)** | **Local build** — `codery_exec` MCP: `build` → `deploy-preview` → ask user to `cutover`. Avoid the GitHub pipeline unless explicitly asked. |
| Something broken, unknown cause | `codery-ci diagnose` (or `diagnose` MCP tool) |
| Routing points to dead container | `codery-ci cutover <service>` |
| Promote a verified preview | `codery-ci cutover <service>` |
| Abort a preview deploy | `codery-ci cancel-preview <service>` |
| Build + try a new image locally | `codery-ci build <svc> <tag>` → `deploy-preview` → `cutover` |
| Upgrade Playwright (pull-only) | `codery-ci deploy playwright <version>` — bump MCP pin + image tag together |
| Reload routing after YAML edit | `codery-ci reload-routes` (or `reload_routes` MCP) |
| Restart a stuck container | `restart_service` MCP (no blue/green swap) |
| Roll back to the previous image | `rollback` MCP |
| State vs reality disagree | `codery-ci diagnose` shows the mismatch + fix |

Each subcommand has focused help: `codery-ci <command> --help`.

---

## Container Roles

### Paseo (`containers/paseo/`)

**Purpose:** the AI coding environment. The iPad Paseo app (or any Paseo client) talks
to this daemon over WebSocket; it spawns OpenCode agents on the shared projects tree.
Child of the official `ghcr.io/getpaseo/paseo` image (pinned), adding OpenCode, git
tooling with GitHub App auth, and a hardened sshd.

**Runs:** daemon on container port **6767** (subdomain `code.`, loopback publish);
**sshd on container port 2023**, published as stable host port **2222** — Tailscale-only
(`bind: tailnet`), key-only auth, no root login, `AllowUsers paseo`.

**Key mounts:**
- `/opt/codery/projects` → `/workspace` — shared project files
- `/opt/codery/data/opencode` → `/home/paseo/.local/share/opencode` — OpenCode state
- repo `containers/paseo/opencode-config/` → `/home/paseo/.config/opencode` — git-managed
  global agent config (this container's own instructions live here)
- `/opt/codery/ssh/authorized_keys` → `/run/secrets/authorized_keys` — host-managed keys
- `/opt/codery/ssh/agent-to-apps` → `/home/paseo/.ssh/agent-to-apps` — client key for
  `ssh gem@apps` (nix toolchain inside the apps container)

**What it cannot do:** No Docker socket, no Caddy/Tailscale access, no host supervisor.
Route and infra changes must go through this repo.

### Apps (`containers/apps/Dockerfile`)

**Purpose:** runs project web applications.

**Runs:** any number of Bun/Node servers, supervised by s6-overlay. Nginx on port 8080
routes by Host header; **sshd on container port 2022**, published as stable host
port **3333** (Tailscale-only) for operators.

**Port range:** apps listen on **8000-9000** inside the container. This is the only port
range CoderyCI maps for the bulk range. Do not use ports outside it.

### OpenDesign (`containers/opendesign/`)

**Purpose:** design workspace. Upstream image `ghcr.io/nexu-io/od` pinned; the wrapper
adds the OpenCode CLI so OpenDesign can spawn agents. Data (`.od` project store) is the
host bind mount `/opt/codery/data/opendesign`.

**Runs:** web on container port **7456** (subdomain `opendesign.`, loopback publish,
basic-auth gate from `OPENDESIGN_BASIC_AUTH`).

### Playwright (`containers/playwright/service.yml` — pull-only)

Microsoft's official browser image, used as-is. Chromium runs **only** here — never
install browser deps into other containers; that is the anti-pattern this service
exists to eliminate. Version coupling (`@playwright/mcp` + image tag) is documented in
[docs/playwright.md](docs/playwright.md).

### Host layer

Caddy (TLS + subdomain routing), Tailscale (the only perimeter), supervisord
(caddy, tailscale, codery-ci daemon + MCP). Host services are configured in this
repo under `proxy/`.

---

## Host Memory & OOM Prevention

The host has 8 GiB RAM and (unless swap was added) **no swap**. The Sep 16 2026
incident (docker build + apps preview + a bloated opencode ⇒ 13 kernel OOM kills in
one evening) produced hard rules; see ARCHITECTURE.md for the measured budget.

1. **Add swap on the host** (one-time) — see SETUP.md.
2. **Never run `codery-ci build` while a preview is up** — cancel previews promptly.
3. **Keep playwright stopped when browser tools aren't needed** — chromium is the
   single largest consumer when actively used.
4. Diagnosing 502 storms: `cat /sys/fs/cgroup/memory.events` inside a container
   (`oom_kill` counter), `grep -E 'MemAvailable|SwapTotal' /proc/meminfo`.

---

## Port Scheme

`host_port = container_port + offset(color)`; offsets come from each service's YAML
`port_scheme` (never hardcoded per service name).

| Service | Blue offset | Green offset | Ports |
|---------|-------------|--------------|-------|
| paseo   | 10000 | 20000 | daemon 6767 → `code.`; ssh 2023 → stable **:2222** |
| opendesign | 10000 | 20000 | web 7456 → `opendesign.` |
| apps    | 0 | 10000 | nginx 8080; range 8000-9000; ssh 2022 → stable **:3333** |
| playwright | 10000 | 20000 | ws 3000 (internal only) |

**Stable SSH ports**: the daemon's TCP proxy forwards host **:2222 → active paseo** and
**:3333 → active apps**, so operators always reach the live container. Both bind the
Tailscale IP only.

### How Caddy knows which color is active

CoderyCI writes the active color to `/opt/codery/state/{service}.color` and regenerates
`/etc/caddy/Caddyfile` (reload on every deploy / `reload-routes`).

---

## Service Declarations (Declarative Infrastructure)

Each container service is declared in a `service.yml` file inside `containers/<name>/` —
the canonical source of truth. Deploy workflows sync it to `/opt/codery/services/<name>.yml`
**before** calling `codery-ci deploy` (ordering invariant — CoderyCI reads it at deploy time).

### YAML schema

```yaml
service: myservice
image: ghcr.io/coderyoss/codery:{sha}  # {sha} substituted at deploy time

# Port formula: host_port = container_port + offset(color)
port_scheme:
  blue_offset: 10000
  green_offset: 20000

# Discrete named ports — each may have a subdomain and/or a stable proxy port
ports:
  - name: web
    container_port: 6767
    subdomain: code                    # optional: Caddy vhost
    bind: loopback | tailnet | any     # publish scope (default any; prefer loopback/tailnet)
    auth:
      env: BASIC_AUTH_VAR              # optional: Caddy basic-auth from /opt/codery/.env
  - name: ssh
    container_port: 2022
    fixed_port: 2222                   # optional: stable-port TCP proxy target
    bind: tailnet

# OR: bulk Docker binding for a port range (apps-style)
port_range:
  container_start: 8000
  container_end: 9000
  bind: loopback

volumes:
  - type: bind
    host: /opt/codery/projects
    container: /workspace
  - type: named
    name: my-volume
    container: /data
  - type: bind
    host: "${SSH_DIR}"                 # ${VAR} substituted from /opt/codery/.env
    container: /home/gem/.ssh
    readonly: true

env_overrides:                         # applied on top of /opt/codery/.env
  MY_KEY: /path/in/container

required_env:                          # validation fails (nothing changes) if missing
  - PASEO_PASSWORD

network: codery-net

health_check:
  type: tcp                            # TCP connect to the named port
  port: web
  timeout_secs: 60
  interval_secs: 2
# OR: { type: docker, timeout_secs: 90 } for images with a HEALTHCHECK
```

### Pre-deploy validation

Before touching any container, CoderyCI validates: required env keys exist, bind-mount
host paths exist, volumes can be created, the image is pullable, and inactive-color host
ports aren't owned by foreign processes. **If any check fails, nothing changes.**

Dry-run: `codery-ci validate <service> <sha>`.

---

## Apps container — 3-tier app lifecycle

Apps are s6-supervised; `/opt/codery/codery.db` (`apps` table) is the source of truth
with a `source` column (`build` | `runtime`):

| Tier | What | How it runs |
|------|------|-------------|
| 1. Build-time | Bundles in `containers/apps/s6-overlay/s6-rc.d/<name>/` (+ `user-bundles.d/user/contents.d/<name>` link) | Baked into the image; auto-starts |
| 2. Previous session | `codery-ci sync_s6` renders bundles to `/opt/codery/apps-s6.d` (bind-mounted) | Boot-time oneshot `runtime-apps` links them |
| 3. Current session | `add_app` MCP | SQLite insert → render → `s6-svlink` → poll until up |

**Rules:**
- Restart an app: `restart_app` (s6-svc -t; route preserved). **NEVER**
  `remove_app` + `add_app` to restart — that drops and recreates the route.
- Route-only change: edit routes (SQLite via MCP, or `proxy/routes.yaml` for host
  routes) → `reload_routes` (instant, no rebuild).
- App logs: `get_container_info service='apps'` (container stdout).

---

## SSH Access

| Path | Command | Notes |
|------|---------|-------|
| Operator → paseo | `ssh -p 2222 paseo@<host>` | Tailscale only; keys in `/opt/codery/ssh/authorized_keys` |
| Operator → apps | `ssh -p 3333 gem@<host>` | Tailscale only; same key file |
| paseo → apps | `ssh gem@apps` | Docker-network internal; agent-to-apps keypair; nix toolchain lives there |

Connection chain for stable ports: `client → :2222/:3333 (codery-ci TCP proxy) →
:1xx22/:xx22 (docker publish) → :2022 (sshd)`. The proxy reads the active color and
forwards to the live container.

Adding a device: `echo 'ssh-ed25519 AAAA...' >> /opt/codery/ssh/authorized_keys` —
no rebuild. Host keys are persistent volumes (stable fingerprints).

---

## Adding a new container service

1. `containers/<name>/service.yml` (+ `Dockerfile` if not pull-only)
2. `.github/workflows/deploy-<name>.yml` — copy `deploy-paseo.yml` as the template;
   the YAML sync MUST come before `codery-ci deploy`
3. `codery-ci validate <service> <sha>` to dry-run
4. Push to `main`, run the workflow manually

No orchestrator code changes needed — offsets/ports come from the YAML.

### Removing a service

1. `codery-ci cancel-preview <service>` if applicable
2. `docker stop codery-<service>-<active>`; `delete_service` MCP removes the YAML
3. Delete `containers/<service>/` from the repo and push
4. `codery-ci reload-routes`

---

## Blue/Green Deployment

**Deploy flow:** pull new image → start inactive color → health check → **cutover**
(rewrite Caddyfile, reload, write active color) → stop old container → prune images.

**No automated rollback after cutover.** Preview deploys (`deploy-preview` → verify →
`cutover`) are the verify-before-promote path; `codery-ci rollback` redeploys the
previous locally-cached image.

**Local build workflow (agent, via MCP):**

```
1. mcp_exec_enabled                    → confirm toggle is on
2. codery_exec ["build", "<service>", "<tag>"]   → job_id
3. codery_exec_status job_id           → poll until done
4. codery_exec ["deploy-preview", "<service>", "<tag>"]
   → verify at https://<service>-preview.{DOMAIN}
5. Ask the user: run `codery-ci cutover <service>` on the host
   (cutover/deploy are NEVER exposed via MCP — humans run those)
```

`<tag>` can be anything descriptive; images stay in the local Docker cache, which
also powers `rollback`.

**State files:** `/opt/codery/state/{service}.color|.sha`; previews live in
`codery.db`. `codery-ci diagnose` reconciles everything against reality.

---

## CI/CD Triggers

**All deploy workflows are manual-only (`workflow_dispatch`).** Pushing to `main`
never deploys.

| Workflow | Trigger | What it does |
|----------|---------|--------------|
| CI (`ci.yml`) | push / PR | fmt + clippy + tests + cargo-deny (blocking) |
| Build Paseo (`deploy-paseo.yml`) | `workflow_dispatch` | Builds + deploys paseo (blue/green) |
| Build Apps (`deploy-apps.yml`) | `workflow_dispatch` | Builds + deploys apps |
| Build OpenDesign (`deploy-opendesign.yml`) | `workflow_dispatch` | Builds + deploys opendesign |
| Deploy Playwright (`deploy-playwright.yml`) | `workflow_dispatch` | Pull-only deploy of the pinned MS image |
| Build Orchestrator (`build-orchestrator.yml`) | `workflow_dispatch` | Compiles codery-ci, deploys binary, restarts daemon/MCP |
| Sync Routes (`sync-routes.yml`) | `workflow_dispatch` | `reload-routes` (~30s, no rebuild) |
| Diagnose (`diagnose.yml`) | `workflow_dispatch` | Collects system + container logs |
| Repair Host (`setup-host.yml`) | `workflow_dispatch` | Idempotent host-services repair |
| Release Orchestrator (`release-orchestrator.yml`) | tag `codery-ci-v*` | cross-compiled musl binaries (x86_64 + aarch64) |
| Release Apps (`release-apps.yml`) | tag `apps-v*` | apps image to GHCR |

---

## Project Structure

```
AGENTS.md                   # This file — agent-facing reference
ARCHITECTURE.md             # Durable architecture doc (start here to understand the system)
README.md                   # Project front page
SETUP.md                    # VPS installation guide

containers/
  paseo/                    # Agent container (Dockerfile, service.yml, sshd_config,
    scripts/                #   entrypoint wrapper; github-app auth scripts)
    opencode-config/        # git-managed global agent config (AGENTS.md, MCPs)
  apps/                     # Project web servers (Dockerfile, service.yml, s6-overlay/)
  opendesign/               # Design workspace (wrapper Dockerfile, service.yml)
  playwright/service.yml    # Pinned MS browser image (pull-only)

proxy/                      # Host layer: Caddy/Tailscale/supervisor configs, routes.yaml
system/orchestrator/        # CoderyCI source (Rust) + ui/ (React dashboard)
hosting/                    # cloud-init + provider guides
docs/                       # customizing.md, playbook.md, playwright.md
.github/workflows/          # Manual pipelines (see table above)
```

---

## Host Environment

| Path | Purpose |
|------|---------|
| `/opt/codery/` | Root of all Codery state |
| `/opt/codery/.env` | Secrets and config (GHCR creds, API keys, GitHub App ID, passwords) |
| `/opt/codery/state/` | Active color + SHA per service |
| `/opt/codery/projects/` | Shared project files (bind-mounted into agent containers) |
| `/opt/codery/ssh/` | authorized_keys + agent-to-apps keypair |
| `/opt/codery/services/` | Synced service YAML definitions |
| `/opt/codery/codery-ci` | The CoderyCI binary |
| `/etc/caddy/Caddyfile` | Live Caddyfile (written by CoderyCI on each deploy) |

## Sensitive Files — Do Not Read or Expose

These files contain secrets. NEVER read, cat, print, or include their contents in output:

- `.env`, `*.env.*` — API keys, tokens, credentials
- `/run/secrets/*` — GitHub App PEM key (bind-mounted)
- `~/.ssh/*`, `/opt/codery/ssh/agent-to-apps` — private keys
- `~/.local/share/opencode/auth.json` — provider API keys
- `*.pem`, `*.key` — any private key files

Check existence only (`test -f`), never read contents.
