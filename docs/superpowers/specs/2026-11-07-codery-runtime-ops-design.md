# Codery Runtime-Ops Redesign — Design Spec

**Date:** 2026-11-07
**Status:** Approved by Rob (this document is the authority for the migration branch `migration/runtime-ops`)
**Implements the decision record from the 2026-11-07 brainstorming session.**

---

## 1. Intent and why

Codery's deployment layer (codery-ci blue/green) is heavy for what a single-user
dev lab needs, two pain points have persisted (project compose stacks are
unmanaged — agents improvise; dev tools require slow image rebuilds), and the
operator surface is scattered across host shells. This redesign:

- Moves **all container lifecycle** to **Dockhand** (runtime operator + ops UI).
- Reimplements zero-downtime switching at the **routing layer** (Caddy upstream
  flip via `routes.yaml` → `reload-routes`), retiring blue/green wholesale.
- Makes dev tools **installable at runtime** and persistent across redeploys
  (named `/nix` volume + `tooladd` + committed manifest).
- Shrinks codery-ci to: **routes + health + dashboard + agent MCP + DNS +
  supervisord**.
- Ends with a **runbook/skills** so OpenMinis (Rob's on-device ops harness) can
  set up a fresh server and troubleshoot this one.

**Non-goals:** multi-host central management (each Codery box is standalone;
the repo + runbook is the portability), Kubernetes/orchestration, adopting
Dockhand stack-secrets (project `.env` files stay gitignored and local),
replacing GitHub for code hosting, changing the paseo sandbox model.

**Policy (pinned by Rob):** switching/upgrades happen ONLY when agent sessions
are inactive. Session continuity is never engineered for; it is a scheduling
rule.

## 2. Target architecture

```
iPad ── OpenMinis (ops harness) ── SSH over tailnet ──┐        Paseo app ── WS ── paseo container (OpenCode agents)
                                                      │
HOST:  Tailscale (the perimeter) · Caddy (TLS + routing; upstream flips via routes.yaml)
       supervisord (caddy, tailscale, dns-update, codery-ci-slim)
       codery-ci-slim = routes + health (triage/diagnose/doctor) + MCP + dashboard + DNS
       Dockhand container = ALL container lifecycle + compose stacks + ops UI
AGENTS: paseo (sandboxed, workspace + thin stack MCP + ssh gem@apps) ·
        OpenMinis (full host via SSH; never codes inside codery)
STACKS: one Dockhand stack per project (REMI first) + one per core service;
        all Internal (filesystem) stacks; Git-sync opt-in only, never required
```

### Two agent tiers (distinct, non-overlapping surfaces)

| | Paseo (in-server dev agent) | OpenMinis (on-device ops harness) |
|---|---|---|
| Role | build/plan/code (OpenCode, modes) | system admin: setup, deploys, troubleshooting |
| Surface | workspace files; thin stack MCP; `ssh gem@apps` (tooladd) | SSH to host; `codery-triage`/`doctor`; Dockhand UI/API; runbook skills |
| Trust | no host access (sandbox preserved) | full host, gated by tailnet SSH keys |

### Switching (replaces blue/green)

1. Run the new version **alongside** (second container/stack on another host
   port) — running-alongside IS the preview.
2. Verify (health endpoint, preview URL).
3. Flip the upstream: edit one line in `proxy/routes.yaml` → `codery-ci
   reload-routes` → Caddy regenerates + graceful reload.
4. Drop the old container via Dockhand when idle. Policy gate: sessions
   inactive.
5. Rollback = re-point `routes.yaml` at the previous upstream / previous local
   image tag; images persist in the local docker cache.

Stable ports become **direct tailnet publishes** (no colors to follow):
`paseo` ssh 2023→2222, daemon 6767→loopback; `apps` ssh 2022→3333, mosh
61000/udp. The codery-ci TCP proxies and mosh UDP relay retire.

## 3. Layer ownership contract (the anti-freestyle rule)

Lives in both AGENTS.md files every agent session loads. Decision test:
**defined under `containers/` in the Codery repo? No → it is a project →
never commit to Codery.**

| What | Where it is declared | Managed by | Route layer |
|---|---|---|---|
| Codery infra (paseo, apps, opendesign, playwright, dockhand, host) | `containers/<name>/` in Codery repo | codery-ci-slim / runbook | Codery repo routes (infra only) |
| Project backing services | `<project>/docker-compose.yml` | Dockhand stack + stack MCP | project-owned (runtime-app registration or stack port) |
| Project web UIs (tailnet subdomain) | runtime app in codery.db via `add_app` MCP (qdrant/graph/bolt-proxy pattern) | add_app/restart_app | SQLite, never committed to Codery repo |
| Dev tools | `containers/apps/tools-manifest.txt` + live `tools` nix profile | tooladd | n/a |

Guardrail: opencode permission config denies agent writes to
`proxy/routes.yaml` and `containers/*/service.yml` by default (in-session
approval to override).

## 4. Dockhand security fence

- Bind loopback on host; Caddy `dockhand.<domain>` binds **tailnet IP only**;
  Caddy basic-auth gate; Dockhand auth ON with Rob's admin user (setup gate —
  its first-launch default is auth-off).
