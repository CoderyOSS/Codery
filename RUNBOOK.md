# RUNBOOK — Bootstrap Codery on a fresh VPS

**Audience:** an AI agent (OpenMINIS-class) with root SSH on a fresh VPS, this
repo, and operator-staged secrets. Human version: [SETUP.md](SETUP.md).

Execute phases in order. Every phase has a gate — do not start the next phase
until the gate passes. Idempotent where possible; if a step fails, fix and
re-run it, never skip.

## Phase 0 — Contracts

**You receive:**

- root SSH to a fresh Ubuntu/Debian 24.x VPS (no other software assumed)
- this repo, cloned to the VPS at `/root/Codery` (or readable anywhere)
- secrets staged at `/root/secrets.env` in `KEY=value` form — the key set is
  exactly [`host/templates/env.example`](host/templates/env.example). If a key
  is missing, STOP and ask the operator; do not invent values.

**You deliver:**

- a running system: Tailscale perimeter, Dockhand management plane, Zoraxy
  routing edge (wildcard TLS), the devtools stack, operator access verified
- the operator's first task after handoff: change passwords, review Zoraxy
  routes

**Operator decisions to collect before starting** (ask once):

- domain name (default: `rancidgrandmas.online`)
- which opt-in tools to deploy: `COMPOSE_PROFILES` list (default `browser,db`)

## Phase 1 — CORE

### 1.1 Host layer

```bash
sudo bash /root/Codery/host/bootstrap.sh /root/secrets.env
```

The script is idempotent: installs docker + supervisord + tailscale, sets the
tailnet-bind sysctl, builds the `/opt/codery` tree, assembles
`/opt/codery/.env` from staged secrets, installs host scripts and supervisor
confs, brings up tailscale. Read it before running; it prints a gate line per
step.

**Gate C1:** `tailscale ip` returns an IP; `docker info` works;
`ls /opt/codery/.env /opt/codery/stacks` all exist; `cat /run/tailscale.ip`
returns the same IP.

### 1.2 Dockhand

```bash
mkdir -p /opt/codery/stacks/dockhand
cp /root/Codery/stacks/core/dockhand/compose.yml /opt/codery/stacks/dockhand/
cat > /opt/codery/stacks/dockhand/.env <<EOF
DOCKHAND_IMAGE=ghcr.io/finsys/dockhand:latest
EOF
docker compose -f /opt/codery/stacks/dockhand/compose.yml \
  --env-file /opt/codery/stacks/dockhand/.env up -d
```

First-run: open `http://<tailnet-ip>:23000`, create the admin account, mint an
API token (`dh_…`) — the operator does this or hands you credentials.

**Gate C2:** `curl -s http://127.0.0.1:23000/api/health` returns 200;
`docker ps` shows `dockhand` healthy.

### 1.3 Zoraxy

```bash
mkdir -p /opt/codery/stacks/zoraxy
cp /root/Codery/stacks/core/zoraxy/compose.yml /opt/codery/stacks/zoraxy/
echo "TAILNET_IP=$(cat /run/tailscale.ip)" > /opt/codery/stacks/zoraxy/.env
docker compose -f /opt/codery/stacks/zoraxy/compose.yml \
  --env-file /opt/codery/stacks/zoraxy/.env up -d
```

Then: `:8000` → create admin account; add wildcard cert via Cloudflare DNS-01
(`CLOUDFLARE_API_TOKEN` from the secrets file); add proxy-hosts as services
come up (table in ARCHITECTURE.md).

**Gate C3:** `curl -sk https://<tailnet-ip>/ -H 'Host: <domain>'` answers
(with cert or default page); Zoraxy shows the tailnet upstreams healthy.

### 1.4 codery-net

```bash
docker network create codery-net
```

**Gate C4:** `docker network ls` shows `codery-net`.

## Phase 2 — TOOLS (devtools stack)

```bash
mkdir -p /opt/codery/stacks/devtools
cp /root/Codery/stacks/tools/devtools/compose.yml /opt/codery/stacks/devtools/
cat > /opt/codery/stacks/devtools/.env <<EOF
TAILNET_IP=$(cat /run/tailscale.ip)
PASEO_IMAGE=<paseo image ref — ghcr.io/coderyoss/codery:paseo-… or build via release-apps.yml>
APPS_IMAGE=<apps image ref>
OPENDESIGN_IMAGE=<opendesign image ref>   # only if design profile wanted
COMPOSE_PROFILES=<chosen list, e.g. browser,db>
SSH_DIR=/opt/codery/ssh
DOCKHAND_API_TOKEN=<dh_ token from 1.2>
EOF
docker compose -f /opt/codery/stacks/devtools/compose.yml \
  --env-file /opt/codery/stacks/devtools/.env up -d
```

Prefer deploying through Dockhand once it's up (register the stack from
`/opt/codery/stacks/devtools/compose.yml`) — after this phase, Dockhand owns
the stack lifecycle.

Image note: `ghcr.io/coderyoss/codery:*` is private — `GHCR_USERNAME`/
`GHCR_TOKEN` in `.env` must be valid, or build images with
`.github/workflows/release-apps.yml` (apps) and push to your own registry.

**Gate T1:** `docker ps` shows paseo + apps (+ profiled tools) running and
healthy. **Gate T2:** `ssh -p 2222 paseo@<tailnet-ip>` works; `ssh -p 3333
gem@<tailnet-ip>` works; mosh connects. **Gate T3:** agent session in paseo
loads the `dockhand.*` MCP (`dockhand_list_stacks` returns the stacks).

Add Zoraxy proxy-hosts for the enabled tools (code., opendesign., dbgate. →
container:port from the compose header).

## Phase 3 — PROJECTS

Per project: clone into `/opt/codery/projects/<name>`, register its
root `docker-compose.yml` as a Dockhand stack, deploy, add Zoraxy routes.
Projects keep their own secrets in their own repos. Gate per project: route
answers through Zoraxy with TLS.

## Handoff checklist

- [ ] operator changes Dockhand admin + Zoraxy admin passwords
- [ ] `host/templates/env.example` keys all present in `/opt/codery/.env`
      (0600, root-owned)
- [ ] first `codery-backup.sh` run + restore spot-check
- [ ] `ARCHITECTURE.md` read by the operator (10 minutes, saves hours)
