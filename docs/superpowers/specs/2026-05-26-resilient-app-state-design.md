# Resilient App State: SQLite + Layered Routes

**Date:** 2026-05-26
**Status:** Approved

## Problem

Codery MCP state is spread across multiple files that can desync during container changes:

- `apps-routes.json` — Caddy/Nginx route source (host-only)
- `apps-launchy.d/*.json` — Launchy process configs (bind-mounted into container)
- `apps-nginx.conf` — Nginx internal routing (bind-mounted into container)

When `reload_routes` or a deploy regenerates Caddy from `apps-routes.json`, runtime apps added via MCP (`add_app`) can lose their routes while their Launchy processes keep running. The incident: a runtime app had a Launchy config but no Caddy route because `apps-routes.json` was `[]`.

Additionally, route declarations are fragmented across four files (`service.yml`, `host-routes.json`, `sandbox-routes.json`, `apps-routes.json`) with no clear override semantics.

## Design

### Two-state architecture

| Source | What it holds | Mutability |
|--------|--------------|------------|
| **SQLite** (`/opt/codery/codery.db`) | Runtime apps (added/removed via MCP) | Dynamic |
| **Static config** (`proxy/routes.yaml`) | Host services, user-specific routes | Version-controlled, rare changes |
| **Service YAMLs** (`service.yml`) | Container infrastructure routes | Version-controlled, changed with deploys |

### Layered route system

Routes are resolved in priority order. Later layers override earlier layers by **subdomain** with **full replacement** semantics:

```
Layer 1 (base):  service.yml  ports[].subdomain   — container infrastructure
Layer 2 (mid):   routes.yaml  static routes        — user overrides
Layer 3 (top):   SQLite apps                       — runtime, highest priority
```

If a SQLite entry is removed, lower-layer entries for that subdomain reappear.

### Merge algorithm (`build_route_map()`)

```
1. For each service.yml:
   - Parse ports[].subdomain → Route { subdomain, port: container_port, target: service_name }
   - Insert into HashMap<subdomain, Route>

2. Load proxy/routes.yaml:
   - Parse routes[] → Route entries
   - Upsert into map (overwrites by subdomain)

3. Query SELECT * FROM apps:
   - Map each row → Route { subdomain, port: 8080, target: "apps", internal_port }
   - Upsert into map (overwrites by subdomain)

4. For each route in unified map:
   - target: host    → caddy_block(fqdn, port)           // no offset
   - target: sandbox → caddy_block(fqdn, port + sandbox_color_offset)
   - target: apps    → caddy_block(fqdn, 8080 + apps_color_offset) + nginx internal routing
```

### Static routes file — `proxy/routes.yaml`

Replaces `host-routes.json` and `sandbox-routes.json`. Does NOT replace `service.yml` ports.

```yaml
# Static routes — host services and user-specific overrides.
# Layer 2: overrides service.yml by subdomain, overridden by SQLite.

routes:
  - subdomain: mcp
    port: 4040
    target: host

  - subdomain: ci
    port: 4041
    target: host

  - subdomain: trailhead
    port: 4050
    target: host
```

Route entry schema:
- `subdomain` (string, required) — subdomain prefix or full FQDN
- `port` (integer, required) — container port (for container targets) or host port (for host targets)
- `target` (string, required) — `"host"`, `"sandbox"`, or `"apps"`

### SQLite schema — `/opt/codery/codery.db`

```sql
CREATE TABLE apps (
    name          TEXT PRIMARY KEY,
    subdomain     TEXT NOT NULL UNIQUE,
    internal_port INTEGER NOT NULL,
    command       TEXT NOT NULL,
    directory     TEXT NOT NULL,
    env           TEXT,              -- JSON object string, nullable
    priority      INTEGER NOT NULL DEFAULT 100,
    user          TEXT NOT NULL DEFAULT 'gem',
    restart       TEXT NOT NULL DEFAULT 'always',
    created_at    TEXT NOT NULL DEFAULT (datetime('now'))
);
```

DB lifecycle:
- Created by CoderyCI on first startup if missing
- Schema migration on startup if DB exists but is missing columns/tables
- Never shipped as artifact — purely operational state on the host
- Survives deploys, reboots, and CoderyCI binary updates