- Verified-unreachable from public internet (runbook gate probe).
- Thin stack MCP exposes only `stack_list/ps/sync/logs/restart/health` — no
  terminal/exec/file-browser tools. Paseo still has NO docker socket.
- Dockhand API tokens are full-scope in the free edition — containment comes
  from the narrow MCP surface, not the token. Disclosed and accepted.
- Image tag pinned; bumps are deliberate runbook steps.
- Optional phase-5 hardening: tecnativa socket-proxy in front of the socket.
- Dockhand activity log = audit trail for agent actions.

## 5. Credential map (complete)

| Credential | Used by | Location / notes |
|---|---|---|
| Tailnet SSH keys | OpenMinis→host, paseo→apps | `/opt/codery/ssh/authorized_keys` (host-managed, unchanged) |
| GitHub App PEM | agents pushing code ONLY | `/opt/codery/github-app.pem` — unchanged; nothing new depends on it |
| Dockhand API token | thin stack MCP + OpenMinis curl | minted at setup → `/opt/codery/.env` (`DOCKHAND_API_TOKEN`) → paseo env_overrides; OpenMinis copy in its secrets file |
| Dockhand basic-auth | Caddy gate | `/opt/codery/.env` (`DOCKHAND_BASIC_AUTH`…), house pattern |
| Dockhand UI login | Rob only | created at setup; stored in Rob's password manager |
| OpenMinis local secrets | OpenMinis sandbox | `~/.codery/secrets.env` in the Minis persistent sandbox, populated by Rob from LastPass (format below); **never** in any git repo |
| (optional, only if push-to-deploy wanted later) read-only fine-grained PAT or SSH deploy key | one stack's webhook auto-sync | minted later; AES-256-GCM-encrypted inside Dockhand |

**OpenMinis secrets.env format (documented in the runbook):**

```
CODEMY_HOST=root@<tailnet-ip-or-magicdns>
DOCKHAND_URL=https://dockhand.<domain>
DOCKHAND_BASIC_AUTH=<user>:<password>
DOCKHAND_API_TOKEN=<token>
GITHUB_TOKEN=<fine-grained token for pushes if used>
SSH_KEY_PATH=~/.ssh/id_ed25519
```

No git credentials are required for stack deploys (filesystem-first). Git
stays versioning-only; new-server reproduction = projects-tree restore +
one-time bootstrap clone.

## 6. Tools persistence design (problem 2)

- Named Docker volume on `/nix` of the apps container: auto-seeded from the
  image on first boot; runtime installs persist across redeploys and image
  rebuilds.
- Deploy-time store sync (`/opt/codery/bin/nix-store-sync.sh`): one-shot
  container from the new image rsyncs new store paths (content-addressed,
  `--ignore-existing`) into the volume and repoints the default profile. Run
  before every apps deploy that changes the image (runbook step).
- `tooladd <pkg>`: `sudo nix profile install --profile
  /nix/var/nix/profiles/tools nixpkgs#<pkg>` + regenerate
  `containers/apps/tools-manifest.txt` + commit via github-push. PATH via
  `/etc/profile.d/tools.sh` → `/nix/var/nix/profiles/tools/bin`.
