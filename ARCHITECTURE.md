# Codery — Architecture

This is the durable architecture reference: what runs where, how traffic flows,
how deploys work, and the operational lessons that shaped the design.
For day-to-day commands see [AGENTS.md](AGENTS.md); for setup see [SETUP.md](SETUP.md).

## The one-paragraph version

Codery is a VPS (Ubuntu) running three layers: a **host layer** (Caddy, Tailscale,
supervisord), a set of **service containers** on a private Docker network
(`codery-net`), and **CoderyCI** — a Rust binary that owns blue/green deployments,
routing generation, health checks, host metrics, and diagnostics. Everything
external enters through Tailscale; Caddy terminates wildcard TLS and routes
subdomains; no port is publicly exposed.

## Components and trust boundaries

```
Internet ──► Tailscale (host) ──► Caddy ──► ┌─ paseo    (agent container)
                                            ├─ apps     (project servers)
                                            ├─ opendesign (design workspace)
                                            └─ playwright (browser runtime)
                                   CoderyCI (host binary): deploy/rollback/diagnose
```

| Boundary | Rule |
|---|---|
| Agent containers → host | Nothing. No Docker socket, no Caddy/Tailscale config, no supervisor. |
| Agent containers → infra | Only this repo: route/service changes are commits, deployed by humans. |
| Anything public | Nothing is public. Tailscale is the perimeter; Caddy binds tailnet-served subdomains. |
| Chromium | Only in the playwright container (`ws://playwright:3000`); never installed elsewhere. |
| Raw TCP (SSH) | Bound to the Tailscale IP only (`bind: tailnet`), key-only auth, no root login. |

## Services (declarative)

Every container service is declared in `containers/<name>/service.yml` and synced
to `/opt/codery/services/<name>.yml` before each deploy. One YAML describes ports,
volumes, health checks, env, and networking; CoderyCI derives everything else.

| Service | Color offsets (blue/green) | Notable ports |
|---|---|---|
| `paseo` | +10000 / +20000 | daemon 6767 (`code.` subdomain), ssh 2022 → stable host :2222 |
| `opendesign` | +10000 / +20000 | web 7456 (`opendesign.`, loopback + basic auth) |
| `apps` | +0 / +10000 | Nginx 8080; app port range 8000–9000; ssh 2022 → stable host :3333 |
| `playwright` | +10000 / +20000 | ws :3000, internal only |

**Port formula**: `host_port = container_port + offset(color)` — offsets come from
each service's YAML (`port_scheme`), not from hardcoded service names.

## Blue/green deployment

1. **Build** → `codery-ci build <service> <tag>` (or the GitHub pipeline; manual-only).
2. **Preview** → `deploy-preview` starts the *inactive* color, health-checks it,
   and exposes it on `<service>-preview.<domain>`. Live traffic is untouched.
3. **Cutover** → verifies the inactive container is running and healthy, rewrites
   the Caddyfile, reloads, writes the new active color to `/opt/codery/state/`,
   stops the old container, prunes images.
   **Cutover refuses to stop the active container without a verified replacement.**
4. **Rollback** → `codery-ci rollback` redeploys the previous locally-cached image
   through the same blue/green path.

State lives in small text files (`/opt/codery/state/{service}.color|.sha`) plus
SQLite (`codery.db`: runtime app routes, preview registrations). `codery-ci
diagnose` reconciles declared state against actual containers, listeners, and
routes, and prints the exact fix commands when they disagree.

A **stable-port TCP proxy** (part of the daemon) forwards host :2222/:3333 to the
*active color's* SSH port, so operators always reach the live container on the
same port.

## Apps lifecycle (3 tiers)

Apps in the apps container are s6-supervised. `/opt/codery/codery.db` is the
source of truth, with a `source` column separating tiers:

1. **Build-time** — bundles baked into the image (`containers/apps/s6-overlay/…`).
2. **Previous session** — `sync_s6` renders bundles to `/opt/codery/apps-s6.d`
   (bind-mounted); a boot-time oneshot links them at container start.
3. **Current session** — `add_app` hot-adds: SQLite insert → bundle render →
   `s6-svlink` → poll until up. Subdomain routing follows instantly.

Routing is independent of supervision: `apps-routes` rows (or build-time routes)
drive Nginx + Caddy; `reload-routes` applies route-only changes with no rebuild.

## Memory budget (8 GiB host, no swap)

| Component | Steady | Peak |
|---|---|---|
| Host stack (dockerd, Caddy, Tailscale) | ~1.5 GiB | ~2 GiB |
| Paseo (daemon + agents grow with sessions) | ~1.4 GiB | ~2.4 GiB |
| Apps | ~1 GiB | ~1.5 GiB |
| Playwright (Chromium) | 0.5–1 GiB | 1.5+ GiB in use |
| Docker build spike (buildkit) | — | +1.5–2.5 GiB |

Lessons that produced hardening (Sep 16 2026 incident: 13 OOM kills in one evening):

- Never run a build while a preview is up — build spike + preview + active stack
  exceeded the box. Previews are cancelled promptly; builds are serialized.
- opencode RSS is capped (serve-guard SIGTERMs at a threshold; s6 respawns in ~15s
  with history intact) so the leak backstop fires before host pressure builds.
- opencode/metrics processes get favorable `oom_score_adj` so the kernel prefers
  killing builds over the live session.
- 16 GiB is the comfortable size for the full stack; the mitigations keep 8 GiB
  workable.

## Engineering standards

- **Zero warnings, enforced in-tree**: `Cargo.toml [lints]` denies rustc warnings,
  `unsafe_code`, `clippy::all`, and `unwrap_used` (tests exempt); clippy::pedantic
  is warn for triage. CI runs fmt/clippy/test/cargo-deny on every push.
- **unsafe policy**: libc calls (flock, sysconf) are scoped, documented with
  SAFETY invariants, and individually allowed.
- **Types at boundaries**: colors, ports, and service names are parsed once at
  IO edges; logic matches on enums (impossible states don't compile).
- **Manual-only deploys**: pushing to `main` never deploys. Pipelines are
  `workflow_dispatch`; production is an environment gate.

## Operational playbook

[docs/playbook.md](docs/playbook.md) is the outage playbook + postmortem
(the 2026-10-18 "box disappeared" incident): triage order, decision shortcuts,
and the hardening that came out of it.
