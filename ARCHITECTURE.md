# Codery — Architecture

What runs where, how traffic flows, and the rules that keep the system
comprehensible. For setup see [SETUP.md](SETUP.md) / [RUNBOOK.md](RUNBOOK.md);
for the agent contract see [AGENTS.md](AGENTS.md).

## The layers

| Layer | Members | Defined in | Deployed by | State |
|---|---|---|---|---|
| **Core** | Dockhand, Zoraxy, Tailscale, host layer | `stacks/core/`, `host/` | host-side compose / Dockhand | `codery-dockhand-data`, `zoraxy_state` volumes, host config |
| **Tools** | paseo, apps, playwright, opendesign, dbgate | `stacks/tools/devtools/`, `images/` | Dockhand (`devtools` stack) | `/opt/codery/data/*` + named volumes |
| **Projects** | your applications | project repos (compose in project root) | Dockhand (per-project stacks) | project-scoped volumes |

**Layer tests** (apply before adding anything):

- management / routing / perimeter → **core** — stock upstream image, no custom
  builds
- developer-facing tool → **tools** — Codery-built image, opt-in via
  `COMPOSE_PROFILES`
- app-specific → **project stack** — lives in the project repo, never in
  `images/` or `/opt/codery/data`

## Host layout (`/opt/codery/`)

```
.env          THE secrets file (0600) — every core/tool stack reads it
stacks/       infra stack dirs: dockhand/, zoraxy/, devtools/ (+ stack .envs)
projects/     project repos — their compose stacks live at each repo root
data/         tool service data: opencode/, opendesign/, dbgate/
ssh/          authorized_keys + agent-to-apps keypair
state/        disabled codery-ci retainers (revert path only)
backups/      codery-backup.sh output
```

Full tree documentation: `/opt/codery/README.md` on the host.

## Traffic flow

```
iPad / dev machines (tailnet)
   │  *.<your-domain> → tailscale IP
   ▼
Zoraxy :443 (wildcard TLS, Cloudflare DNS-01)
   │  proxy-host → codery-net container names (no host ports for HTTP)
   ▼
paseo:6767 · apps:<port> · opendesign:7456 · dbgate:3000
webapp:3000 · api:8000 · dockhand:3000 — whatever your projects run
```

Only SSH, mosh, and transition publishes touch host ports
(`:2222 paseo ssh`, `:3333/:61000 apps`, `:8080 apps nginx — transition-only`,
`:23000 dockhand`, Zoraxy on the tailnet IP). Everything HTTP is name-based
on `codery-net`.

## Boot order & invariants

1. tailscale (host daemon) → 2. docker → 3. stacks. Host sysctl
   `net.ipv4.ip_nonlocal_bind=1` lets tailnet-IP binds succeed before
   tailscale0 exists (`/etc/sysctl.d/99-tailnet-binds.conf`).

Invariants that must never be violated:

- `codery-dockhand-data` volume is never renamed (holds all `dh_` tokens)
- dockhand is never deployed by Dockhand itself (lockout foot-gun) — host-side
  compose only
- `env_file: /opt/codery/.env` requires the file to be mounted into the
  dockhand container (compose resolves env_file client-side)
- container/host names are DNS: Zoraxy upstreams are codery-net names; renaming
  a container breaks routes unless the alias moves with it

## Visibility & permissions

| Consumer | Sees | Writes |
|---|---|---|
| apps (operator terminal) | full `/opt/codery` | `stacks/` as gem; everything else via sudo (secrets 0600) |
| paseo agents | `/opt/codery/{stacks,data}` | read-only |
| projects | their own tree | their own volumes |

## Opt-in tools

`COMPOSE_PROFILES` in the devtools stack `.env` is the inclusion list:
`browser` (playwright), `design` (opendesign), `db` (dbgate). Base (paseo +
apps) is always on. Absent tools degrade gracefully — paseo browser tools fail
fast; opendesign previews error until `browser` is enabled alongside.
