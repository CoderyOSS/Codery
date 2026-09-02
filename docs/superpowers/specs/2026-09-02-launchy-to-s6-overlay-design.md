# Design: Full Launchy → s6-overlay Migration

**Date:** 2026-09-02
**Status:** Approved (all decisions confirmed with user via Q&A)
**Implementation plan:** `docs/superpowers/plans/2026-09-02-launchy-to-s6-overlay.md`

---

## Context

Codery has three process-supervision implementations in play:

| Layer | Supervisor today | State |
|---|---|---|
| apps container | **s6-overlay v3.2.3.2** (`/init` → s6-svscan) | Migration complete at image level (commit `c7c2c55`) |
| sandbox container | **Launchy** (`/sbin/launchy /etc/launchy.json` PID 1) | Untouched by the apps migration |
| orchestrator (codery-ci) | speaks the **Launchy protocol** | `add_app`/`remove_app`/`restart_app`/`get_app_status` broken against live apps (handoff.md Issue 2) |

Liabilities in the status quo:

- The checked-in Launchy binary blob (`containers/sandbox/bin/launchy`) **predates its own source by two feature commits** — the running sandbox Launchy has no SIGHUP reload, no status file, no `include_dirs`, no priority support. No CI job rebuilds it; drift is structural.
- `sync_launchy` writes app configs to `/opt/codery/apps-launchy.d`, a host dir **no longer bind-mounted anywhere** (service.yml now mounts `/opt/codery/apps-s6.d` → `/etc/s6-overlay/apps.d`).
- SQLite rows for `cartaclient`/`cbe1`/`design` (Launchy era) are a duplicate source of truth: the image's s6-rc bundles run the processes; the DB rows supply only their routes.
- Two supervision models to document and reason about forever.

## Decision (confirmed)

**All containers on s6-overlay; Launchy deleted entirely.** Phased: (1) orchestrator learns s6 → (2) sandbox image migrates → (3) delete Launchy + docs sweep. Playwright container is pull-only (Microsoft image) and out of scope. Host-layer supervisord is out of scope (not a container).

Sub-decisions:

1. `.devcontainer/devcontainer.json` is **deleted** — consumed only by Launchy; sandbox services become s6-rc bundles under `containers/sandbox/s6-overlay/`.
2. `restart_count` is **dropped** from the status API — s6 does not track it and nothing consumes it (verified: mcp.rs only reads `name`/`pid` programmatically).
3. Build-time vs runtime apps are distinguished by a **`source` DB column** (`ALTER TABLE apps ADD COLUMN source TEXT NOT NULL DEFAULT 'runtime'`, same pattern as the existing `no_cache` migration), not container probing — deterministic across blue/green transitions, keeps `db.rs` host-side-pure.
4. Runtime-app persistence across container restarts is restored by a **boot-time oneshot** (`runtime-apps`) baked into the apps image that `s6-svlink`s every bundle in `/etc/s6-overlay/apps.d/` — the functional equivalent of Launchy's startup `include_dirs` scan.

## API equivalence (Launchy → s6-overlay)

| Launchy API (as consumed) | s6-overlay equivalent | Notes |
|---|---|---|
| JSON config per app, host→container bind mount | s6 service bundle dir per app (`type`, `run`, `finish`, `timeout-kill`, `dependencies.d/base`) in `/opt/codery/apps-s6.d` | Mount already exists in `service.yml` |
| SIGHUP hot-reload (global rescan) | `s6-svlink` / `s6-svunlink` per service via `docker exec` | Explicit, race-free (blocks until supervisor spawns/exits) |
| Status file `{name,pid,status,uptime_secs,restart_count}` | `s6-svstat -o up,pid,updownfor` (machine-readable, documented) | `restart_count` dropped; everything else covered |
| restart `always` / `never` / `on_failure` | longrun default / `finish` running `s6-svc -d .` / conditional `finish` | Full parity |
| per-service `user` | `s6-setuidgid <user>` in run script | Full parity |
| `directory`, `env` map | `cd` + `export` lines in generated run script | Full parity; env imported from `/run/s6/container_environment` where needed |
| `priority` startup ordering | s6-rc `dependencies.d/` (build-time only) | Runtime hot-adds have no ordering need |
| Logs → container stdout (`Stdio::inherit`) | s6 default (`S6_LOGGING=0`) → container stdout | Identical consumption via `docker logs` |
| SIGTERM → 10s grace → SIGKILL | `timeout-kill` = `10000` per bundle + s6-overlay stage 3 | Same semantics, per-service; repo has full tuning guide |

The three app-installation layers are preserved:

1. **Build-time** — image-baked s6-rc bundles + `user-bundles.d` (works today; SQLite rows marked `source='build'` provide routing only).
2. **Past-session restore** — SQLite (host source of truth) → `sync_s6` renders bundles to host dir → bind mount → `runtime-apps` oneshot links them at boot.
3. **Hot-add to active container** — `add_app` → SQLite → `sync_s6` → `s6-svlink` (verified up via `s6-svstat`). Removal: `s6-svunlink`. Restart: `s6-svc -t` (escalate `-k`).

