# Handoff: Spotted Issues (apps container)

Two issues surfaced during the Elixir-UTF8 work. Both are **pre-existing** and
**independent** of that fix. They block runtime-app management and clean nginx
route reloads respectively.

- **Status**: green preview (`codery-apps-green`, tag `apps-elixir-utf8-v2`) is
  verified working for the UTF8 fix. These two issues are orthogonal.

---

## Issue 1 — nginx reload fails: `getgrnam("nogroup") failed`

### Symptom

Every `nginx::generate_and_reload()` call emits to stderr:

```
nginx: [emerg] getgrnam("nogroup") failed
```

Reproduced live:
```bash
ssh gem@apps 'sudo nginx -s reload'
# nginx: [emerg] getgrnam("nogroup") failed
```

Consequence: the orchestrator writes `/opt/codery/proxy/apps-nginx.conf` (the
host-side file, bind-mounted into the container) but the **in-container reload
never applies**. New/changed app routes only take effect on full container
restart, not on `reload_routes` / `add_app` / `remove_app`.

Note: nginx *itself runs fine* — port 8080 returns 404, master is up, pid file
present at `/run/nginx.pid`. Only the reload signal is broken.

### Root cause

`system/orchestrator/src/nginx.rs:97`:

```rust
cmd: Some(vec!["nginx", "-s", "reload"]),
```

The reload runs **without `-c`**, so nginx reads its **compiled-in default
config** (`/nix/store/...-nginx-1.28.0/conf/nginx.conf`), not
`/etc/nginx/nginx.conf`. That nix default config has `user` commented out, so
nginx falls back to its **compile-time group default — `nogroup`** — which
doesn't exist in this image (`/etc/group` has `nobody:65534` and `www-data:33`,
but no `nogroup`).

The s6-rc `run` script starts nginx correctly with
`nginx -c /etc/nginx/nginx.conf` (which declares `user www-data;` — group
exists). Only the reload diverges.

### Why not "just add a nogroup group"?

That masks the real bug. The reload still reads the *wrong config* (nix default,
wrong pid path `logs/nginx.pid` instead of `/run/nginx.pid`). Even if the emerg
went away, the reload would target the wrong master.

### Recommended fix

Pass the explicit config path so reload reads the same file nginx started with:

```rust
// system/orchestrator/src/nginx.rs:97
cmd: Some(vec!["nginx", "-s", "reload", "-c", "/etc/nginx/nginx.conf"]),
```

That config sets `pid /run/nginx.pid`, so the reload finds the live master.
One-line change, no Dockerfile edit. Test after rebuilding the orchestrator:

```bash
# rebuild codery-ci, then from host:
codery-ci reload-routes   # should print "Reloaded Nginx in codery-apps-<color>"
# verify no emerg:
ssh gem@apps 'sudo nginx -t -c /etc/nginx/nginx.conf'
```

### Secondary hardening (optional)

Add `nogroup` to `/etc/group` in `containers/apps/Dockerfile` as defense in
depth, so any future bare `nginx` invocation doesn't emerg:

```dockerfile
RUN echo 'nogroup:x:65534:' >> /etc/group
```

But the `-c` fix is the real cure.

---

## Issue 2 — runtime app management (add_app etc.) targets a non-existent Launchy

### Symptom

The MCP app-management tools are **broken** against the current apps image:

- `add_app` writes a config + sends `kill -HUP 1` to PID 1, but PID 1 is now
  **s6-overlay** (s6-svscan), not Launchy. s6 ignores the HUP. The app never
  starts.
- `get_app_status` / `list_apps` try to `cat /run/launchy-status.json` and
  `ls /etc/launchy/built-in/` — both absent → errors like
  *"Launchy status file not found"*.
- `restart_app` / `remove_app` similarly no-op or misreport.

### Root cause

The apps container was migrated from **Launchy** (Rust PID 1) to **s6-overlay
v3** (s6-svscan PID 1 + s6-rc longruns). The orchestrator's app-management code
was never updated — it still speaks the Launchy protocol:

| Orchestrator action | What it does (Launchy-era) | Reality now (s6) |
|---|---|---|
| `db::sync_launchy` (`db.rs:317`) | Writes `{name}.json` into `/opt/codery/apps-launchy.d` (bind → `/etc/launchy/apps.d/`) | Dir is bind-mounted to `/etc/s6-overlay/apps.d/` now, but JSON is **not** a valid s6 service bundle |
| `add_app` (`mcp.rs:1110`) | `kill -HUP 1` to tell Launchy to hot-reload | s6-svscan doesn't HUP-reload service dirs |
| status reads (`mcp.rs:1121,1224,1340`) | `cat /run/launchy-status.json` | No such file; s6 has no JSON status |
| built-in list (`mcp.rs:1353`) | `ls /etc/launchy/built-in/` | No such dir |

