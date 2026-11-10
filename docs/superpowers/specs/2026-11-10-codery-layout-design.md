# Codery Layout & Bootstrap Design — 2026-11-10

**Status:** approved (Rob) · **Scope:** repo reorganization + fresh-VPS bootstrap path
**Runtime phases (P2+) execute after GitHub review of P1.**

## Intent

Make the Codery repo read like a well-planned open source project: three clean
layers, composable stacks, opt-in developer tools, and an agent-executable
bootstrap so a fresh VPS reaches full operation with no tribal knowledge.

## The three layers (load-bearing taxonomy)

| Layer | Members | Repo | Host | Data |
|---|---|---|---|---|
| **Core** — the kernel | Dockhand, Zoraxy, Tailscale, host layer | `stacks/core/{dockhand,zoraxy}`, `host/` | `/opt/codery/stacks/{dockhand,zoraxy}` | `codery-dockhand-data`, `zoraxy_state` vols |
| **Tools** — the userland | paseo, apps, playwright, opendesign, dbgate | `stacks/tools/devtools`, `images/` | `/opt/codery/stacks/devtools` | `/opt/codery/data/{opencode,opendesign,dbgate}` + vols |
| **Projects** — user files | REMI, Carta, future projects | project repos | `/opt/codery/projects/` | their own compose volumes |

Rules:

1. **Image provenance:** core = stock upstream images, no custom builds; tools =
   Codery-built images (`images/`). Provenance identifies the layer.
2. **Secrets:** core + tools share `/opt/codery/.env` (0600); project secrets
   never leave the project repo.
3. **Access flows downward:** core manages everything; tools see projects;
   projects see only themselves.
4. **Data:** tool service data lives under `/opt/codery/data/<tool>`; project
   data lives in project-scoped volumes. Never crossed.
5. **New-component test:** management/routing/perimeter → core; developer-facing
   → tools; app-specific → project stack. No exceptions.

## Decisions (Rob-approved)

- **Dockhand:** compose-native (`stacks/core/dockhand/compose.yml`), deployed
  **host-side** (`docker compose up`), never self-registered in Dockhand —
  recreating the management plane from within itself is a lockout foot-gun.
  Container named plain `dockhand` (blue/green suffix dropped); state volume
  `codery-dockhand-data` kept (holds API tokens).
- **Host perms:** `/opt/codery` mounted **rw** into apps; permission model gates
  writes — `stacks/` chowned gem (uid 1000, direct nvim editing), everything
  else root-owned, secrets 0600 (sudo-only). `sudo` = root access, by design.
- **Agent visibility (paseo):** `/opt/codery/stacks` + `/opt/codery/data`, ro.
  No `.env` exposure.
- **Opt-in tools:** compose **profiles** + inclusion list
  (`COMPOSE_PROFILES=browser,design,db` in the stack `.env`). Base always-on:
  paseo + apps. Consumers degrade gracefully (paseo: lazy endpoint, browser
  tool fails fast; opendesign: preview features error when used).
- **REMI helpers migration** (layer violation: project services in the apps
  container): dedicated phase — port qdrant/graph/bolt proxies + ideas +
  booksearch into the `remi_backend` stack, re-point Zoraxy, retire
  ports 8123-8128.
- **Bootstrap:** `RUNBOOK.md` (repo root) = agent-facing fresh-VPS runbook:
  prerequisites contract → secrets contract (`host/templates/env.example`) →
  host bootstrap (`host/bootstrap.sh`) → stacks by layer with gates → project
  import. Replicates the full host layer (tailscale, supervisord, sysctl,
  dns-update), not a docker-only minimum.

## Phase plan

| Phase | Scope | Risk |
|---|---|---|
| P1 — repo reorg | this document's repo changes; 3 commits to main; **pause for GitHub review** | none (repo-only) |
| P2 — dockhand compose-native + devtools deploy #1 | host-side recreate (rename + `.env` mount + perms); opendesign auth fix + dbgate live | brief dockhand blip |
| P3 — visibility-mount deploy | devtools deploy #2; paseo+apps recreate **once**, gated on idle sessions, triggered by Rob | paseo down mid-deploy |
| P4 — REMI helpers migration | new containers in `remi_backend`; Zoraxy re-point; retire 8123-8128 | route blip |
| P5 — leftovers | `system/` purge, OpenSandbox noted as future | none |

## Repo layout after P1

```
Codery/
├── README.md  SETUP.md  RUNBOOK.md  ARCHITECTURE.md  AGENTS.md  LICENSE
├── stacks/
│   ├── core/{dockhand,zoraxy}/compose.yml
│   └── tools/devtools/compose.yml
├── images/{apps,paseo,opendesign}/
├── host/            # supervisor confs, tailscale/dns/backup/nix-sync scripts, bootstrap.sh
│   └── templates/env.example
└── docs/            # guides/, incidents/, runbook/ (historical), superpowers/specs/
```

Purged: retired `service.yml`s, caddy confs/scripts/routes, `hosting/`,
`setup-host.yml`, root `opencode.json`, stale `customizing.md`.
