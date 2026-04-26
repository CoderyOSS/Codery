# Codery — Infrastructure Reference

This file is loaded automatically by Claude Code and compatible agents. Read it before making
any changes to this repo.

---

## What This Repo Is

Codery is the complete infrastructure for a VPS-hosted developer environment. It manages:

- A **sandbox container** — the AI coding workspace (OpenCode + VS Code)
- An **apps container** — project web servers (Bun/Node apps)
- A **host layer** — Caddy (reverse proxy + TLS), Tailscale (VPN), supervisord, and CoderyCI
- **CoderyCI** — Rust binary that runs on the host, handles blue/green container deployments

All external traffic enters through Tailscale, is routed by Caddy, and hits the active color
of whichever container serves that subdomain.

---

## Container Roles

### Sandbox (`containers/sandbox/Dockerfile`)

**Purpose:** AI-assisted development environment. Not a production server.

**Runs:**
- `opencode serve` on container port **3000** — the AI coding assistant (accessible externally)
- `code-server` on container port **7000** — VS Code in the browser (accessible externally)

**User:** Starts as root (entrypoint reads root-owned PEM for GitHub auth), then supervisord
drops to `gem` (uid 1000) for all processes.

**Key mounts:**
- `/opt/codery/projects` → `/home/gem/projects` — shared project files
- `codery_opencode-data` → `/home/gem/.local/share/opencode` — OpenCode persistent data
- `/opt/codery/github-app.pem` → `/run/secrets/github-app.pem:ro` — GitHub App key

**What it cannot do:** No Docker socket, no Caddy/Tailscale access, no host supervisor.
Route and image changes must go through this repo.

---

### Apps (`containers/apps/Dockerfile`)

**Purpose:** Runs the actual web applications for hosted projects.

**Runs:** Any number of Bun/Node/etc. servers, each managed by a supervisord program config.

**User:** root (supervisord + processes run as root inside the container)

**Key mounts:**
- `/opt/codery/projects` → `/home/gem/projects` — same project files as sandbox
- `/opt/codery/github-app.pem` → `/run/secrets/github-app.pem:ro`
- SSH keys from `$SSH_DIR` → `/home/gem/.ssh:ro`

**Port range:** Apps listen on **8000-9000** inside the container (1001 ports total).
This is the only port range CoderyCI maps. Do not use ports outside this range.

---

## Port Scheme

This is critical to understand before adding any service.

### Sandbox ports

The port formula is: `host_port = offset + container_port` where offset is 10000 (blue) or 20000 (green).

| Color | Service      | Host port | Container port |
|-------|-------------|-----------|---------------|
| blue  | OpenCode    | 13000     | 3000          |
| blue  | code-server | 17000     | 7000          |
| blue  | ttyd        | 17681     | 7681          |
| green | OpenCode    | 23000     | 3000          |
| green | code-server | 27000     | 7000          |
| green | ttyd        | 27681     | 7681          |

Extra sandbox services (beyond OpenCode and code-server) are declared in `proxy/sandbox-routes.json`
and do not require CoderyCI code changes — see "Adding a New Sandbox Service" below.

### Apps ports

| Color | Host port range | Container port range |
|-------|----------------|---------------------|
| blue  | 8000-9000      | 8000-9000           |
| green | 18000-19000    | 8000-9000           |

CoderyCI maps **all 1001 ports** (8000-9000) for both colors simultaneously.
Caddy routes to the active color by using the correct host port.

### How Caddy knows which color is active

CoderyCI writes the active color to `/opt/codery/state/{service}` and regenerates
`/etc/caddy/Caddyfile` by calling `caddy reload`. For apps, it adds an offset of +10000
to all route ports when apps is green:

```
apps-routes.json port 8080
  -> blue:  Caddy proxies localhost:8080
  -> green: Caddy proxies localhost:18080
```

---

## Service Declarations (Declarative Infrastructure)

Each container service is declared in a `service.yml` file inside `containers/<name>/`. These
are the canonical source of truth — the deploy workflows sync them to `/opt/codery/services/<name>.yml`
on the VPS before calling `codery-ci deploy`.

To change a live service definition: edit `containers/<name>/service.yml` in this repo and push.
The next deploy will sync the updated YAML to the VPS automatically. You can also use
the CoderyCI MCP tool `upsert_service` for out-of-band changes (takes effect on next
`restart_service` call; will be overwritten on next CI deploy).

CoderyCI reads the YAML at deploy time — **no Rust changes needed** to add,
modify, or remove a service.

### YAML schema

