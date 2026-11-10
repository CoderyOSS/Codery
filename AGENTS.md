# Codery — Infrastructure Reference

This file is loaded automatically by Claude Code and compatible agents (as AGENTS.md). Read it before making
any changes to this repo.

> **REV2 (2026-11-09) — read this first.** The codery-ci binary (blue/green deploys,
> Caddyfile generation, container lifecycle, the `codery.*` MCP) is **disabled, not
> deleted**: no codery process runs; supervisor confs/state/binary are retained for
> revert. Routing is **Zoraxy**, stacks run under **Dockhand**, and this repo now
> holds: stack definitions, the host layer, docs/runbooks, and agent config.
> The pre-REV2 architecture (blue/green, Caddy, apps-nginx hosting) survives only in
> git history and the retained host state — see `docs/runbook/openminis-migration.md`.

---

## What This Repo Is

Codery is the infrastructure for a VPS-hosted developer environment. After the 2026-11
runtime-ops migration it is:

- **Stack definitions** for the Dockhand-managed containers (this repo is the canonical
  mirror; the deployed copies live in Dockhand stack storage / `/opt/codery/stacks`)
- **The host layer**: Tailscale (the perimeter), supervisord, dns-update, Zoraxy's
  supervisor entry, host firewall/sysctl conventions
- **Docs and runbooks**: `docs/runbook/openminis-migration.md` (REV2 phases A–E is the
  authoritative operating manual), `docs/superpowers/specs/` (design records),
  `AGENTS.md` (this file) and the agent config under `containers/paseo/opencode-config/`
- **A disabled, retained revert path**: the codery-ci binary and supervisor confs remain
  on the host; the Rust source was removed from the repo (recoverable from git history,
  pre-REV2 tags/commits)

## REV2 Architecture

| Layer | Runs | Managed by |
|---|---|---|
| Router | **Zoraxy** (container, tailnet-IP `:443/:80`, mgmt UI `:8000`) — wildcard TLS via Cloudflare DNS-01, all vhosts, uptime monitor | Zoraxy UI/API (Rob); agents do not edit routes |
| Stacks | **Dockhand**: `devtools` (paseo, apps, opendesign, playwright), `remi_backend`, `carta` (carta-client, carta-design, carta-cbe1) | Dockhand UI/API + the `dockhand.*` agent MCP |
| Host | Tailscale, supervisord (zoraxy mgmt entries, dns-update), sshd per container | host layer / runbook |
| Codery | **Disabled** (daemon, MCP, Caddy: stopped + autostart=false; binary/state retained) | revert via `docs/runbook/openminis-migration.md` FULL ABORT/re-enable notes |

**Network fabric**: `codery-net` (external docker network). Every stack attaches to it;
Zoraxy proxies to containers by name — HTTP services need **no host port publishes**.

**External publishes** (tailnet-IP or loopback only, never public):
`paseo` `127.0.0.1:6767` + `:2222`; `apps` `:3333`, `:61000/udp`, `127.0.0.1:8080`
(transition-only); `opendesign` `127.0.0.1:7456`; `dockhand` `127.0.0.1:23000`;
zoraxy `${TAILNET_IP}:443/:80/:8000`. Host sysctl `net.ipv4.ip_nonlocal_bind=1`
(`/etc/sysctl.d/99-tailnet-binds.conf`) lets these bind before tailscale0 exists.

## Stack-File Convention

- **Project stacks** live in the project root: `/opt/codery/projects/REMI/docker-compose.yml`
  (committed to CoderyOSS/REMI), `/opt/codery/projects/Carta/docker-compose.yml`
  (committed to CoderyOSS/Carta)
- **Infra stacks** live at `/opt/codery/stacks/<name>/compose.yml` with the canonical
  mirror in this repo: `containers/devtools/compose.yml`, `containers/zoraxy/compose.yml`
- Edit either copy, but never let them diverge: repo = record, stacks dir = runtime
  truth. Adding a service: edit the stack compose → Dockhand Deploy (or API
  `POST /api/stacks/<name>/deploy`) → add the Zoraxy proxy-host (subdomain →
  `container:port`)

## Where Things Run (ownership contract)

Before creating any service, apply this test: **is it project-specific? → that
project's stack. Is it shared infra? → this repo / the devtools stack. Never commit
personal-project config to the Codery repo.**