## Architecture

### Phase 1 — orchestrator speaks s6

`system/orchestrator/`:

- `db.rs` — `source` column migration; `AppRecord.source`; `set_app_source()`; `sync_launchy` → `sync_s6` (renders s6 bundles for `runtime` rows only, prunes stale dirs); unit tests.
- `s6.rs` (new) — `SvcStat`, `parse_svstat_batch()`, `SVSTAT_BATCH_CMD` (bash one-liner enumerating `/run/service`, filtering the three s6-internal services), `S6_INTERNAL_SERVICES`.
- `config.rs` — `APPS_LAUNCHY_DIR` → `APPS_S6_DIR = "/opt/codery/apps-s6.d"`.
- `mcp.rs` — `add_app` (optional `source` param; svlink + up-verify), `remove_app` (refuses `build` rows; svunlink), `restart_app` (`s6-svc -t`/`-k`, pid-change poll), `get_app_status` (svstat batch + DB join), `get_supervisor_status` (s6-first with supervisord fallback for non-s6 containers), guidance strings + `INSTRUCTIONS` rewrite.
- `main.rs` — `set-app-source <name> <build|runtime>` subcommand (one-time marking of the 3 baked apps; future use).
- apps image: `runtime-apps` restore oneshot + `runtime-apps-restore` script.

Deploy: codery-ci release (tag `codery-ci-v*`) → release workflow → `Deploy CoderyCI` workflow; apps image via local build loop (`codery_exec` build → deploy-preview → user cutover); one-time `set-app-source ×3` on the host.

**Ordering constraint:** `set-app-source` must run **before** the oneshot image cuts over — otherwise a container restart would double-start the baked apps from their rendered bundles.

### Phase 2 — sandbox image on s6-overlay

- `containers/sandbox/s6-overlay/s6-rc.d/` — 6 longrun bundles (`sshd`, `opencode`, `opencode-diff-pruner`, `opencode-serve-guard`, `tmux`, `opendesign`), each: `type`, `run`, `dependencies.d/base`, `timeout-kill` (10000); plus `user-bundles.d/user/contents.d/` entries.
- `docker-entrypoint.d/` → `cont-init.d/` — s6-overlay runs these before services; all get `#!/command/with-contenv bash` shebangs (restores full docker-env access). `15-render-domain.sh` writes `/run/env/opendesign.env` (replaces sed-on-launchy.json).
- `s6-import-container-env` helper — run scripts source it to import `/run/s6/container_environment` (parity with Launchy children inheriting container env); `S6_IMPORT_SKIP` lets opendesign apply its own env overrides.
- `configuration.nix` — drop launchy/devcontainer/entrypoint copies; add cont-init.d + s6-overlay + helper copies; `/var/run` becomes a symlink to `/run`.
- `examples/Dockerfile.sandbox` — s6-overlay tarball stage (pin 3.2.3.2, matching apps), `ENTRYPOINT ["/init"]`, `S6_BEHAVIOUR_IF_STAGE2_FAILS=1` (warn-and-continue parity), `S6_KILL_GRACETIME=250`.
- Delete `.devcontainer/`.
- Deploy: local build → deploy-preview → verify → user cutover (kills the agent session; standard for sandbox redeploys).

### Phase 3 — delete Launchy + docs

- Delete `system/launchy/`, `containers/sandbox/bin/launchy`, `containers/sandbox/scripts/entrypoint.sh`, `handoff.md`.
- Docs sweep: `AGENTS.md`, `containers/sandbox/agents_file`, `SETUP.md`, `containers/apps/README-NIX.md`, stale comments (`flake.nix`, `healthcheck.sh`, `deploy-sandbox.yml`).

## Risks

| Risk | Mitigation |
|---|---|
| Double-start of baked apps if bundles render before `source` marking | Deploy order enforced: new codery-ci → `set-app-source ×3` → oneshot image cutover |
| s6-overlay on the nix-built scratch rootfs | Static binaries; bash present; apps container proves host compatibility; preview-deploy verification before cutover |
| Env regressions for opencode (API keys, PATH, HOME) | `s6-import-container-env` restores full container env; HOME set explicitly per run script |
| cont-init.d failure kills boot | `S6_BEHAVIOUR_IF_STAGE2_FAILS=1` = warn-and-continue (matches current entrypoint semantics) |
| `/var/run` real dir conflicts with s6-overlay's symlink fix | Make it a symlink at build time |
| Sandbox cutover kills the agent session | Standard for this repo; preview-verify first; `codery-ci rollback sandbox` available |

## Verification

- Rust: `cargo test` in `system/orchestrator` (via `ssh gem@apps` — sandbox has no compiler).
- Phase 1 e2e: probe app add → svstat up → curl 200 → restart (new pid) → remove → gone; persistence probe across `restart_service apps`.
- Phase 2: preview checks (opencode UI, svstat all services, tmux session, sshd on preview port, opendesign port, `gh auth status`, clean cont-init logs).
- Final: repo-wide `rg -i launchy` shows only historical design docs.