- Boot oneshot `tools-replay`: idempotent install from the manifest (fast —
  cache.nixos.org substitutes; no-op when the store already has paths).
- apps Dockerfile build-time caching: NOTE — a BuildKit cache mount on
  `/nix` was rejected: cache-mount contents do not persist into image
  layers, so the baked store would be empty. Build-time flake rebuilds stay
  as-is; the runtime win (no rebuilds needed for tools) is the actual fix.
  If build speed matters later, the path is a codery-ci build change
  (local registry cache), out of scope here.
- Agents install tools via `ssh gem@apps tooladd <pkg>`; Rob uses the same
  path interactively.

## 7. Routing + health + dashboard (codery-ci-slim scope)

- Routes view merges ALL layers: repo routes (routes.yaml/service.yml),
  SQLite runtime apps, stack-published ports. Each row: subdomain, upstream,
  source layer, health (periodic HTTP probe, TCP fallback; red = upstream
  down; last-seen-OK), plus mismatch flags both directions (route without
  upstream, listener without route).
- Health inherits `codery-triage`/`diagnose`; playbook P1/P2
  (`boot-verify`, `doctor --fix`) land in this workstream.
- Dashboard = the existing React `system/orchestrator/ui/`, Routes + Health.

## 8. Phasing (execution)

- **W1 (repo scaffold)**: spec, AGENTS.md contracts, permission guardrail,
  thin stack MCP, Dockhand service.yml + security checklist, tooladd +
  manifest + replay oneshot + nix-store-sync, staged compose.yml files, in-band
  backup script, OpenMinis runbook. Dashboard MVP last (slippable).
- **W2 (machine staging, additive only)**: deploy Dockhand; run security
  gates; adopt REMI as Internal stack; wire MCP token into paseo env (one
  idle-time recreate); verify agent ps/logs/sync via MCP. Backout: remove
  Dockhand stack.
- **W3 (tools persistence)**: apps image rebuild from branch (nix volume +
  tooladd + cache mount); `tooladd cloc` live test; persistence across
  recreate. Backout: previous apps image tag.
- **W4 (handover prep)**: OpenMinis runbook finalized; in-band backup
  executed; G0 confirmed (netcup snapshot — Rob restored control-panel
  access).
- **W5 (OpenMinis executes; only after G0)**: paseo off blue/green →
  opendesign/playwright → apps off blue/green → codery-ci slim + dashboard +
  doctor/boot-verify → OpenMinis skills finalized.

## 9. Operational safety rules (pinned by Rob, 2026-11-07)

1. **Self-preservation**: the paseo container hosting a working agent is
   NEVER recreated/redeployed by that agent. Paseo env/config wiring that
   requires a recreate (e.g. adding `DOCKHAND_API_TOKEN` env_override) is
   executed by OpenMinis or Rob at migration time, when sessions are idle.
   The stack MCP must degrade gracefully (clear remediation message) until
   the token is present.
2. Switching/upgrades only when agent sessions are inactive (§1 policy).
3. apps container operations (rebuild/recreate) require a quick session
   check (`ssh gem@apps who; tmux ls`) — mosh/ssh sessions die on recreate.

## 10. Backout

Per-phase fallbacks (see plan + runbook): previous image tags retained;
routes.yaml/Caddyfile/service.yml backed up before first touch; paseo/apps
service.yml preserved until phase 4; full abort = restore `/opt/codery` from
the in-band backup → `codery-ci deploy <svc> <last-good-sha>` per service →
`main` checkout (scripted, ≤ 1 hr). Netcup hypervisor snapshot is the outer
safety net (Rob takes it before handoff).

## 10. Decisions of record

Dockhand over Komodo/Portainer/Dockge/PaaS (in-place adoption, token API,
restic backups, UI; BSL free for this use). Filesystem-first stacks (no
standing git credentials for deploys). Local builds over releases (docker
build on host, local tags; no registry). Blue/green retired wholesale;
switching at the routing layer. Guardrail: deny-by-default agent writes to
Codery infra files. MCP reachable by paseo (in-container `dockhand:3000`)
and OpenMinis (`https://dockhand.<domain>` over tailnet, basic auth +
bearer).