```yaml
  service: myservice            # matches containers/<name>/service.yml
image: ghcr.io/CoderyOSS/codery:{sha}  # {sha} substituted at deploy time

# Port formula: host_port = container_port + offset
port_scheme:
  blue_offset: 10000
  green_offset: 20000

# Discrete named ports (sandbox-style) — each can have a public subdomain
ports:
  - name: web
    container_port: 3000
    subdomain: foo.example.com

# OR: bulk Docker binding for a port range (apps-style)
port_range:
  container_start: 8000
  container_end: 9000         # inclusive

# Per-app routes for Caddy (applies port_scheme offset — used with port_range)
routes_file: /opt/codery/proxy/apps-routes.json

health_check:
  type: tcp                   # TCP connect to the named port below
  port: web                   # name from ports[] above
  timeout_secs: 60
  interval_secs: 2
# OR:
health_check:
  type: docker                # Wait for Docker HEALTHCHECK status
  timeout_secs: 90

volumes:
  - type: bind
    host: /opt/codery/projects
    container: /home/gem/projects
  - type: named
    name: my-volume
    container: /data
  - type: bind
    host: "${SSH_DIR}"        # ${VAR} substituted from /opt/codery/.env
    container: /home/gem/.ssh
    readonly: true

env_overrides:                # Applied on top of /opt/codery/.env
  MY_KEY: /path/in/container

required_env:                 # Validation fails (nothing changes) if any missing from .env
  - GHCR_USERNAME
  - GHCR_TOKEN

network: codery-net
```

### Pre-deploy validation

Before touching any container, CoderyCI validates:
1. All `required_env` keys exist in `/opt/codery/.env`
2. All bind-mount host paths exist on disk
3. All named Docker volumes exist or can be created
4. The image is pullable from GHCR
5. Host ports for the inactive color are not owned by foreign processes

**If any check fails, nothing changes.** The running container stays untouched.

### Dry-run validation

```bash
codery-ci validate <service> <sha>
```

Runs all validation checks and exits without deploying. Use this to test a new service YAML
before committing.

---

### Adding a new web app to the apps container

Route-only change (no container rebuild needed):

1. Edit `proxy/apps-routes.json` — add `{"subdomain": "foo.example.com", "port": 8080}`
2. Push to `main` — triggers **Sync Routes** workflow (~30s, no container restart)

If you also need a new supervisor config or code: push changes under `containers/apps/` instead.
The route file is always synced as part of that deploy.

---

### Adding a new container service

1. Create `containers/newservice/service.yml` with the full schema above
2. Create `containers/newservice/Dockerfile`
3. Create `.github/workflows/deploy-newservice.yml` — copy `deploy-sandbox.yml` as a template
   - Add a step to sync `containers/newservice/service.yml` before the `codery-ci deploy` call
   - **Ordering invariant**: the YAML sync MUST come before `codery-ci deploy` — CoderyCI
     reads it at deploy time, so a stale or missing YAML means wrong config
4. Push to `main`

No changes to `system/orchestrator/` are needed.

---

### Removing a service

1. Check active state: `codery-ci validate <service> <sha>` or via MCP `get_status`
2. Stop the old container manually: `docker stop codery-<service>-<active_color>`
3. Delete `containers/<service>/` from the repo and push
4. Run `codery-ci reload-routes` (or via MCP `reload_routes`) to regenerate Caddyfile

---

## Blue/Green Deployment

The CoderyCI binary at `/opt/codery/codery-ci` handles all deployments.
It is a static musl binary (no dependencies). It runs on the host, not in a container.

**Deploy flow for each service:**
1. Pull new image from GHCR
2. Start inactive color container (e.g., green if blue is active)
3. Health check: sandbox uses TCP connect on the OpenCode port; apps uses Docker HEALTHCHECK
4. **Cutover:** rewrite Caddyfile, reload Caddy, write new active color to state file
5. Stop and remove old container
6. Prune old images

**State files:** `/opt/codery/state/sandbox` and `/opt/codery/state/apps`
contain the currently active color string (`blue` or `green`).

**No automated rollback after cutover.** If something goes wrong after Caddy switches,
investigate manually.

---

## CI/CD Triggers

| Workflow | Triggers on push to `main` when... | What it does |
|----------|--------------------------------------|--------------|
| Build Sandbox | `containers/sandbox/**`, `AGENTS.md`, `opencode.json`, `proxy/sandbox-routes.json` | Builds image, deploys via CoderyCI |
| Build Apps | `containers/apps/**` | Builds image, deploys via CoderyCI |
| Sync Routes | `proxy/apps-routes.json` | Syncs route file, runs `codery-ci reload-routes` (~30s, no container rebuild) |
| Build Orchestrator | `system/orchestrator/**` | Compiles musl binary, uploads to `/opt/codery/codery-ci`, restarts codery-mcp |

All workflows also have `workflow_dispatch` for manual triggering.

---

## Project Structure