The service.yml already pivoted the bind mount:
```yaml
# containers/apps/service.yml:46-47
host: /opt/codery/apps-launchy.d        # (stale name, still the host dir)
container: /etc/s6-overlay/apps.d
```
…and `containers/apps/Dockerfile:250` does `mkdir -p /etc/s6-overlay/apps.d`.

The **intended s6 runtime-app flow is already documented** in
`containers/apps/README-NIX.md:137-138`:
1. Create an s6 service directory at `/etc/s6-overlay/apps.d/<name>/` with
   `type: longrun`, a `run` script, and `dependencies.d/base`.
2. Link it into supervision: `s6-svlink /run/service /etc/s6-overlay/apps.d/<name>`.

The orchestrator just hasn't been taught to emit that shape.

### Scope of the migration (what needs to change)

1. **`db.rs::sync_launchy` → `sync_s6`** — instead of one JSON file, write an
   s6-rc service bundle per app:
   - `/etc/s6-overlay/apps.d/<name>/type` → `longrun`
   - `/etc/s6-overlay/apps.d/<name>/run` → shebang + `s6-load-locale-env` +
     `cd <directory>` + `exec <command>` (run as `gem`)
   - `/etc/s6-overlay/apps.d/<name>/dependencies.d/base` (so it starts after
     base services)
   - Optionally `/etc/s6-overlay/apps.d/<name>/run.user` / `notification-fd`
     for readiness.
2. **`mcp.rs::add_app`** — replace `kill -HUP 1` with s6 bring-up:
   `s6-svlink /run/service /etc/s6-overlay/apps.d/<name>` (per README-NIX),
   then poll `s6-svstat /run/service/<name>`.
3. **`mcp.rs::remove_app`** — `s6-svc -d /run/service/<name>` (down),
   `s6-svunlink`, then `rm -r` the bundle dir.
4. **`mcp.rs::restart_app`** — `s6-svc -t /run/service/<name>` (restart).
5. **Status reads** — replace `cat /run/launchy-status.json` with
   `s6-svstat /run/service/<name>` (parse `up/down` + epoch timestamp for
   uptime). Built-in app list = `ls /etc/s6-overlay/s6-rc.d/` filtered to
   runtime apps, or enumerate `/run/service/`.
6. **`config.rs` constant rename** — `APPS_LAUNCHY_DIR` → `APPS_S6_DIR`
   (host dir `/opt/codery/apps-launchy.d` can keep its path or be renamed;
   the bind target is already `/etc/s6-overlay/apps.d`).
7. **Log path** — s6 longruns log via `s6-log` to
   `/run/service/<name>/...` or a configured logger. Update the
   `read_container_file … /var/log/launchy/<name>.log` guidance strings to the
   new s6 log location (likely `/run/s6-log/<name>/` or whatever the bundle's
   `log/run` points at). Decide and set a `producer-for` / logger, or use
   s6's default catch-all.
8. **Tests** — `db.rs` and `nginx.rs` tests reference the old shape; update
   any `sync_launchy` unit tests to the bundle layout.

### Verification plan after migration

```bash
# add a throwaway app
# (via MCP) add_app name='probe' subdomain='probe' internal_port=8099 \
#   command='python3 -m http.server 8099' directory='/tmp'
ssh gem@apps 's6-svstat /run/service/probe'          # → up (pid N) ...
curl -s -o /dev/null -w "%{http_code}\n" http://127.0.0.1:8099/   # → 200
ssh gem@apps 'ls /etc/s6-overlay/apps.d/probe/'      # type, run, dependencies.d
# (via MCP) get_app_status   # → probe listed, uptime shown
# (via MCP) restart_app name='probe'   # → new pid
# (via MCP) remove_app name='probe'
ssh gem@apps 'ls /run/service/probe' 2>&1            # → No such file
```

### Build/deploy note

