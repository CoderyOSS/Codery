# Codery

[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![GitHub Release](https://img.shields.io/github/v/release/CoderyOSS/Codery)](https://github.com/CoderyOSS/Codery/releases)
[![CI](https://github.com/CoderyOSS/Codery/actions/workflows/ci.yml/badge.svg)](https://github.com/CoderyOSS/Codery/actions/workflows/ci.yml)
![Status](https://img.shields.io/badge/status-experimental-orange)

**Codery is the infrastructure this site runs on**: a self-hosted AI development platform
where coding agents work inside isolated containers, projects deploy as web apps with
zero-downtime blue/green releases, and a Rust control plane owns routing, health,
rollback, and diagnostics — all reachable over a private tailnet.

<!-- TODO: hero screenshot — CoderyCI dashboard (Host Health + services). -->
<!-- TODO: second screenshot — a deploy mid-preview (blue/green state). -->

## Why this exists

Running AI coding agents directly on a host means giving a model's shell your kernel,
your Docker socket, and your home directory. Codery inverts that: agents get an
isolated container on a shared, bind-mounted projects tree — and *nothing else*.
Every project runs as its own supervised process in a separate runtime container.
The host runs one small Rust binary that turns `git push`-driven builds into
health-checked blue/green cutovers it can also diagnose and roll back.

## Components

| Component | What it is |
|-----------|------------|
| **CoderyCI** | Rust orchestrator binary + web dashboard: blue/green deploys, preview staging, TCP port proxy, MCP control plane, host metrics, `diagnose`, rollback |
| **Paseo** | Agent environment (paseo.sh daemon + OpenCode CLI); iPad/CLI clients connect over WebSocket; hardened tailnet-only SSH |
| **Apps** | Project web servers (Bun/Node), s6-supervised; every app gets a subdomain, basic-auth gates available |
| **OpenDesign** | Standalone design workspace (upstream image + pinned wrapper), same blue/green treatment |
| **Playwright** | Dedicated browser runtime container — the only place Chromium runs |
| **Host layer** | Caddy (TLS + subdomain routing), Tailscale (the perimeter), supervisord |

## Engineering notes

- **Blue/green, not mutable containers.** Every service deploys to the inactive
  color, health-checks, verifies the replacement is running, *then* cuts over —
  cutover refuses to stop the live container without a verified replacement.
- **Declarative services.** Adding a service is one YAML file (`service.yml`):
  ports, volumes, health checks, env, network. CoderyCI derives Docker port
  bindings, Caddy vhosts, Nginx config, and color offsets from it.
- **One trust boundary.** Nothing is public. External access is Tailscale; Caddy
  serves subdomains over the tailnet; raw TCP (SSH) binds the tailnet IP only.
- **Agent containment.** Agent containers have no Docker socket, no host config,
  no Caddy/Tailscale access. Route and infra changes go through this repo.
- **Operated, not demo'd.** Memory budgets with OOM mitigations, a postmortem
  playbook, `codery-ci diagnose` state-vs-reality reconciliation, and preview
  deploys that never touch live traffic. See [ARCHITECTURE.md](ARCHITECTURE.md).

## Quickstart

Provision an Ubuntu 24.04 VPS, apply [`hosting/cloud-init.yaml`](hosting/cloud-init.yaml),
then follow [SETUP.md](SETUP.md). In short:

```bash
# 1. Clone this repo on (or into) the box, configure /opt/codery/.env
# 2. One-time host setup (Caddy, Tailscale, supervisord):
gh workflow run "Setup Host"
# 3. Deploy services (manual pipelines — pushes never deploy):
gh workflow run "Build Paseo"
gh workflow run "Build Apps"
```

## Status

Experimental / pre-1.0. Codery runs real infrastructure — it serves this site —
but its interfaces and configuration formats are still evolving. It is not yet a
polished product for strangers; semver applies to what ships, and documented
breaking changes land as `feat!:` commits.

## Documentation

- [ARCHITECTURE.md](ARCHITECTURE.md) — architecture, port scheme, deploy semantics, operational lessons
- [AGENTS.md](AGENTS.md) — the reference agents load when working inside this repo
- [SETUP.md](SETUP.md) — VPS installation guide
- [docs/customizing.md](docs/customizing.md) — GitHub App auth, domains, env
- [docs/playwright.md](docs/playwright.md) — browser runtime boundary

## Support

[![GitHub Sponsors](https://img.shields.io/badge/GitHub-Sponsors-ea4aaa?logo=github)](https://github.com/sponsors/rm-rf-etc)