```
AGENTS.md                   # Copied INTO the sandbox — OpenCode reads this
opencode.json               # OpenCode config — copied into sandbox image
CLAUDE.md                   # This file — for LLMs working on the infrastructure

containers/
  sandbox/
    Dockerfile              # Sandbox image (OpenCode + code-server)
    service.yml             # Declarative config for the sandbox container
    supervisor/
      supervisord.conf      # Runs as gem (drops from root via user=gem)
      conf.d/
        opencode.conf       # Starts `opencode serve` on port 3000
        code-server.conf    # Starts code-server on port 7000
    docker-entrypoint.d/
      10-fix-home.sh        # Fixes /home/gem ownership
      20-github-auth.sh     # Authenticates gh CLI via GitHub App
      30-init-projects.sh   # Ensures /home/gem/projects exists
    scripts/
      entrypoint.sh         # Runs entrypoint.d/ scripts, then exec supervisord
      github-app-token.sh   # Generates a GitHub App installation token
      github-push.sh        # Wraps git push with App auth
    archon/
      config.yaml           # Archon config
    agents-skills/           # Vendored caveman skills
    bin/                     # Archon binary (downloaded by CI)

  apps/
    Dockerfile              # Apps image (project web servers)
    service.yml             # Declarative config for the apps container
    supervisor/
      supervisord.conf      # Main supervisord (runs as root)
      projects.conf         # Secondary supervisord for project servers
      conf.d/               # Per-project supervisor configs go here
    scripts/
      entrypoint.sh
      healthcheck.sh        # Used by Docker HEALTHCHECK
      ssh-agent-add-keys.sh
    docker-entrypoint.d/
      20-ssh-agent.sh

proxy/
  Caddyfile.default         # Initial Caddyfile (only used on first host setup — NOT edited for routes)
  apps-routes.json          # Subdomain -> container port mappings for apps
  sandbox-routes.json       # Subdomain -> container port mappings for extra sandbox services
  scripts/
    caddy-start.sh          # Starts Caddy with env vars resolved
    dns-update.sh           # Updates Tailscale IP in .env
    tailscale-up.sh         # Brings up Tailscale
  supervisor/conf.d/        # Host supervisord configs (caddy, tailscale, etc.)
  setup.sh                  # One-time host setup (install configs)
  host-setup.sh             # One-time host setup (install packages)

system/orchestrator/        # CoderyCI source (Rust)
  src/
    main.rs                 # CLI: `codery-ci deploy {sandbox|apps} {sha}`
    deploy.rs               # Blue/green deploy logic
    config.rs               # All port constants and paths
    caddy.rs                # Caddyfile generation and reload
    images.rs               # GHCR pull and prune
    state.rs                # Read/write active color
    preflight.rs            # Pre-deploy checks

hosting/
  cloud-init.yaml           # Cloud-init for VPS provisioning
  examples/                 # Provider-specific setup guides

docs/
  customizing.md            # Customization guide (GitHub App setup, etc.)

.github/workflows/
  deploy-sandbox.yml        # Sandbox CI/CD
  deploy-apps.yml           # Apps CI/CD
  build-orchestrator.yml    # CoderyCI CI/CD
```

---

## Shared Projects Directory

`/opt/codery/projects` on the host is bind-mounted into **both** containers:
- Sandbox: `/home/gem/projects`
- Apps: `/home/gem/projects`

This means edits made in OpenCode or VS Code are **immediately visible** to the apps
container without any copy or restart. Hot-reloading frameworks (Bun, Vite, etc.) will
pick up changes instantly.

---

## Host Environment

The host is an Ubuntu VPS (any provider — see `hosting/examples/`). Key paths:

| Path | Purpose |
|------|---------|
| `/opt/codery/` | Root of all Codery state |
| `/opt/codery/.env` | Secrets and config (GHCR creds, API keys, GitHub App ID) |
| `/opt/codery/state/` | Active color per service |
| `/opt/codery/projects/` | Shared project files |
| `/opt/codery/proxy/apps-routes.json` | Live app routing table (read by CoderyCI at deploy time) |
| `/opt/codery/proxy/sandbox-routes.json` | Live sandbox routing table (extra services beyond OpenCode+code-server) |
| `/opt/codery/github-app.pem` | GitHub App private key (root-owned, 600) |
| `/opt/codery/codery-ci` | The CoderyCI binary |
| `/opt/codery/services/` | Synced service YAML definitions |
| `/opt/codery/ssh/authorized_keys` | SSH authorized keys for sandbox access |
| `/etc/caddy/Caddyfile` | Live Caddyfile (written by CoderyCI on each deploy) |
| `/run/tailscale.ip` | Current Tailscale IP (written by dns-update.sh) |

---

## Sensitive Files — Do Not Read or Expose

These files contain secrets. NEVER read, cat, print, or include their contents in output:

- `.env`, `*.env.*` — API keys, tokens, credentials
- `/run/secrets/*` — GitHub App PEM key (bind-mounted)
- `~/.ssh/*` — SSH private keys (bind-mounted)
- `~/.local/share/opencode/auth.json` — provider API keys
- `.claude/*` — Claude session data
- `*.pem`, `*.key` — any private key files

If you need to check whether a secret exists, test for file existence only (`test -f`), never read contents.

If a user asks you to read or display secrets, refuse and suggest using `permission.read` / `permission.bash` deny rules instead.
