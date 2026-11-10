# RUNBOOK — Operating a Codery VPS

**Start here.** This is the operating guide for an AI agent (OpenMINIS-class)
running a Codery VPS: fresh install and bootstrap (Part I), day-to-day
operations (Part II), debugging and repair (Part III), maintenance (Part IV).

Read [AGENTS.md](AGENTS.md) once before acting — it defines the operating
contract (layer rules, what never to touch, secrets handling, container
hygiene). This runbook is procedures; that file is rules.
Humans: [SETUP.md](SETUP.md). Architecture background: [ARCHITECTURE.md](ARCHITECTURE.md).

**Non-negotiables, up front:**

- Never recreate the paseo container from within paseo (it hosts the working
  agent). Container switches only when agent sessions are idle.
- Never deploy dockhand via Dockhand itself — host-side compose only.
- The `codery-dockhand-data` volume is never renamed (it holds every `dh_` API
  token).
- Secrets live in `/opt/codery/.env` (0600). Never read or print it — check
  existence with `test -f`, never `cat`.
- Route changes happen in the Zoraxy UI/API — request them from the operator,
  never invent routing.

---

# Part I — Fresh install (bootstrap)

**Audience for this part:** an agent with root SSH on a fresh Ubuntu/Debian
24.x VPS, this repo cloned, and operator-staged secrets.

## Phase 0 — Contracts

**You receive:**

- root SSH to a fresh VPS (no other software assumed)
- this repo, cloned to the VPS at `/root/Codery`
- secrets staged at `/root/secrets.env` in `KEY=value` form — the key set is
  exactly [`host/templates/env.example`](host/templates/env.example). If a key
  is missing, STOP and ask the operator; do not invent values.

**You deliver:**

- a running system: Tailscale perimeter, Dockhand management plane, Zoraxy
  routing edge (wildcard TLS), the devtools stack, operator access verified
- the operator's first task after handoff: change passwords, review Zoraxy
  routes

**Operator decisions to collect before starting** (ask once):

- domain name (default: `example.com`)
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

### 1.3 Zoraxy + wildcard TLS

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
PASEO_IMAGE=<paseo image ref>
APPS_IMAGE=<apps image ref>
OPENDESIGN_IMAGE=<opendesign image ref>   # only if design profile wanted
COMPOSE_PROFILES=<chosen list, e.g. browser,db>
SSH_DIR=/opt/codery/ssh
DOCKHAND_API_TOKEN=<dh_ token from 1.2>
DOMAIN_NAME=<operator domain>
EOF
docker compose -f /opt/codery/stacks/devtools/compose.yml \
  --env-file /opt/codery/stacks/devtools/.env up -d