Orchestrator changes rebuild `codery-ci` (Rust). Per AGENTS.md: build on the
host (full toolchain) — the **sandbox has no compiler**. Use the MCP build loop
or `Build Orchestrator` workflow, then `codery-ci deploy apps` is **not**
required (these are host-side orchestrator code paths), but the binary must be
updated at `/opt/codery/codery-ci` and `codery-ci-mcp` restarted.

---

## Priority

| Issue | Impact | Effort |
|---|---|---|
| 1. nginx reload `-c` | Route changes don't hot-apply; masked by full deploys | Trivial (1 line) |
| 2. Launchy→s6 runtime apps | `add_app`/`remove_app`/`restart_app`/`get_app_status` all broken | Moderate (orchestrator rewrite of ~5 fns + tests) |

Issue 1 is a quick win and should land first — it also makes Issue 2's
verification (nginx route per new app) actually work.

---

## Verification status (2026-08-21)

Checked live containers + repo on 2026-08-21. Summary: **apps is s6-only,
sandbox is still Launchy, orchestrator is still Launchy-era.**

### Live containers

| Container | PID 1 | Supervisor | Evidence |
|---|---|---|---|
| sandbox (blue, image Aug 9) | `/sbin/launchy /etc/launchy.json` | Launchy | `opencode serve` + sshd + opendesign managed by Launchy; `15-render-domain.sh` still sed-substitutes `/etc/launchy.json` |
| apps | `/init` → `s6-svscan` | s6-overlay v3 | `/run/service/{nginx,sshd,ssh-agent,cartaclient,cbe1,design}` all s6-svstat'd; no `/etc/launchy`, no `/run/launchy-status.json` |

### Repo state

- `containers/apps/` — fully s6: `Dockerfile` (`ENTRYPOINT ["/init"]`,
  s6-overlay 3.2.3.2 tarballs), `s6-overlay/s6-rc.d/` + `user-bundles.d/`,
  `README-NIX.md` documents s6-rc flow, `service.yml` binds
  `/opt/codery/apps-s6.d` → `/etc/s6-overlay/apps.d`. Launchy gone from apps.
- `containers/sandbox/` — still Launchy end-to-end: `Dockerfile.base` copies
  `bin/launchy` to `/sbin/launchy`, `nixos/configuration.nix` copies
  `.devcontainer/devcontainer.json` → `/etc/launchy.json`,
  `scripts/entrypoint.sh` execs `/sbin/launchy`. No s6 anywhere in sandbox.
  Launchy is **not** deprecated for sandbox — only for apps.
- `system/orchestrator/` — still Launchy-era for app management. Confirmed
  unchanged since handoff:
  - `db.rs:317` `sync_launchy()` writes JSON bundles
  - `mcp.rs:903,1121,1224,1340` `cat /run/launchy-status.json` (→
    `get_app_status` returns MCP error `-32603: Launchy status file not
    found` live)
  - `mcp.rs:1353` `ls /etc/launchy/built-in/`
  - `mcp.rs` tool descriptions still advertise Launchy ("Both containers
    use Launchy as PID 1", `/var/log/launchy/{name}.log` guidance)
  - `config.rs:7` `APPS_LAUNCHY_DIR = "/opt/codery/apps-launchy.d"`
  - `nginx.rs:97` still `nginx -s reload` without `-c` (Issue 1 unfixed)

### New drift found

`config.rs:7` still points at `/opt/codery/apps-launchy.d`, but
`containers/apps/service.yml:46` and `.github/workflows/deploy-apps.yml:81`
now use **`/opt/codery/apps-s6.d`**. So even the Launchy-era JSON sync writes
to a host dir that is no longer bind-mounted. Rename the constant as part of
the Issue 2 migration (handoff plan step 6 already calls for this).

### Duplicate source of truth for the 3 runtime apps

SQLite (`list_apps`) and the image both define `cartaclient`, `cbe1`,
`design`. The image copies (`containers/apps/s6-overlay/s6-rc.d/*`) are what
actually run — baked at build time. The SQLite records date from the Launchy
era (2026-07-23) and are ignored by s6. Decide: either keep build-time apps
in SQLite and render bundles (Issue 2 migration), or delete the stale records.

### Impact on live ops

- `add_app`/`remove_app`/`restart_app`/`get_app_status` are dead against the
  current apps image. `list_apps` works (reads SQLite on the host).
- `reload_routes` still regenerates Caddy/Nginx config but the nginx reload
  inside the apps container is a no-op (Issue 1).
- Sandbox supervision is unchanged — Launchy is alive there. If a sandbox
  s6 migration is planned, it has not started in this repo.
