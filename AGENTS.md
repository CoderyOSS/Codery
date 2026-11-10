# Codery — Infrastructure Reference

This file is loaded automatically by Claude Code and compatible agents (as
AGENTS.md). Read it before making any changes to this repo.

> **REV3 (2026-11-10) — three-layer layout.** Codery is an OS for developers:
> **core** (Dockhand, Zoraxy, Tailscale, host layer) · **tools** (paseo, apps,
> playwright, opendesign, dbgate — opt-in via compose profiles) · **projects**
> (your applications — one stack per project repo). The codery-ci binary is disabled
> on the host (revert path retained); routing is Zoraxy, stacks run under
> Dockhand. Fresh installs: `RUNBOOK.md`. Instance-specific history and
> design records live in your private deployment overlay, not in this repo.

## What This Repo Is

- **Stack definitions** — `stacks/core/` (dockhand, zoraxy) and
  `stacks/tools/devtools/`; canonical mirrors of the deployed copies in
  `/opt/codery/stacks/`. Never let them diverge: repo = record, stacks dir =
  runtime truth.
- **Image builds** — `images/{apps,paseo,opendesign}`; core runs stock
  upstream images, tools are Codery-built. Provenance identifies the layer.
- **Host layer** — `host/`: supervisor confs, tailscale/dns/backup scripts,
  `bootstrap.sh` (fresh-VPS prep), `templates/env.example` (secrets contract).
- **Docs** — `RUNBOOK.md` (agent bootstrap), `SETUP.md`, `ARCHITECTURE.md`,
  `docs/` (guides, incidents, historical runbooks, specs).
- **Agent config** — `images/paseo/opencode-config/` is the LIVE config for
  the paseo container (bind-mounted).

## The Three Layers (do not cross them)

| Layer | Members | Defined in | State lives in |
|---|---|---|---|
| Core | dockhand, zoraxy, tailscale, host layer | `stacks/core/`, `host/` | named volumes + host config |
| Tools | paseo, apps, playwright, opendesign, dbgate | `stacks/tools/devtools/`, `images/` | `/opt/codery/data/<tool>` + volumes |
| Projects | your applications | project repos (compose at repo root) | project volumes |

Rules:

1. Management/routing/perimeter → core. Developer-facing → tools.
   App-specific → project stack. No exceptions.
2. Secrets: core + tools share `/opt/codery/.env` (0600). Project secrets
   never leave the project repo.
3. Access flows downward: core manages everything; tools see projects;
   projects see only themselves.
4. Tools are opt-in: `COMPOSE_PROFILES` in the stack `.env` (`browser`,
   `design`, `db`). Base (paseo + apps) is always on; consumers degrade
   gracefully.

## Runtime Map

| Layer | Runs | Managed by |
|---|---|---|
| Router | **Zoraxy** (tailnet-IP `:443/:80`, mgmt `:8000`) — wildcard TLS via Cloudflare DNS-01, vhosts, uptime monitor | Zoraxy UI/API (Rob); agents request, never edit |
| Stacks | **Dockhand**: `devtools` + one stack per project — dockhand itself is host-side compose, never self-registered | Dockhand UI/API + `dockhand.*` agent MCP |
| Host | Tailscale, supervisord (dns-update, tailscale), sysctl tailnet binds | `host/` + RUNBOOK |

**Network fabric**: `codery-net` (external). Every stack attaches; Zoraxy
proxies by container name — HTTP services need no host port publishes.

**Host publishes** (tailnet-IP or loopback only, never public): paseo
`127.0.0.1:6767` + `:2222`; apps `:3333`, `:61000/udp`, `127.0.0.1:8080`
(transition-only); opendesign `127.0.0.1:7456`; dockhand `127.0.0.1:23000`;
zoraxy `${TAILNET_IP}:443/:80/:8000`. Sysctl `net.ipv4.ip_nonlocal_bind=1`
lets these bind before tailscale0 exists.

## Common Operations

| Situation | Action |
|---|---|
| Add/change a stack service | Edit stack compose (repo + `/opt/codery/stacks/`) → Dockhand **Deploy** |
| Add a subdomain | Zoraxy UI → proxy host (subdomain → `container:port`) |
| Container down / logs / restart | Dockhand UI or `dockhand.*` MCP |
| Enable/disable a tool | Edit `COMPOSE_PROFILES` in the stack `.env` → Deploy |
| Install a dev tool | `ssh gem@apps tooladd <nixpkgs-pkg>` (persists in `/nix`; manifest auto-committed) |
| Deploy dockhand itself | host-side only: `docker compose -f /opt/codery/stacks/dockhand/compose.yml up -d` |
| Host unhealthy | `/usr/local/bin/codery-triage` (`--json` for machines) |
| Full backup | `/usr/local/bin/codery-backup.sh` → `/opt/codery/backups/` |
| Router lockout | tailnet SSH is independent: `ssh -L 23000:127.0.0.1:23000` → Dockhand UI on localhost |
| Re-enable codery-ci (revert) | retained on the host: disabled supervisor programs + `/opt/codery/state/` (see your instance docs) |

## SSH Access

| Path | Command | Notes |
|---|---|---|
| Operator → paseo | `ssh -p 2222 paseo@<tailnet-ip>` | keys in `/opt/codery/ssh/authorized_keys` |
| Operator → apps | `ssh -p 3333 gem@<tailnet-ip>` | mosh via `:61000/udp` |
| paseo → apps | `ssh gem@apps` | codery-net alias; agent-to-apps key |
| Zoraxy mgmt UI | `http://<tailnet-ip>:8000` | browser only |

SSH host keys are persistent (named volumes) — fingerprints survive recreates.

## Host Environment

| Path | Purpose |
|------|---------|
| `/opt/codery/.env` | THE secrets file (0600 root; sudo-only from apps) |
| `/opt/codery/stacks/` | infra stack dirs — Dockhand-deployed, gem-writable |
| `/opt/codery/projects/` | shared projects tree (paseo `/workspace`, apps `/home/gem/projects`) |
| `/opt/codery/data/` | tool service data (opencode, opendesign, dbgate) |
| `/opt/codery/{ssh,state,backups}/` | keys · disabled codery retainers · backups |
| `/run/tailscale.ip` | host tailnet IP (stack `.env` TAILNET_IP) |

Host memory: 16 GiB RAM, 4 GiB swap (swap use is normal). Watch `free -m` and
the OOM counter before big builds.

## Disabled Codery (revert path)

Supervisor programs `codery-ci`, `codery-ci-mcp`, `caddy`: stopped +
`autostart=false`; state retained on the host (`/opt/codery/state/`,
`codery.db`, binary, images). Rust source is recoverable from git history
(pre-`migration/runtime-ops`). Re-enable steps live in your instance docs
(the retained host state documents the exact programs and paths).

## Workflows (.github/workflows)

`release-apps.yml` — builds/pushes the apps image (CI path for the devtools
stack). `release-orchestrator.yml` + `diagnose.yml` — revert/host-repair
paths.

## Sensitive Files — Do Not Read or Expose

NEVER read, cat, print, or include their contents in output:

- `.env`, `*.env.*` — API keys, tokens, credentials
- `/run/secrets/*` — GitHub App PEM key (bind-mounted)
- `~/.ssh/*`, `/opt/codery/ssh/agent-to-apps` — private keys
- `~/.local/share/opencode/auth.json` — provider API keys
- `*.pem`, `*.key` — any private key files

Check existence only (`test -f`), never read contents.