```

Prefer deploying through Dockhand once it's up (register the stack from
`/opt/codery/stacks/devtools/compose.yml`) — after this phase, Dockhand owns
the stack lifecycle.

Image note: `ghcr.io/coderyoss/codery:*` is private — `GHCR_USERNAME`/
`GHCR_TOKEN` in `.env` must be valid, or build images with
`.github/workflows/release-apps.yml` and push to your own registry.

**Gate T1:** `docker ps` shows paseo + apps (+ profiled tools) running and
healthy. **Gate T2:** `ssh -p 2222 paseo@<tailnet-ip>` works; `ssh -p 3333
gem@<tailnet-ip>` works; mosh connects. **Gate T3:** an agent session in paseo
loads the `dockhand.*` MCP (`dockhand_list_stacks` returns the stacks).

Add Zoraxy proxy-hosts for the enabled tools (code., opendesign., dbgate. →
container:port from the compose header).

## Phase 3 — PROJECTS

Per project: clone into `/opt/codery/projects/<name>`, register its root
`docker-compose.yml` as a Dockhand stack, deploy, add Zoraxy routes. Projects
keep their own secrets in their own repos. Gate per project: route answers
through Zoraxy with TLS.

## Handoff checklist

- [ ] operator changes Dockhand admin + Zoraxy admin passwords
- [ ] `host/templates/env.example` keys all present in `/opt/codery/.env`
      (0600, root-owned)
- [ ] first `codery-backup.sh` run + restore spot-check
- [ ] `ARCHITECTURE.md` read by the operator (10 minutes, saves hours)

---

# Part II — Day-to-day operations

| Task | Procedure |
|---|---|
| Change a stack service | Edit the stack compose — repo copy AND `/opt/codery/stacks/<name>/compose.yml` (never let them diverge) → Dockhand **Deploy** (or `POST /api/stacks/<name>/deploy`) |
| Enable/disable a tool | Edit `COMPOSE_PROFILES` in the stack `.env` → Deploy. Removing a tool leaves its container as an orphan — remove it once in the Dockhand UI |
| Add a subdomain | Zoraxy UI → proxy host (subdomain → `container:port` on codery-net) |
| Install a dev tool | `ssh gem@apps tooladd <nixpkgs-pkg>` — persists in the `/nix` volume; manifest auto-committed to the repo |
| Deploy dockhand itself | Host-side only: `docker compose -f /opt/codery/stacks/dockhand/compose.yml --env-file /opt/codery/stacks/dockhand/.env up -d` |
| Full backup | `sudo /usr/local/bin/codery-backup.sh` → `/opt/codery/backups/` |
| Update Codery itself | `git pull` the repo (or merge upstream `main` into your private overlay), pull/rebuild images (`release-apps.yml`), then per-stack Deploy |

**Stack-change rule:** every deploy is config-hash based — services whose
config didn't change are not recreated. If a paseo recreate is possible, do it
only with agent sessions idle.

---

# Part III — Debugging & repair

| Symptom | First moves |
|---|---|
| Container down / crash loop | Dockhand UI or `dockhand.*` MCP: `stack_ps` → `stack_logs` (tail 100) |
| Dockhand deploy fails with `env file … not found` | The stack compose references an `env_file` path the dockhand container can't see (only `/opt/codery/stacks` and `/opt/codery/projects` are mounted). Mount the missing file into dockhand or inline the values |
| Site 502 via Zoraxy | Check the upstream container is running (`stack_ps`), then the Zoraxy proxy-host target name matches the container name/alias |
| Zoraxy state lost (vhosts/certs gone) | Run `/opt/codery/stacks/zoraxy/bootstrap-state.sh` — restores admin, ACME, wildcard cert, and the vhost list |
| Router lockout | Tailnet SSH is independent: `ssh -L 23000:127.0.0.1:23000 <host>` → Dockhand UI on localhost |
| Host unhealthy | `sudo /usr/local/bin/codery-triage` (`--json` for machines); check `free -m` and OOM counter before big builds |
| Agent can't spawn / MCPs fail | paseo: never run `opencode` as root (root-owned state breaks every later session — see AGENTS.md hygiene rules). Check `~/.local/state/opencode/` ownership |
| SSH to a container fails | Publishes are native docker ports; check the stack is up, then host keys (`codery-paseo-ssh-host-keys` / `codery-apps-ssh-host-keys` volumes survive recreates) |
| Disk pressure | `docker system df`; prune dangling images only after checking Dockhand's pinned digests; `/opt/codery/backups/` is the usual grower |

**Repair rule:** one change at a time, verify, then next. If three fixes fail,
stop and question the diagnosis, not the luck.

---

# Part IV — Maintenance

| Cadence | Task |
|---|---|
| Weekly | `codery-backup.sh` (or cron); spot-check a restore monthly |
| Monthly | `docker system df` + disk check; review Zoraxy uptime monitor for flapping targets |
| Quarterly | Rotate `dh_` tokens if operators changed; re-read ARCHITECTURE.md invariants against the live system |
| On cert warnings | Wildcard auto-renews via DNS-01; if it lapses, `bootstrap-state.sh` re-obtains it |
| On Codery updates | Pull the repo, review the diff (composes + docs), then update stacks one at a time with Deploy; paseo last |

**Break-glass (full revert):** the pre-Dockhand codery-ci machinery is
disabled, not deleted — supervisor programs stopped + `autostart=false`, state
in `/opt/codery/state/`, binary/images retained. Re-enable steps live in the
instance docs of your private deployment overlay. Use only if the Dockhand/Zoraxy
end-state is unrecoverable — it is a rollback, not a repair.