| What | Where | Managed by |
|---|---|---|
| paseo (agents), apps (dev env), opendesign, playwright | `devtools` stack | Dockhand / runbook |
| REMI backing services + frontend | `remi_backend` stack | REMI project (compose at REMI root) |
| carta-client, carta-design, carta-cbe1 | `carta` stack | Carta project |
| Routing (TLS, vhosts, uptime monitor) | Zoraxy UI/API | Rob (agents: request, don't edit) |
| Dev CLI tools | `tooladd` (see below) | manifest + nix profile |

## Common Operations

| Situation | Action |
|---|---|
| Add/change a stack service | Edit stack compose (repo + Dockhand) → Dockhand **Deploy** |
| Add a subdomain | Zoraxy UI → proxy host (subdomain → `container:port` on codery-net) |
| Container down / logs / restart | Dockhand UI or `dockhand.*` agent MCP |
| Install a dev tool | `ssh gem@apps tooladd <nixpkgs-pkg>` (persists in the `/nix` volume; manifest auto-committed) |
| Host unhealthy | `/usr/local/bin/codery-triage` (or `--json`) |
| Full backup | `/opt/codery/bin/codery-backup.sh` |
| Router broken (lockout) | tailnet SSH is independent: `ssh -L 23000:127.0.0.1:23000` → Dockhand UI on localhost |
| Re-enable Codery (revert) | runbook FULL ABORT / re-enable notes — requires resurrecting Rust from git history |

## SSH Access

| Path | Command | Notes |
|---|---|---|
| Operator → paseo | `ssh -p 2222 paseo@<tailnet-ip>` | direct docker publish; keys in `/opt/codery/ssh/authorized_keys` |
| Operator → apps | `ssh -p 3333 gem@<tailnet-ip>` | direct docker publish; mosh via `:61000/udp` |
| paseo → apps | `ssh gem@apps` | codery-net alias; agent-to-apps key |
| Zoraxy mgmt UI | `http://<tailnet-ip>:8000` | browser only |

SSH **host keys are persistent** (named volumes per container) — fingerprints are
stable across recreates. The `:2222`/`:3333`/`:61000` publishes are native Docker
port mappings; there is no tunnel software.

## Disabled Codery (revert path)

- Supervisor programs `codery-ci`, `codery-ci-mcp`, `caddy`: stopped, `autostart=false`,
  conf files retained in `proxy/supervisor/` (host copies in supervisord)
- State retained: `/opt/codery/state/`, `/opt/codery/codery.db`, local image cache,
  `/opt/codery/bin/` scripts
- Rust source: removed from this repo at REV2 (recoverable from git history —
  see commits pre-`migration/runtime-ops` merge, or the branch reflog)
- Re-enable sequence + FULL ABORT: `docs/runbook/openminis-migration.md`

## Workflows (.github/workflows)

Retained: `release-apps.yml` (builds/pushes the apps image — the CI build path for
the devtools stack), `release-orchestrator.yml` + `setup-host.yml` + `diagnose.yml`
(revert/host-repair paths). Removed at REV2: the codery-ci deploy/sync/test workflows.

## Host Environment

| Path | Purpose |
|------|---------|
| `/opt/codery/` | Root of all Codery state (`.env`, `state/`, `ssh/`, `codery.db`, `bin/`, `data/`) |
| `/opt/codery/projects/` | Shared projects tree (bind-mounted into paseo `/workspace`, apps `/home/gem/projects`) |
| `/opt/codery/stacks/` | Infra stack composes (devtools, zoraxy) — Dockhand-deployed |
| `/etc/caddy/` | retired with Caddy (kept on disk for revert) |
| `/run/tailscale.ip` | host tailnet IP (used by stack `.env` TAILNET_IP) |

## Host Memory

16 GiB RAM, 4 GiB swap (swap use is normal — the dashboard health rules that
flagged it are retired with the dashboard). Watch `free -m` and the OOM counter
in triage before big builds.

## Sensitive Files — Do Not Read or Expose

These files contain secrets. NEVER read, cat, print, or include their contents in output:

- `.env`, `*.env.*` — API keys, tokens, credentials
- `/run/secrets/*` — GitHub App PEM key (bind-mounted)
- `~/.ssh/*`, `/opt/codery/ssh/agent-to-apps` — private keys
- `~/.local/share/opencode/auth.json` — provider API keys
- `*.pem`, `*.key` — any private key files

Check existence only (`test -f`), never read contents.
