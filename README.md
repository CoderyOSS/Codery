# Codery

[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

> **AI agents (OpenMINIS-class):** start with [RUNBOOK.md](RUNBOOK.md) — it is
> the operating guide for a Codery VPS: install, setup, debugging, maintenance.

**Codery is a coding platform for your private VPS.** A Tailscale-
perimetered homelab: Docker provides the runtime, Dockhand the management
plane, Zoraxy the routing edge — and on top of them, a developer toolchain
(agent daemon, terminal environment, design workspace, database client) that
hosts your actual code projects side by side.

## The three layers

```
┌──────────────────────────────────────────────────────────────────┐
│  PROJECTS — your code                                            │
│  each project is a compose stack living in its own repo          │
├──────────────────────────────────────────────────────────────────┤
│  TOOLS — the developer userland (opt-in)                         │
│  paseo (agent daemon) · apps (terminal: nvchad, tmux, mosh)      │
│  playwright (browser) · opendesign (design) · dbgate (databases) │
├──────────────────────────────────────────────────────────────────┤
│  CORE — the kernel                                               │
│  dockhand (stacks) · zoraxy (routing/TLS) · tailscale (perimeter)│
└──────────────────────────────────────────────────────────────────┘
```

Rules that keep the layers honest:

- **Core** runs stock upstream images; **tools** are Codery-built (`images/`).
- **Secrets** live in one file on the host (`/opt/codery/.env`); project
  secrets never leave the project repo.
- **Access flows downward**: core manages everything, tools see projects,
  projects see only themselves.
- **Tools are opt-in** — the `COMPOSE_PROFILES` list in a stack's `.env`
  decides what deploys; consumers degrade gracefully without them.

## Repo layout

| Path | Contents |
|---|---|
| `stacks/core/` | Dockhand + Zoraxy compose files (deployed host-side / via Dockhand) |
| `stacks/tools/` | the devtools stack (always-on + profile-gated services) |
| `images/` | Dockerfiles + config for Codery-built tool images |
| `host/` | host layer: supervisor confs, Tailscale/DNS/backup scripts, `bootstrap.sh` |
| `docs/` | guides, incident records, design specs, historical runbooks |
| `RUNBOOK.md` | **agent-facing bootstrap**: fresh VPS → full system, gated steps |
| `SETUP.md` | human-oriented install guide |
| `ARCHITECTURE.md` | end-state reference: what runs where, traffic, invariants |
| `AGENTS.md` | operating contract for AI agents working on this system |

## Quick start

1. Launch any VPS (Ubuntu/Debian), get root SSH.
2. Stage your secrets (see `host/templates/env.example`).
3. Hand the VPS + repo + secrets to an OpenMINIS agent with `RUNBOOK.md`.
4. Or follow `SETUP.md` yourself.

License: MIT.