### Mutation cascade

Every write to SQLite (`add_app`, `remove_app`) triggers:

```
SQLite INSERT/DELETE
  ├─→ build_route_map() — merge all 3 layers into unified map
  ├─→ sync_launchy(): SELECT * FROM apps → generate apps-launchy.d/*.json, SIGHUP Launchy
  ├─→ caddy::apply_all(): unified map → Caddyfile, caddy reload
  └─→ nginx::generate_and_reload(): SQLite → apps-nginx.conf, nginx -s reload
```

### Startup sequence

```
1. Create/migrate SQLite DB if needed
2. build_route_map() — merge service.yml + routes.yaml + SQLite
3. Generate Caddyfile from unified map
4. Generate apps-nginx.conf from SQLite
5. sync_launchy() — render Launchy JSON directory from SQLite
```

### Read operations

| Tool/Module | Change from current |
|-------------|-------------------|
| `list_apps` | `SELECT * FROM apps` instead of reading `apps-routes.json` |
| `get_routes` | Uses `build_route_map()` instead of reading `apps-routes.json` |
| `get_app_status` | Same as today (container status file), metadata from SQLite |
| `reload_routes` | `build_route_map()` from all 3 layers instead of 4 separate files |
| `caddy.rs` | Reads from unified route map instead of 3 JSON files |
| `nginx.rs` | Queries SQLite instead of `apps-routes.json` |

### Files eliminated

| File | Fate |
|------|------|
| `proxy/host-routes.json` | Deleted — merged into `routes.yaml` |
| `proxy/sandbox-routes.json` | Deleted — merged into `routes.yaml` |
| `proxy/apps-routes.json` | Deleted — replaced by SQLite layer |
| `containers/apps/scripts/gen-apps-routes.py` | Deleted — unused |

### Files unchanged

| File | Why |
|------|-----|
| `service.yml` | Keeps `ports[].subdomain` — acts as Layer 1 |
| Launchy binary | No changes needed |
| Launchy config format | No changes needed |
| Launchy config.json | Still reads `/etc/launchy/built-in/` and `/etc/launchy/apps.d/` |

### Files that become derived (generated from SQLite)

| File | Generator | When |
|------|-----------|------|
| `/opt/codery/apps-launchy.d/*.json` | `sync_launchy()` from SQLite | Every SQLite mutation |
| `/opt/codery/proxy/apps-nginx.conf` | `nginx::generate_and_reload()` from SQLite | Every SQLite mutation |
| `/etc/caddy/Caddyfile` | `caddy::apply_all()` from unified map | Every mutation, deploy, startup |

### Rust changes

- **New crates:** `rusqlite` (bundled feature), `serde_yaml`
- **New module:** `src/db.rs` — SQLite CRUD, `sync_launchy()`, `build_route_map()`
- **Modified:** `src/caddy.rs` — read from unified route map
- **Modified:** `src/nginx.rs` — query SQLite
- **Modified:** `src/mcp.rs` — `add_app`/`remove_app`/`list_apps` use SQLite + cascade
- **Modified:** `src/config.rs` — new path for `routes.yaml`, remove old JSON paths
- **Modified:** `src/main.rs` — startup calls DB init + `build_route_map()`

### CI pipeline changes

- Remove `gen-apps-routes.py` step from `deploy-apps.yml`
- Remove `apps-routes.json` sync step from `deploy-apps.yml`
- Add `routes.yaml` sync step to deploy workflows (replaces host-routes + sandbox-routes sync)
- `gen-launchy-conf.py` still generates infra service JSONs (sshd, nginx, ssh-agent) — those aren't apps

### Build-time apps (deferred)

Build-time apps (from `devcontainer.json` `codery.apps` array) have never been used (`apps: []` since inception). Support is deferred:
- Future: `build.rs` reads `devcontainer.json`, compiles app definitions into binary as const data
- At startup: CoderyCI upserts baked-in apps into SQLite with `source = 'build'`
- For now: all apps are runtime-only, added via MCP `add_app`
