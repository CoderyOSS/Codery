# Launchy → s6-overlay Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove Launchy entirely and put every Codery container on s6-overlay, with s6 providing a full equivalent for every Launchy API the orchestrator consumes.

**Architecture:** Three phases, each independently shippable. Phase 1 teaches codery-ci the s6 protocol (SQLite `source` column, bundle renderer `sync_s6`, `s6-svlink`/`s6-svunlink`/`s6-svc`/`s6-svstat` via docker exec) plus a boot-time restore oneshot in the apps image. Phase 2 migrates the sandbox image to s6-overlay (6 s6-rc bundles, cont-init.d, `/init` entrypoint). Phase 3 deletes Launchy and sweeps docs.

**Tech Stack:** Rust (codery-ci: rusqlite, serde_json, rmcp), s6-overlay 3.2.3.2, Docker, Nix (sandbox rootfs), GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-09-02-launchy-to-s6-overlay-design.md`

## Global Constraints

- **No compiler in the sandbox container.** Rust builds/tests run via the apps container: `ssh gem@apps 'cd /home/gem/projects/Codery/system/orchestrator && cargo test'`. If the profile toolchain is too old: `ssh gem@apps 'sudo nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#gcc -c bash -lc "cd /home/gem/projects/Codery/system/orchestrator && cargo test"'`.
- **Never `git push`** — always `github-push` (works for branches and tags).
- **No Docker socket from the sandbox.** Container operations go through codery MCP tools or `codery_exec` (allowlist: `build`, `validate`, `deploy-preview`, `cancel-preview`).
- **codery-ci deploys only via GitHub Release** → `Deploy CoderyCI` workflow (`build-orchestrator.yml` downloads `releases/latest` — verify the latest release IS the new codery-ci tag before running it).
- **s6-overlay pin: 3.2.3.2** — must match `containers/apps/Dockerfile` `ARG S6_OVERLAY_VERSION`.
- **`cutover` and `deploy` are host-shell user steps** — never agent-run. Preview + verify first.
- **Never read or print secrets** (`.env`, `*.pem`, SSH keys, auth.json). Test existence only.
- **Work on master** (repo convention: direct commits, manual-only deploy workflows).
- **Commit after every task** with a clear message.

---

# PHASE 1 — Orchestrator speaks s6

Outcome: `add_app`/`remove_app`/`restart_app`/`get_app_status` work against the live s6-based apps container; runtime apps persist across container restarts.

### Task 1: `db.rs` — `source` column + `sync_s6` bundle renderer

**Files:**
- Modify: `system/orchestrator/src/db.rs` (init migration:35-67, AppRecord:9-22, insert_app:69-87, list_apps:95-120, sync_launchy:317-359, tests:361-535)
- Modify: `system/orchestrator/src/config.rs:5-7`

**Interfaces:**
- Produces (used by Tasks 3, 4, 5, 8):
  - `AppRecord.source: String` (`"runtime"` | `"build"`)
  - `pub fn set_app_source(conn: &Connection, name: &str, source: &str) -> Result<bool>`
  - `pub fn sync_s6(conn: &Connection) -> Result<()>` (uses `config::APPS_S6_DIR`)
  - `pub fn sync_s6_to_dir(conn: &Connection, dir: &std::path::Path) -> Result<()>` (testable core)
  - `pub fn shell_quote(s: &str) -> String`
  - `pub const APPS_S6_DIR: &str = "/opt/codery/apps-s6.d";` (in config.rs, replaces `APPS_LAUNCHY_DIR`)

- [ ] **Step 1: Write the failing tests**

In `system/orchestrator/src/db.rs`, inside `mod tests` (after `use super::*;`), add a tempdir helper and update `sample_app` to include the new field, then add the new tests:

```rust
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "codery-dbtest-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
```

Update `sample_app` (line 371) — add field `source: "runtime".to_string(),` after `no_cache: false,`.

New tests:

```rust
    // ── source column + sync_s6 ────────────────────────────────────────────

    #[test]
    fn source_defaults_to_runtime() {
        let conn = test_conn();
        insert_app(&conn, &sample_app("myapp")).unwrap();
        let apps = list_apps(&conn).unwrap();
        assert_eq!(apps[0].source, "runtime");
    }

    #[test]
    fn set_app_source_roundtrips() {
        let conn = test_conn();
        insert_app(&conn, &sample_app("myapp")).unwrap();
        assert!(set_app_source(&conn, "myapp", "build").unwrap());
        assert_eq!(find_app_by_name(&conn, "myapp").unwrap().unwrap().source, "build");
        assert!(!set_app_source(&conn, "ghost", "build").unwrap());
    }

    #[test]
    fn sync_s6_renders_bundle_for_runtime_app() {
        let conn = test_conn();
        insert_app(&conn, &sample_app("myapp")).unwrap();
        let dir = temp_dir("render");
        sync_s6_to_dir(&conn, &dir).unwrap();

        let b = dir.join("myapp");
        assert_eq!(std::fs::read_to_string(b.join("type")).unwrap(), "longrun\n");
        assert!(b.join("dependencies.d/base").exists());
        assert_eq!(std::fs::read_to_string(b.join("timeout-kill")).unwrap(), "10000\n");
        assert!(!b.join("finish").exists(), "restart=always needs no finish script");

        let run = std::fs::read_to_string(b.join("run")).unwrap();
        assert!(run.starts_with("#!/bin/bash\n"));
        assert!(run.contains(". /usr/local/bin/s6-load-locale-env\n"));
        assert!(run.contains("export PATH=\"/nix/var/nix/profiles/default/bin:$PATH\"\n"));
        assert!(run.contains("export HOME=/home/gem\n"));
        assert!(run.contains("cd '/home/gem/projects/myapp'\n"));
        assert!(run.contains("exec /command/s6-setuidgid 'gem' bash -c 'bun run start'\n"));

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(b.join("run")).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0, "run must be executable");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sync_s6_shell_quotes_command_and_env() {
        let conn = test_conn();
        let mut app = sample_app("quotey");
        app.command = "echo it's done".to_string();
        app.env = Some(r#"{"GREETING":"va'l ue","PLAIN":"simple"}"#.to_string());
        insert_app(&conn, &app).unwrap();
        let dir = temp_dir("quote");
        sync_s6_to_dir(&conn, &dir).unwrap();

        let run = std::fs::read_to_string(dir.join("quotey").join("run")).unwrap();
        assert!(run.contains("export GREETING='va'\\''l ue'\n"), "run was:\n{}", run);
        assert!(run.contains("export PLAIN='simple'\n"), "run was:\n{}", run);
        assert!(run.contains("bash -c 'echo it'\\''s done'\n"), "run was:\n{}", run);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sync_s6_skips_build_apps_and_prunes_stale_bundles() {
        let conn = test_conn();
        insert_app(&conn, &sample_app("rt")).unwrap();
        insert_app(&conn, &sample_app("bk")).unwrap();
        set_app_source(&conn, "bk", "build").unwrap();
        let dir = temp_dir("prune");
        std::fs::create_dir_all(dir.join("ghost")).unwrap(); // stale bundle
        sync_s6_to_dir(&conn, &dir).unwrap();

        assert!(dir.join("rt").is_dir(), "runtime app bundle rendered");
        assert!(!dir.join("bk").exists(), "build app gets no bundle");
        assert!(!dir.join("ghost").exists(), "stale bundle pruned");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sync_s6_writes_finish_for_restart_policies() {
        let conn = test_conn();

        let mut onfail = sample_app("onfail");
        onfail.restart = "on_failure".to_string();
        insert_app(&conn, &onfail).unwrap();

        let mut never = sample_app("never");
        never.restart = "never".to_string();
        insert_app(&conn, &never).unwrap();

        let dir = temp_dir("finish");
        sync_s6_to_dir(&conn, &dir).unwrap();

        let f1 = std::fs::read_to_string(dir.join("onfail").join("finish")).unwrap();
        assert!(f1.contains("[ \"$1\" = \"0\" ]"), "on_failure finish was:\n{}", f1);
        assert!(f1.contains("s6-svc -d ."), "on_failure finish was:\n{}", f1);

        let f2 = std::fs::read_to_string(dir.join("never").join("finish")).unwrap();
        assert!(f2.contains("exec /command/s6-svc -d ."), "never finish was:\n{}", f2);

        // Flip back to always: finish must be removed on re-render.
        conn.execute("UPDATE apps SET restart = 'always' WHERE name = 'onfail'", []).unwrap();
        sync_s6_to_dir(&conn, &dir).unwrap();
        assert!(!dir.join("onfail").join("finish").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
ssh gem@apps 'cd /home/gem/projects/Codery/system/orchestrator && cargo test db:: 2>&1 | tail -20'
```
Expected: compile error — `AppRecord` has no field `source`, `set_app_source`/`sync_s6_to_dir` not found.

- [ ] **Step 3: Implement**

**3a.** `system/orchestrator/src/config.rs:5-7` — replace the stale comment + constant:

```rust
/// Host directory bind-mounted into the apps container at /etc/s6-overlay/apps.d/.
/// MCP add_app/remove_app render s6 service bundles here; a boot-time oneshot
/// in the apps image links them into supervision.
pub const APPS_S6_DIR: &str = "/opt/codery/apps-s6.d";
```

**3b.** `db.rs` — `AppRecord` (line 9): add field after `no_cache`:

```rust
    pub no_cache: bool,
    pub source: String,
    pub created_at: String,
```

**3c.** `db.rs` `init()` — after the `no_cache` ALTER (line 51-53), add:

```rust
    let _ = conn.execute_batch(
        "ALTER TABLE apps ADD COLUMN source TEXT NOT NULL DEFAULT 'runtime';"
    );
```

**3d.** `insert_app` — add `source` to the SQL and params:

```rust
        "INSERT INTO apps (name, subdomain, internal_port, command, directory, env, priority, user, restart, no_cache, source)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
```
(add `&app.source,` as the 11th param)

**3e.** `list_apps` — SELECT gains `source` (between `no_cache` and `created_at`); `AppRecord` construction gains `source: row.get(10)?,` and `created_at` becomes index 11.

**3f.** New function after `delete_app`:

```rust
pub fn set_app_source(conn: &Connection, name: &str, source: &str) -> Result<bool> {
    let rows = conn
        .execute("UPDATE apps SET source = ?1 WHERE name = ?2", (source, name))
        .with_context(|| format!("failed to set source for app '{}'", name))?;
    Ok(rows > 0)
}
```

**3g.** Replace `sync_launchy` (lines 317-359) entirely with:

```rust
pub fn sync_s6(conn: &Connection) -> Result<()> {
    sync_s6_to_dir(conn, std::path::Path::new(config::APPS_S6_DIR))
}

/// Render one s6 service bundle per runtime-managed app into `dir`, and prune
/// bundles whose app was deleted or is no longer runtime-managed.
pub fn sync_s6_to_dir(conn: &Connection, dir: &std::path::Path) -> Result<()> {
    let apps = list_apps(conn)?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create {}", dir.display()))?;

    // Prune: any subdir that doesn't belong to a runtime app is stale.
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let managed = apps.iter().any(|a| a.name == name && a.source == "runtime");
            if !managed {
                std::fs::remove_dir_all(&path)
                    .with_context(|| format!("failed to prune {:?}", path))?;
            }
        }
    }

    for app in apps.iter().filter(|a| a.source == "runtime") {
        write_bundle(dir, app)?;
    }
    Ok(())
}

fn write_bundle(dir: &std::path::Path, app: &AppRecord) -> Result<()> {
    let bdir = dir.join(&app.name);
    std::fs::create_dir_all(bdir.join("dependencies.d"))?;
    std::fs::write(bdir.join("type"), "longrun\n")?;
    std::fs::write(bdir.join("dependencies.d").join("base"), "")?;
    // Parity with Launchy's shutdown: SIGTERM, 10s grace, SIGKILL.
    std::fs::write(bdir.join("timeout-kill"), "10000\n")?;
    write_executable(&bdir.join("run"), &render_run(app))?;
    match app.restart.as_str() {
        "always" => {
            // s6-supervise default respawns on death; drop any stale finish.
            let _ = std::fs::remove_file(bdir.join("finish"));
        }
        policy => write_executable(&bdir.join("finish"), &render_finish(policy))?,
    }
    Ok(())
}

fn write_executable(path: &std::path::Path, content: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, content).with_context(|| format!("failed to write {:?}", path))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn render_run(app: &AppRecord) -> String {
    let mut s = String::new();
    s.push_str("#!/bin/bash\n");
    s.push_str(". /usr/local/bin/s6-load-locale-env\n");
    s.push_str("export PATH=\"/nix/var/nix/profiles/default/bin:$PATH\"\n");
    s.push_str("export HOME=/home/gem\n");
    if let Some(env_json) = &app.env {
        if let Ok(env_map) = serde_json::from_str::<HashMap<String, String>>(env_json) {
            let mut pairs: Vec<_> = env_map.into_iter().collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0)); // deterministic output
            for (k, v) in pairs {
                s.push_str(&format!("export {}={}\n", k, shell_quote(&v)));
            }
        }
    }
    s.push_str(&format!("cd {}\n", shell_quote(&app.directory)));
    s.push_str(&format!(
        "exec /command/s6-setuidgid {} bash -c {}\n",
        shell_quote(&app.user),
        shell_quote(&app.command)
    ));
    s
}

fn render_finish(policy: &str) -> String {
    match policy {
        // Respawn only on non-zero exit (or signal): down the service on clean exit.
        "on_failure" => "#!/bin/bash\nif [ \"$1\" = \"0\" ]; then exec /command/s6-svc -d .; fi\nexit 0\n".to_string(),
        // Never respawn: always down the service after the first exit.
        _ => "#!/bin/bash\nexec /command/s6-svc -d .\n".to_string(),
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
ssh gem@apps 'cd /home/gem/projects/Codery/system/orchestrator && cargo test db:: 2>&1 | tail -10'
```
Expected: all tests pass, including the 6 new ones. (Note: `main.rs:113` still calls `sync_launchy` — expect a compile error there; fix it now as part of this task: rename the call to `db::sync_s6(&conn)?;`.)

- [ ] **Step 5: Commit**

```bash
git add system/orchestrator/src/db.rs system/orchestrator/src/config.rs system/orchestrator/src/main.rs
git commit -m "codery-ci: apps source column + sync_s6 s6-bundle renderer (replaces sync_launchy)"
```

---

### Task 2: `s6.rs` — svstat batch parser

**Files:**
- Create: `system/orchestrator/src/s6.rs`
- Modify: `system/orchestrator/src/main.rs` (module list — add `mod s6;` next to the other `mod` declarations near the top)

**Interfaces:**
- Produces (used by Tasks 4-7):
  - `pub struct SvcStat { pub name: String, pub up: bool, pub pid: i64, pub uptime_secs: u64 }` (derives `Debug, Clone, PartialEq, Serialize`)
  - `pub const SVSTAT_BATCH_CMD: &str` — bash one-liner for `docker exec bash -c`
  - `pub const S6_INTERNAL_SERVICES: [&str; 3]`
  - `pub fn parse_svstat_batch(output: &str) -> Vec<SvcStat>`

- [ ] **Step 1: Write the failing tests** (in the same new file, bottom `mod tests`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_up_service() {
        let out = "web|true 1234 42\n";
        let stats = parse_svstat_batch(out);
        assert_eq!(stats, vec![SvcStat { name: "web".into(), up: true, pid: 1234, uptime_secs: 42 }]);
    }

    #[test]
    fn parses_down_service() {
        let out = "worker|false -1 7\n";
        let stats = parse_svstat_batch(out);
        assert_eq!(stats[0].up, false);
        assert_eq!(stats[0].pid, -1);
        assert_eq!(stats[0].uptime_secs, 7);
    }

    #[test]
    fn skips_malformed_lines() {
        let out = "garbage\nweb|true 1 2\n|no-name\nbroken|true x 2\n";
        let stats = parse_svstat_batch(out);
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].name, "web");
    }

    #[test]
    fn handles_empty_output() {
        assert!(parse_svstat_batch("").is_empty());
    }
}
```

- [ ] **Step 2: Run to verify failure**

```bash
ssh gem@apps 'cd /home/gem/projects/Codery/system/orchestrator && cargo test s6:: 2>&1 | tail -5'
```
Expected: compile error — `s6` module not found.

- [ ] **Step 3: Implement** `system/orchestrator/src/s6.rs`:

```rust
//! s6 tooling output parsers + the shell snippets used via `docker exec`.
//! All s6 binaries are addressed by absolute path (/command/...) — the
//! container PATH seen by `docker exec` is not guaranteed to include it.

use serde::Serialize;

/// Services s6-overlay runs for its own plumbing — never report as apps.
pub const S6_INTERNAL_SERVICES: [&str; 3] = [
    "s6-linux-init-shutdownd",
    "s6rc-fdholder",
    "s6rc-oneshot-runner",
];

/// Bash one-liner printing `name|up pid uptime_secs` per supervised service.
/// Run as: docker exec <container> bash -c "$SVSTAT_BATCH_CMD".
pub const SVSTAT_BATCH_CMD: &str = r#"for d in /run/service/*/; do n=$(basename "$d"); case "$n" in s6-linux-init-shutdownd|s6rc-fdholder|s6rc-oneshot-runner) continue;; esac; echo "$n|$(/command/s6-svstat -o up,pid,updownfor "$d")"; done"#;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SvcStat {
    pub name: String,
    pub up: bool,
    pub pid: i64,
    pub uptime_secs: u64,
}

pub fn parse_svstat_batch(output: &str) -> Vec<SvcStat> {
    output
        .lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once('|')?;
            if name.is_empty() {
                return None;
            }
            let mut it = rest.split_whitespace();
            let up = matches!(it.next()?, "true");
            let pid: i64 = it.next()?.parse().ok()?;
            let uptime_secs: u64 = it.next()?.parse().ok()?;
            Some(SvcStat {
                name: name.to_string(),
                up,
                pid,
                uptime_secs,
            })
        })
        .collect()
}
```

(plus the tests from Step 1 at the bottom)

- [ ] **Step 4: Run to verify pass**

```bash
ssh gem@apps 'cd /home/gem/projects/Codery/system/orchestrator && cargo test 2>&1 | tail -6'
```
Expected: all tests pass (db + s6).

- [ ] **Step 5: Commit**

```bash
git add system/orchestrator/src/s6.rs system/orchestrator/src/main.rs
git commit -m "codery-ci: s6 module — svstat batch command + parser"
```

---

### Task 3: `mcp.rs` — add_app speaks s6

**Files:**
- Modify: `system/orchestrator/src/mcp.rs` (AddAppParams:106-130, add_app:1052-1157)

**Interfaces:**
- Consumes: `db::sync_s6`, `AppRecord.source` (Task 1)
- Produces: `AddAppParams.source: Option<String>` — `"runtime"` (default) or `"build"` (routing-only registration for image-baked apps; no process management)

- [ ] **Step 1: Add the `source` param**

In `AddAppParams` (after the `no_cache` field, ~line 129):

```rust
    /// 'runtime' (default): orchestrator-managed process via s6 bundle.
    /// 'build': image-baked app — registers routing only, no process management.
    pub source: Option<String>,
```

- [ ] **Step 2: Rewrite the add_app body**

In `add_app`, after the existing validations (name charset, directory exists, port free, name unique — keep all), change record construction and the post-insert flow. Replace the `AppRecord` literal's tail (`priority: 100, user: "gem".to_string(), restart: "always".to_string(), no_cache: ...`) to add:

```rust
            source: p.source.clone().unwrap_or_else(|| "runtime".to_string()),
```

Then replace everything from `db::insert_app` through the Launchy verification block (old lines ~1107-1137) with:

```rust
        let source = p.source.clone().unwrap_or_else(|| "runtime".to_string());
        if source != "runtime" && source != "build" {
            return Err(tool_err(format!(
                "invalid source '{}' — must be 'runtime' or 'build'",
                source
            )));
        }

        db::insert_app(&conn, &record).map_err(|e| tool_err(e.to_string()))?;
        db::sync_s6(&conn).map_err(|e| tool_err(e.to_string()))?;

        if source == "runtime" {
            // Link the freshly rendered bundle into supervision. s6-svlink is
            // race-free by design: it blocks until s6-supervise is spawned.
            let bundle = format!("/etc/s6-overlay/apps.d/{}", p.name);
            container_exec("apps", &["/command/s6-svlink", "/run/service", &bundle])
                .await
                .map_err(|e| tool_err(format!("failed to link s6 service: {}", e)))?;

            // Wait until the service reports up (s6-svstat -o up => "true").
            let svc = format!("/run/service/{}", p.name);
            let mut up = false;
            for _ in 0..10 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                if let Ok(out) =
                    container_exec("apps", &["/command/s6-svstat", "-o", "up", &svc]).await
                {
                    if out.trim() == "true" {
                        up = true;
                        break;
                    }
                }
            }
            if !up {
                return Err(tool_err(format!(
                    "App '{}' config written and route added, but service did not come up. \
                     Service output goes to container stdout — inspect with \
                     get_container_info service='apps'.",
                    p.name
                )));
            }
        }

        // Register routes (Caddy + Nginx) — unchanged from before.
        if let Err(e) = caddy::apply_all() {
            return Err(tool_err(format!("app added but Caddy reload failed: {}", e)));
        }
        if let Err(e) = nginx::generate_and_reload().await {
            return Err(tool_err(format!("app added but Nginx reload failed: {}", e)));
        }
```

(The `AppRecord` is currently constructed inline around line 1100 — name the binding `record` so `insert_app(&conn, &record)` works; keep `no_cache: p.no_cache.unwrap_or(false)`.)

Success response: keep the existing JSON shape but replace the guidance string:

```rust
                "to_read_logs": "Service output goes to container stdout — use get_container_info service='apps'"
```

- [ ] **Step 3: Compile check**

```bash
ssh gem@apps 'cd /home/gem/projects/Codery/system/orchestrator && cargo check 2>&1 | tail -5'
```
Expected: clean (warnings about unused `find_pid`-era code in restart_app are fine — Task 5 removes it).

- [ ] **Step 4: Commit**

```bash
git add system/orchestrator/src/mcp.rs
git commit -m "codery-ci: add_app via s6-svlink + svstat verify; source=build routing-only mode"
```

---

### Task 4: `mcp.rs` — remove_app speaks s6

**Files:**
- Modify: `system/orchestrator/src/mcp.rs` (remove_app:1159-1208)

**Interfaces:**
- Consumes: `db::sync_s6`, `AppRecord.source` (Task 1)

- [ ] **Step 1: Rewrite remove_app**

Replace the body after the app lookup (keep the "not found" error) with:

```rust
        if app.source == "build" {
            return Err(tool_err(format!(
                "'{}' is image-baked (source=build); its process is not orchestrator-managed. \
                 Row left untouched. To fully remove it: delete its bundle from \
                 containers/apps/s6-overlay/s6-rc.d + user-bundles.d, rebuild the apps image, \
                 then run `codery-ci set-app-source {} runtime` on the host and retry remove_app.",
                p.name, p.name
            )));
        }

        // Unlink first while the bundle still exists. s6-svunlink downs the
        // service and waits for the supervisor to exit; warn-and-continue so
        // removal is idempotent even if the service was never linked.
        if let Err(e) = container_exec("apps", &["/command/s6-svunlink", "/run/service", &p.name]).await {
            eprintln!("[remove_app] s6-svunlink warning (continuing): {}", e);
        }

        db::delete_app(&conn, &p.name).map_err(|e| tool_err(e.to_string()))?;
        db::sync_s6(&conn).map_err(|e| tool_err(e.to_string()))?;

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        if let Err(e) = caddy::apply_all() {
            return Err(tool_err(format!("app removed but Caddy reload failed: {}", e)));
        }
        if let Err(e) = nginx::generate_and_reload().await {
            return Err(tool_err(format!("app removed but Nginx reload failed: {}", e)));
        }
```

(The lookup currently reads `db::find_app_by_name(&conn, &p.name)` — keep it, binding `app`.)

- [ ] **Step 2: Compile check**

```bash
ssh gem@apps 'cd /home/gem/projects/Codery/system/orchestrator && cargo check 2>&1 | tail -5'
```
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add system/orchestrator/src/mcp.rs
git commit -m "codery-ci: remove_app via s6-svunlink; refuse source=build rows"
```

---

### Task 5: `mcp.rs` — restart_app speaks s6

**Files:**
- Modify: `system/orchestrator/src/mcp.rs` (restart_app:1210-1307)

**Interfaces:**
- None new (self-contained; works for both `runtime` and `build` apps — `s6-svc` doesn't care who linked the service).

- [ ] **Step 1: Rewrite restart_app**

Replace the entire status-file/pid-dance body with:

```rust
        let svc = format!("/run/service/{}", p.name);

        let pid_of = || async {
            container_exec("apps", &["/command/s6-svstat", "-o", "pid", &svc])
                .await
                .ok()
                .and_then(|out| out.trim().parse::<i64>().ok())
        };

        let old_pid = pid_of().await.unwrap_or(-1);
        if old_pid <= 0 {
            return Err(tool_err(format!(
                "App '{}' is not up (or not supervised by s6). \
                 Check get_app_status for current state.",
                p.name
            )));
        }

        // SIGTERM; s6-supervise respawns automatically (restart=always bundles).
        container_exec("apps", &["/command/s6-svc", "-t", &svc])
            .await
            .map_err(|e| tool_err(format!("failed to signal service: {}", e)))?;

        for attempt in 1..=10u32 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let new_pid = pid_of().await.unwrap_or(-1);
            if new_pid > 0 && new_pid != old_pid {
                return Ok(Json(serde_json::json!({
                    "restarted": p.name,
                    "old_pid": old_pid,
                    "new_pid": new_pid,
                    "note": "Route, bundle, and SQLite record untouched.",
                })));
            }
            if attempt == 3 {
                // App ignored SIGTERM — escalate to SIGKILL.
                let _ = container_exec("apps", &["/command/s6-svc", "-k", &svc]).await;
            }
        }

        Err(tool_err(format!(
            "App '{}' did not restart within 5s (pid still {}). \
             Inspect with get_container_info service='apps'.",
            p.name, old_pid
        )))
```

(Adjust the return type/shape to match the existing handler signature; the success JSON above mirrors the old one minus `restart_count`.)

- [ ] **Step 2: Compile check**

```bash
ssh gem@apps 'cd /home/gem/projects/Codery/system/orchestrator && cargo check 2>&1 | tail -5'
```
Expected: clean. (The old `read_status`/`find_pid` closures and `/run/launchy-status.json` reads in this function are now fully gone.)

- [ ] **Step 3: Commit**

```bash
git add system/orchestrator/src/mcp.rs
git commit -m "codery-ci: restart_app via s6-svc -t/-k with pid-change verification"
```

---

### Task 6: `mcp.rs` — get_app_status + get_supervisor_status via svstat

**Files:**
- Modify: `system/orchestrator/src/mcp.rs` (get_supervisor_status:893-919, get_app_status:1332-1398)

**Interfaces:**
- Consumes: `s6::{SvcStat, SVSTAT_BATCH_CMD, parse_svstat_batch}` (Task 2), `AppRecord.source` (Task 1)
- Produces: `async fn svc_stats(service: &str) -> Result<Vec<crate::s6::SvcStat>, String>` (private helper near `container_exec`, mcp.rs:~201)

- [ ] **Step 1: Add the svc_stats helper**

Next to `container_exec` (~line 208):

```rust
/// Read per-service status from an s6-based container. Err if the container
/// isn't s6-based (no /command/s6-svstat or /run/service).
async fn svc_stats(service: &str) -> Result<Vec<crate::s6::SvcStat>, String> {
    let out = container_exec(service, &["bash", "-c", crate::s6::SVSTAT_BATCH_CMD]).await?;
    if out.starts_with("[exited") {
        return Err(format!("s6 status read failed: {}", out));
    }
    Ok(crate::s6::parse_svstat_batch(&out))
}
```

- [ ] **Step 2: Rewrite get_app_status**

Replace the Launchy-status-file body with:

```rust
        let conn = db::open().map_err(|e| tool_err(e.to_string()))?;
        db::init(&conn).map_err(|e| tool_err(e.to_string()))?;
        let apps = db::list_apps(&conn).map_err(|e| tool_err(e.to_string()))?;

        let stats = svc_stats("apps")
            .await
            .map_err(|e| tool_err(format!("failed to read s6 service status: {}", e)))?;

        let services: Vec<_> = stats
            .iter()
            .map(|st| {
                let rec = apps.iter().find(|a| a.name == st.name);
                serde_json::json!({
                    "name": st.name,
                    "pid": st.pid,
                    "status": if st.up { "running" } else { "down" },
                    "uptime_secs": st.uptime_secs,
                    "source": rec.map(|a| a.source.as_str()).unwrap_or("build"),
                    "subdomain": rec.map(|a| a.subdomain.as_str()),
                    "internal_port": rec.map(|a| a.internal_port),
                })
            })
            .collect();

        Ok(Json(serde_json::json!({
            "services": services,
            "guidance": {
                "to_restart": "restart_app name='<name>'",
                "to_read_logs": "Service output goes to container stdout — use get_container_info service='apps'",
            }
        })))
```

- [ ] **Step 3: Rewrite get_supervisor_status**

Replace the apps/supervisorctl branch with s6-first + fallback:

```rust
        match svc_stats(&service).await {
            Ok(stats) => Ok(Json(serde_json::json!({
                "service": service,
                "supervisor": "s6-overlay",
                "services": stats,
            }))),
            Err(_) => {
                // Not an s6 container (e.g. playwright, or sandbox pre-migration).
                let output = container_exec(&service, &["supervisorctl", "status"])
                    .await
                    .map_err(|e| tool_err(format!("failed to get supervisor status: {}", e)))?;
                Ok(Json(serde_json::json!({
                    "service": service,
                    "supervisor": "supervisord-or-none",
                    "raw": output,
                })))
            }
        }
```

- [ ] **Step 4: Compile check + full test run**

```bash
ssh gem@apps 'cd /home/gem/projects/Codery/system/orchestrator && cargo test 2>&1 | tail -6'
```
Expected: clean compile, all tests pass.

- [ ] **Step 5: Commit**

```bash
git add system/orchestrator/src/mcp.rs
git commit -m "codery-ci: get_app_status/get_supervisor_status via s6-svstat (drop restart_count)"
```

---

### Task 7: `main.rs` — `set-app-source` subcommand

**Files:**
- Modify: `system/orchestrator/src/main.rs` (dispatch ~110, usage text)

**Interfaces:**
- Consumes: `db::set_app_source`, `db::sync_s6` (Task 1)
- Produces: CLI `codery-ci set-app-source <name> <build|runtime>` (used in Task 11 deploy steps; exit 2 on bad usage, 1 on unknown app)

- [ ] **Step 1: Add the subcommand**

In the dispatch chain (after the `reload-routes` arm, ~line 117):

```rust
        Some("set-app-source") => {
            // codery-ci set-app-source <name> <build|runtime>
            // 'build' rows are routing-only: sync_s6 renders no bundle and any
            // existing bundle is pruned. Process state is never touched here.
            let name = args.get(2).cloned().unwrap_or_default();
            let source = args.get(3).cloned().unwrap_or_default();
            if name.is_empty() || !matches!(source.as_str(), "build" | "runtime") {
                eprintln!("usage: codery-ci set-app-source <name> <build|runtime>");
                std::process::exit(2);
            }
            let conn = db::open()?;
            db::init(&conn)?;
            if db::set_app_source(&conn, &name, &source)? {
                db::sync_s6(&conn)?;
                println!("[db] app '{}' source set to '{}' (bundles re-rendered)", name, source);
            } else {
                eprintln!("app '{}' not found", name);
                std::process::exit(1);
            }
        }
```

Also add a line to the usage/help text: `  set-app-source <name> <build|runtime>   Mark an app image-baked (routing-only) or orchestrator-managed`.

- [ ] **Step 2: Compile check**

```bash
ssh gem@apps 'cd /home/gem/projects/Codery/system/orchestrator && cargo check 2>&1 | tail -3'
```
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add system/orchestrator/src/main.rs
git commit -m "codery-ci: set-app-source subcommand (build/runtime marking)"
```

---

### Task 8: `mcp.rs` — INSTRUCTIONS + guidance strings

**Files:**
- Modify: `system/orchestrator/src/mcp.rs` (param doc:109, guidance strings:602,1134,1152,1290,1391, INSTRUCTIONS:1490-1675)

**Interfaces:** none (docs-only change inside the binary).

- [ ] **Step 1: Targeted string replacements**

| Old | New |
|---|---|
| `Unique app name — used as Launchy service name and config filename` | `Unique app name — used as s6 service name and bundle directory name` |
| Every `For app logs: read_container_file service='apps' path='/var/log/launchy/{name}.log'` (and variants with the app name interpolated) | `App logs go to container stdout — use get_container_info service='apps'` |

In `INSTRUCTIONS`, replace these sections (keep everything else):

- The "Process management" block ("Both containers use **Launchy** (Rust binary) as PID 1 ... Writes status to /run/launchy-status.json (read by MCP tools)") becomes:

```text
### Process management

Both containers use **s6-overlay** as PID 1 (s6-svscan + s6-rc):
- Build-time services are s6-rc bundles baked into the image
  (/etc/s6-overlay/s6-rc.d, started via the user bundle)
- Runtime apps are s6 bundles in /etc/s6-overlay/apps.d (host
  /opt/codery/apps-s6.d), linked into /run/service via s6-svlink
- Status: s6-svstat; control: s6-svc; logs: container stdout (docker logs)
- A boot-time oneshot (runtime-apps) re-links runtime bundles after
  container restarts and blue/green redeploys
```

- The app-management tool table rows and workflow sections: replace mentions of "writes Launchy config / hot-reload on SIGHUP / status file" with "renders s6 bundle + s6-svlink / s6-svunlink / s6-svc -t / s6-svstat" (match the surrounding terse table style; one line per tool, mirroring the actual behavior implemented in Tasks 3-6).

- The persistence paragraph ("Runtime apps persist across container restarts ... Launchy reads `include_dirs` on startup") becomes:

```text
**Runtime apps persist across container restarts and blue/green redeploys.**
SQLite (/opt/codery/codery.db) is the source of truth; sync_s6 renders bundles
to /opt/codery/apps-s6.d on every mutation; the runtime-apps oneshot links
them into supervision at boot. Image-baked apps (source='build') are routed
but never process-managed by the orchestrator.
```

- The diagnostic workflow step 2 (`read_container_file service='apps' path='/var/log/launchy/{name}.log'`) becomes: `get_container_info service='apps'` → crash reason in container stdout tail; after fixing, `restart_app name='{name}'`.

Add one caveat line to the architecture section (removed opportunistically in Phase 3): `Note: the sandbox container is migrating from Launchy to s6-overlay; until Phase 2 lands, get_supervisor_status service='sandbox' falls back to supervisord (which errors) — treat sandbox process status as unavailable.`

- [ ] **Step 2: Compile check + full tests**

```bash
ssh gem@apps 'cd /home/gem/projects/Codery/system/orchestrator && cargo test 2>&1 | tail -4'
```

- [ ] **Step 3: Sweep for leftovers**

```bash
rg -in 'launchy' /home/gem/projects/Codery/system/orchestrator/src/
```
Expected: zero hits (or only historical comments intentionally kept — there should be none).

- [ ] **Step 4: Commit**

```bash
git add system/orchestrator/src/mcp.rs
git commit -m "codery-ci: MCP instructions + guidance strings for s6 supervision model"
```

---

### Task 9: apps image — `runtime-apps` restore oneshot

**Files:**
- Create: `containers/apps/s6-overlay/s6-rc.d/runtime-apps/type` (content: `oneshot`)
- Create: `containers/apps/s6-overlay/s6-rc.d/runtime-apps/up`
- Create: `containers/apps/s6-overlay/s6-rc.d/runtime-apps/dependencies.d/base` (empty)
- Create: `containers/apps/s6-overlay/user-bundles.d/user/contents.d/runtime-apps` (empty)
- Create: `containers/apps/scripts/runtime-apps-restore.sh`
- Modify: `containers/apps/Dockerfile` (script COPY + chmod, near line 220-226)
- Modify: `containers/apps/README-NIX.md:133-143` (runtime-app section)

**Interfaces:**
- Consumes: bundles rendered by `sync_s6` into `/etc/s6-overlay/apps.d/` (Task 1)
- Produces: at every apps-container boot, each bundle in `/etc/s6-overlay/apps.d/` is linked into `/run/service` (restores Launchy's startup `include_dirs` scan semantics)

- [ ] **Step 1: Create the restore script** `containers/apps/scripts/runtime-apps-restore.sh`:

```bash
#!/bin/bash
# runtime-apps-restore — link orchestrator-managed runtime apps into s6
# supervision after container (re)start or blue/green redeploy.
#
# Bundles live in /etc/s6-overlay/apps.d (host /opt/codery/apps-s6.d), rendered
# by codery-ci's sync_s6 from SQLite. /run/service is ephemeral, so links are
# recreated here at every boot. One bad bundle must not block the others.
shopt -s nullglob
found=0
for d in /etc/s6-overlay/apps.d/*/; do
  found=1
  if /command/s6-svlink /run/service "$d"; then
    echo "[runtime-apps] linked ${d}"
  else
    echo "[runtime-apps] WARN: failed to link ${d}" >&2
  fi
done
[ "$found" = "0" ] && echo "[runtime-apps] no runtime apps to restore"
exit 0
```

- [ ] **Step 2: Create the oneshot bundle**

`containers/apps/s6-overlay/s6-rc.d/runtime-apps/type`:
```text
oneshot
```

`containers/apps/s6-overlay/s6-rc.d/runtime-apps/up` (execline, calls the script — same pattern as `ssh-agent-keys/up`):
```text
#!/command/execlineb -P
/usr/local/bin/runtime-apps-restore
```

Empty files: `containers/apps/s6-overlay/s6-rc.d/runtime-apps/dependencies.d/base` and `containers/apps/s6-overlay/user-bundles.d/user/contents.d/runtime-apps` (`touch` both).

- [ ] **Step 3: Wire into the Dockerfile**

In `containers/apps/Dockerfile` near lines 220-226 (the script COPY block), add:

```dockerfile
COPY containers/apps/scripts/runtime-apps-restore.sh /usr/local/bin/runtime-apps-restore
```

and extend the following `RUN chmod +x ...` to include `/usr/local/bin/runtime-apps-restore`.

- [ ] **Step 4: Update README-NIX.md**

Replace the "Runtime app management via s6" section (lines 133-143) with:

```markdown
## Runtime app management via s6

Fully wired through the codery-ci MCP tools — no manual steps needed:

- `add_app` renders an s6 bundle to `/etc/s6-overlay/apps.d/<name>/` (host
  `/opt/codery/apps-s6.d`, generated from SQLite) and links it via
  `s6-svlink /run/service`
- `remove_app` runs `s6-svunlink` and prunes the bundle
- `restart_app` runs `s6-svc -t` (SIGTERM + respawn, escalating to `-k`)
- Status comes from `s6-svstat -o up,pid,updownfor`

**Persistence:** the `runtime-apps` oneshot (in the user bundle) re-links every
bundle in `/etc/s6-overlay/apps.d/` at boot, so runtime apps survive container
restarts and blue/green redeploys. Image-baked apps (cartaclient, cbe1, design)
are marked `source='build'` in SQLite — routed, but never process-managed by
the orchestrator.

Manual equivalent (debugging only): `s6-svlink /run/service /etc/s6-overlay/apps.d/<name>`
to start, `s6-svunlink /run/service <name>` to stop.
```

- [ ] **Step 5: Sanity-check the script**

```bash
bash -n containers/apps/scripts/runtime-apps-restore.sh && echo SYNTAX-OK
```
Expected: `SYNTAX-OK`.

- [ ] **Step 6: Commit**

```bash
git add containers/apps/s6-overlay/s6-rc.d/runtime-apps containers/apps/s6-overlay/user-bundles.d/user/contents.d/runtime-apps containers/apps/scripts/runtime-apps-restore.sh containers/apps/Dockerfile containers/apps/README-NIX.md
git commit -m "apps: runtime-apps oneshot — restore MCP-managed apps at boot"
```

---

### Task 10: Phase 1 — release, deploy, mark sources, e2e verify

**Files:** none (operations only).

**Interfaces:**
- Consumes: Tasks 1-9 (all merged on master)
- Produces: live codery-ci with s6 support; apps image with restore oneshot; 3 baked apps marked `source='build'`

- [ ] **Step 1: Full test suite green on master**

```bash
ssh gem@apps 'cd /home/gem/projects/Codery/system/orchestrator && cargo test 2>&1 | tail -4'
```

- [ ] **Step 2: Push + cut the codery-ci release**

```bash
github-push master
git tag -l 'codery-ci-v*' --sort=-v:refname | head -3
```

Bump `version` in `system/orchestrator/Cargo.toml` to the next minor (features added — e.g. 0.12.0 → 0.13.0; if Cargo.toml is already ahead of the latest tag, reuse its version), then:

```bash
git add system/orchestrator/Cargo.toml
git commit -m "codery-ci: bump to vX.Y.Z"
git tag codery-ci-vX.Y.Z
github-push master && github-push codery-ci-vX.Y.Z
```

Wait for the release build:

```bash
gh run list --repo CoderyOSS/Codery --workflow release-orchestrator.yml --limit 1
gh run watch <RUN_ID> --repo CoderyOSS/Codery --exit-status
```

- [ ] **Step 3: Deploy codery-ci to the VPS**

`build-orchestrator.yml` downloads `releases/latest` — verify that IS the new tag first:

```bash
gh api repos/CoderyOSS/Codery/releases/latest --jq .tag_name   # must print codery-ci-vX.Y.Z
gh workflow run build-orchestrator.yml --repo CoderyOSS/Codery
sleep 5 && gh run list --repo CoderyOSS/Codery --workflow build-orchestrator.yml --limit 1
gh run watch <RUN_ID> --repo CoderyOSS/Codery --exit-status
```

Verify via MCP: `get_status` / `run_preflight` respond (daemon restarted with the new binary).

- [ ] **Step 4 (USER, host shell): mark the 3 baked apps — BEFORE the new apps image cuts over**

```bash
codery-ci set-app-source cartaclient build
codery-ci set-app-source cbe1 build
codery-ci set-app-source design build
rm -rf /opt/codery/apps-launchy.d   # inert Launchy-era leftover
```

(Each prints `[db] app '<name>' source set to 'build' (bundles re-rendered)`.)

- [ ] **Step 5: Build + preview the apps image (restore oneshot)**

Via codery MCP: confirm `mcp_exec_enabled`, then:

```text
codery_exec ["build", "apps", "s6-restore-oneshot"]
codery_exec_status <job_id>   # poll until done (timeout_secs: 1800)
codery_exec ["deploy-preview", "apps", "s6-restore-oneshot"]
```

Verify (USER on host, or agent via `codery_exec` logs). The preview runs on the **inactive** color — substitute `green`/`blue` accordingly (`get_status` shows the active one):
```bash
docker logs codery-apps-green 2>&1 | grep -i "runtime-apps"   # oneshot ran
docker exec codery-apps-green ls /etc/s6-overlay/s6-rc.d/runtime-apps
```

Then ask the user: `codery-ci cutover apps`.

- [ ] **Step 6: E2E probe via MCP tools**

```text
add_app name='probe' subdomain='probe' internal_port=8099 command='python3 -m http.server 8099' directory='/tmp'
```
Expected: success (svstat reports up). Then:

```bash
curl -s -o /dev/null -w "%{http_code}\n" --max-time 10 http://apps:8099/   # docker network alias, resolves from the sandbox
```
Expected: `200`. Then MCP: `get_app_status` (probe listed, `source: "runtime"`), `restart_app name='probe'` (new pid returned), `remove_app name='probe'` (clean). Also confirm the 3 baked apps show `source: "build"` in `get_app_status`.

- [ ] **Step 7: Persistence probe (restore oneshot)**

```text
add_app name='probe2' subdomain='probe2' internal_port=8098 command='python3 -m http.server 8098' directory='/tmp'
restart_service service='apps'        # MCP — recreates the active container
get_app_status                        # probe2 must be back, running
remove_app name='probe2'
```
Expected: probe2 survives the container recreate (oneshot re-linked it) and removes cleanly.

- [ ] **Step 8: Commit any fixes discovered during e2e; update this plan's checkboxes**

---

# PHASE 2 — Sandbox image on s6-overlay

Outcome: sandbox boots via `/init`; the 6 services run as s6-rc bundles; Launchy is no longer referenced by any build artifact.

### Task 11: sandbox s6-overlay tree + env-import helper

**Files:**
- Create: `containers/sandbox/s6-overlay/s6-rc.d/{sshd,opencode,opencode-diff-pruner,opencode-serve-guard,tmux,opendesign}/{type,run,timeout-kill,dependencies.d/base}`
- Create: `containers/sandbox/s6-overlay/user-bundles.d/user/contents.d/{sshd,opencode,opencode-diff-pruner,opencode-serve-guard,tmux,opendesign}` (empty)
- Create: `containers/sandbox/scripts/s6-import-container-env.sh`

**Interfaces:**
- Consumes: `/run/s6/container_environment` (s6-overlay captures docker env at boot)
- Produces (used by Task 12 opendesign + Task 14 build): run scripts assume `/usr/local/bin/s6-import-container-env` exists; opendesign's run script sources `/run/env/opendesign.env` (written by Task 12's `15-render-domain.sh`)

- [ ] **Step 1: Create the env-import helper** `containers/sandbox/scripts/s6-import-container-env.sh`:

```bash
#!/bin/bash
# s6-import-container-env — source from s6 run scripts.
#
# s6 services get a minimal env (Launchy children inherited the full container
# env — this restores parity). s6-overlay captures docker env into
# /run/s6/container_environment at boot; import every key here, except those
# listed in $S6_IMPORT_SKIP (space-separated) so the sourcing script can apply
# its own overrides afterwards (e.g. opendesign's NODE_OPTIONS).
_ce=/run/s6/container_environment
if [ -d "$_ce" ]; then
  _skip=" ${S6_IMPORT_SKIP:-} "
  for _f in "$_ce"/*; do
    _k="$(basename "$_f")"
    case "$_skip" in
      *" $_k "*) continue ;;
    esac
    export "$_k=$(cat "$_f")"
  done
  unset _skip _f _k
fi
unset _ce
```

- [ ] **Step 2: Create the 6 bundles**

For every service: `type` = `longrun`, `timeout-kill` = `10000`, `dependencies.d/base` = empty. Run scripts:

`containers/sandbox/s6-overlay/s6-rc.d/sshd/run`:
```bash
#!/bin/bash
exec /usr/sbin/sshd -D -e
```

`containers/sandbox/s6-overlay/s6-rc.d/opencode/run`:
```bash
#!/bin/bash
. /usr/local/bin/s6-import-container-env
export HOME=/home/gem
cd /home/gem/projects
exec /command/s6-setuidgid gem opencode serve --hostname 0.0.0.0 --port 3000
```

`containers/sandbox/s6-overlay/s6-rc.d/opencode-diff-pruner/run`:
```bash
#!/bin/bash
. /usr/local/bin/s6-import-container-env
export HOME=/home/gem
exec /command/s6-setuidgid gem /usr/local/bin/prune-opencode-diffs
```

`containers/sandbox/s6-overlay/s6-rc.d/opencode-serve-guard/run`:
```bash
#!/bin/bash
. /usr/local/bin/s6-import-container-env
export HOME=/home/gem
exec /command/s6-setuidgid gem /usr/local/bin/opencode-serve-guard
```

`containers/sandbox/s6-overlay/s6-rc.d/tmux/run`:
```bash
#!/bin/bash
. /usr/local/bin/s6-import-container-env
export HOME=/home/gem
cd /home/gem/projects
exec /command/s6-setuidgid gem bash -c 'tmux new-session -d -s main 2>/dev/null || true; exec sleep infinity'
```

`containers/sandbox/s6-overlay/s6-rc.d/opendesign/run`:
```bash
#!/bin/bash
export S6_IMPORT_SKIP="NODE_ENV NODE_OPTIONS OD_BIND_HOST OD_PORT OD_DISABLE_API_AUTH OD_ALLOWED_ORIGINS"
. /usr/local/bin/s6-import-container-env
. /run/env/opendesign.env
export HOME=/home/gem
cd /home/gem/open-design
exec /command/s6-setuidgid gem node /home/gem/open-design/apps/daemon/dist/cli.js --no-open
```

- [ ] **Step 3: Create the user-bundle entries**

```bash
mkdir -p containers/sandbox/s6-overlay/user-bundles.d/user/contents.d
for s in sshd opencode opencode-diff-pruner opencode-serve-guard tmux opendesign; do
  touch "containers/sandbox/s6-overlay/user-bundles.d/user/contents.d/$s"
done
```

- [ ] **Step 4: Sanity-check**

```bash
for f in containers/sandbox/s6-overlay/s6-rc.d/*/run containers/sandbox/scripts/s6-import-container-env.sh; do bash -n "$f" || echo "SYNTAX FAIL: $f"; done; echo CHECKED
```
Expected: `CHECKED` with no FAIL lines.

- [ ] **Step 5: Commit**

```bash
git add containers/sandbox/s6-overlay containers/sandbox/scripts/s6-import-container-env.sh
git commit -m "sandbox: s6-rc bundles for the 6 services + container-env importer"
```

---

### Task 12: `docker-entrypoint.d` → `cont-init.d`

**Files:**
- Rename: `containers/sandbox/docker-entrypoint.d/` → `containers/sandbox/cont-init.d/`
- Modify: every script's shebang → `#!/command/with-contenv bash` (restores full docker-env access; s6 otherwise gives init scripts a minimal env)
- Rewrite: `containers/sandbox/cont-init.d/15-render-domain.sh`
- Modify: `containers/sandbox/cont-init.d/40-start-sshd.sh` (comment + `/run/sshd`)

**Interfaces:**
- Produces: `/run/env/opendesign.env` consumed by Task 11's opendesign run script

- [ ] **Step 1: Rename the directory**

```bash
git mv containers/sandbox/docker-entrypoint.d containers/sandbox/cont-init.d
```

- [ ] **Step 2: Update shebangs in all scripts**

In every `containers/sandbox/cont-init.d/*.sh`: replace `#!/bin/bash` (first line) with `#!/command/with-contenv bash`. (with-contenv re-injects the captured container env — required by `20-github-auth.sh` (GITHUB_APP_*), `25-openrouter-auth.sh`, `35-openwiki-auth.sh` (ZAI_API_KEY), `15-render-domain.sh` (DOMAIN_NAME), `60-claude-mcp.sh` (ANTHROPIC_API_KEY).)

- [ ] **Step 3: Rewrite `15-render-domain.sh`**

```bash
#!/command/with-contenv bash
set -e
DOMAIN="${DOMAIN_NAME:-example.com}"
echo "[render-domain] writing /run/env/opendesign.env (domain: ${DOMAIN})"
mkdir -p /run/env
cat > /run/env/opendesign.env <<EOF
NODE_ENV=production
NODE_OPTIONS=--max-old-space-size=192
OD_BIND_HOST=0.0.0.0
OD_PORT=7456
OD_DISABLE_API_AUTH=1
OD_ALLOWED_ORIGINS=https://opendesign.${DOMAIN}
EOF
```

- [ ] **Step 4: Update `40-start-sshd.sh`**

Replace the comment `sshd is managed by launchy (devcontainer.json) — not started here` with `sshd runs as an s6-rc longrun (containers/sandbox/s6-overlay/s6-rc.d/sshd) — this script only prepares keys`. At the end of the script add:

```bash
# sshd privilege-separation runtime dir (s6-overlay symlinks /var/run -> /run)
mkdir -p /run/sshd
```

- [ ] **Step 5: Fix other launchy references in the dir**

```bash
rg -in 'launchy|devcontainer' containers/sandbox/cont-init.d/
```
Update any remaining comment references to say s6-overlay (context: `40-start-sshd.sh` header comment mentions the old flow).

- [ ] **Step 6: Sanity-check**

```bash
for f in containers/sandbox/cont-init.d/*.sh; do bash -n "$f" || echo "SYNTAX FAIL: $f"; done; echo CHECKED
```
Expected: `CHECKED` with no FAIL lines.

- [ ] **Step 7: Commit**

```bash
git add containers/sandbox/cont-init.d containers/sandbox/docker-entrypoint.d
git commit -m "sandbox: docker-entrypoint.d -> cont-init.d (with-contenv shebangs, env-file domain render)"
```

---

### Task 13: `configuration.nix` — drop Launchy, add s6 tree

**Files:**
- Modify: `containers/sandbox/nixos/configuration.nix` (header:5-6, launchy.json:132-134, entrypoint copies:164-172, /var/run:72)
- Modify: `containers/sandbox/nixos/flake.nix:6` (comment)

**Interfaces:**
- Consumes: `containers/sandbox/s6-overlay/` (Task 11), `containers/sandbox/cont-init.d/` (Task 12)

- [ ] **Step 1: Edits**

- Header comment (line 5-6): replace `entrypoint, launchy,` with `cont-init hooks, s6-overlay service dirs,`.
- `/var/run` (line 72): replace `mkdir -p $out/var/run/sshd` with `mkdir -p $out/var && ln -sfn /run $out/var/run` (s6-overlay requires /var/run to be a symlink to /run; the `mkdir` is required because `$out/var` does not exist yet at this point in the runCommand — `var/empty` is created on the next line. `/run/sshd` itself is created at boot by cont-init `40-start-sshd.sh`).
- Delete the launchy.json block (lines 132-134: `# Launchy service definitions …` + `cp ${repo}/.devcontainer/devcontainer.json $out/etc/launchy.json`).
- Delete the entrypoint + launchy binary copies (lines 164-172: the `docker-entrypoint.d` cp, `entrypoint.sh` cp, and `bin/launchy` cp with their comments).
- Insert in their place:

```nix
    # s6-overlay cont-init hooks (run once at boot, before services)
    cp -r ${repo}/containers/sandbox/cont-init.d $out/etc/cont-init.d
    chmod +x $out/etc/cont-init.d/*.sh

    # s6-rc service definitions + user bundle (compiled by s6-overlay at boot)
    cp -r ${repo}/containers/sandbox/s6-overlay $out/etc/s6-overlay
    chmod +x $out/etc/s6-overlay/s6-rc.d/*/run
```

- In the helper-scripts block (line 174-182), add the env importer to the copies and the chmod:

```nix
    cp ${repo}/containers/sandbox/scripts/s6-import-container-env.sh $out/usr/local/bin/s6-import-container-env
```
(add `/usr/local/bin/s6-import-container-env` to the `chmod +x` list)

- `flake.nix` line 6 comment: `Repo content (scripts, configs, launchy binary, SSH keys)` → `Repo content (scripts, configs, s6-overlay service dirs, SSH keys)`.

- [ ] **Step 2: Sweep the file for leftovers**

```bash
rg -in 'launchy|devcontainer|entrypoint' containers/sandbox/nixos/
```
Expected: zero hits.

- [ ] **Step 3: Commit**

```bash
git add containers/sandbox/nixos/configuration.nix containers/sandbox/nixos/flake.nix
git commit -m "sandbox: nix rootfs carries s6-overlay tree + cont-init, drops launchy/entrypoint"
```

---

### Task 14: `examples/Dockerfile.sandbox` — s6-overlay install + `/init`

**Files:**
- Modify: `examples/Dockerfile.sandbox` (new stage after od-build:9, runtime stage:146-176)

**Interfaces:**
- Consumes: Tasks 11-13 (all rootfs content in place)
- Pins: `S6_OVERLAY_VERSION=3.2.3.2` (must equal `containers/apps/Dockerfile` ARG)

- [ ] **Step 1: Add the s6 download stage**

After the `od-build` stage (after line 21), insert:

```dockerfile
# --- s6-overlay download stage (bookworm has tar+xz; the nix-builder may not) ---
FROM docker.io/library/node:24-bookworm-slim AS s6-dl
ARG S6_OVERLAY_VERSION=3.2.3.2
ADD https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-noarch.tar.xz /tmp/
ADD https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-x86_64.tar.xz /tmp/
RUN mkdir -p /s6 \
 && tar -C /s6 -Jxpf /tmp/s6-overlay-noarch.tar.xz \
 && tar -C /s6 -Jxpf /tmp/s6-overlay-x86_64.tar.xz
```

- [ ] **Step 2: Runtime stage — copy s6 in, flip entrypoint, set S6 env**

After `COPY --from=nix-builder /export/cargo /usr/local/cargo` (line 152), insert:

```dockerfile
# s6-overlay (PID 1 + supervision binaries, static)
COPY --from=s6-dl /s6 /
```

Replace `ENTRYPOINT ["/entrypoint.sh"]` (line 176) with:

```dockerfile
ENTRYPOINT ["/init"]
```

Extend the runtime `ENV` block (lines 165-173) with:

```dockerfile
    S6_BEHAVIOUR_IF_STAGE2_FAILS=1 \
    S6_KILL_GRACETIME=250 \
    S6_KILL_FINISH_MAXTIME=1000
```

(`S6_BEHAVIOUR_IF_STAGE2_FAILS=1` = warn-and-continue on cont-init failure, matching the old entrypoint's `|| echo WARNING — continuing` semantics.)

- [ ] **Step 3: Commit**

```bash
git add examples/Dockerfile.sandbox
git commit -m "sandbox: install s6-overlay 3.2.3.2, ENTRYPOINT /init, S6 shutdown tuning"
```

---

### Task 15: Delete `.devcontainer/` + reference sweep

**Files:**
- Delete: `.devcontainer/devcontainer.json` (whole `.devcontainer/` dir)
- Modify: `.github/workflows/deploy-sandbox.yml` (relevant-paths comment mentioning devcontainer.json)

**Interfaces:** none.

- [ ] **Step 1: Verify nothing references it, then delete**

```bash
rg -rn 'devcontainer' --glob '!docs/**' --glob '!handoff.md' /home/gem/projects/Codery
```
Expected hits: `SETUP.md`, `AGENTS.md`, `containers/sandbox/agents_file`, `.github/workflows/deploy-sandbox.yml` (comment), `system/orchestrator/src/mcp.rs` (should have been cleaned in Task 8 — fix any straggler now). `configuration.nix`/`Dockerfile` must have zero hits (Tasks 13-14).

```bash
git rm -r .devcontainer
```

- [ ] **Step 2: Fix the workflow comment** in `deploy-sandbox.yml` — replace `.devcontainer/devcontainer.json` in the relevant-paths comment with `containers/sandbox/s6-overlay/**`, `containers/sandbox/cont-init.d/**`.

(SETUP.md / AGENTS.md / agents_file references are updated in Phase 3, Task 18 — leave for now.)

- [ ] **Step 3: Commit**

```bash
git add -A .devcontainer .github/workflows/deploy-sandbox.yml
git commit -m "sandbox: retire .devcontainer/devcontainer.json (was launchy's config source)"
```

---

### Task 16: Phase 2 — build, preview, verify, cutover

**Files:** none (operations only).

**Interfaces:**
- Consumes: Tasks 11-15 on master
- Produces: sandbox running s6-overlay; this agent session eventually restarts into the new image

- [ ] **Step 1: Push + local build**

```bash
github-push master
```

Via codery MCP: confirm `mcp_exec_enabled`, then `codery_exec ["build", "sandbox", "s6-migration"]` with `timeout_secs: 5400` (nix rootfs rebuild after configuration.nix change is heavy). Poll `codery_exec_status` every ~60s.

- [ ] **Step 2: Deploy preview**

```text
codery_exec ["deploy-preview", "sandbox", "s6-migration"]
```

- [ ] **Step 3: Verify the preview**

Agent-checkable:
```bash
curl -s -o /dev/null -w "%{http_code}\n" --max-time 15 https://sandbox-preview.<DOMAIN>/   # expect 200 (opencode UI)
```

USER on host (the preview runs on the **inactive** color — substitute `green`/`blue` accordingly; `get_status` shows the active one):
```bash
docker exec codery-sandbox-green ls /run/service/
# expect: opencode, opencode-diff-pruner, opencode-serve-guard, opendesign, sshd, tmux (+ s6 internals)
docker exec codery-sandbox-green /command/s6-svstat -o up,pid /run/service/opencode   # "true <pid>"
docker exec codery-sandbox-green tmux ls                                              # main session
docker exec codery-sandbox-green curl -s -o /dev/null -w "%{http_code}\n" http://127.0.0.1:7456/  # opendesign: 200
docker logs codery-sandbox-green 2>&1 | grep -E "render-domain|cont-init|stage 2|fail" | head -20 # clean boot, no stage2 failures
ssh -p 20022 gem@localhost true && echo SSH-OK                                        # preview-color sshd
```

Also verify auth inside the preview (github auth ran as cont-init): `docker exec codery-sandbox-green sudo -u gem gh auth status` (or check via the preview opencode UI once logged in).

- [ ] **Step 4 (USER, host shell): cutover**

```bash
codery-ci cutover sandbox
```

**This kills the current agent session** (standard for sandbox redeploys). Continue in the new session for the post-checks and Phase 3. Rollback if needed: `codery-ci rollback sandbox`.

- [ ] **Step 5: Post-cutover checks (new session)**

```bash
ls /run/service/                       # 6 services + s6 internals
/command/s6-svstat -o up,pid /run/service/opencode
tmux ls
gh auth status
rg -n 'launchy' /etc/launchy.json /sbin/launchy 2>&1   # both gone
```

---

# PHASE 3 — Delete Launchy + docs sweep

### Task 17: Delete Launchy artifacts

**Files:**
- Delete: `system/launchy/` (whole directory — Cargo.toml, Cargo.lock, src/, any target/)
- Delete: `containers/sandbox/bin/launchy` (862KB checked-in blob)
- Delete: `containers/sandbox/scripts/entrypoint.sh`
- Delete: `handoff.md` (both its issues are now resolved: Issue 1 was fixed in `6e90cb0`, Issue 2 by Phase 1)

**Interfaces:** none (image no longer references these since Task 13).

- [ ] **Step 1: Delete + sweep**

```bash
git rm -r system/launchy containers/sandbox/bin/launchy containers/sandbox/scripts/entrypoint.sh handoff.md
rg -in 'launchy' --glob '!docs/superpowers/**' --glob '!docs/plans/**' /home/gem/projects/Codery
```
Expected remaining hits: only the doc files fixed in Task 18.

- [ ] **Step 2: Commit**

```bash
git commit -m "remove Launchy entirely — source, binary blob, entrypoint, resolved handoff doc"
```

---

### Task 18: Docs sweep

**Files:**
- Modify: `AGENTS.md` (sandbox section ~57, sshd ~289, project structure ~546-555, add-app flow, MCP table)
- Modify: `containers/sandbox/agents_file` (MCP section ~213-222)
- Modify: `SETUP.md:189`
- Modify: `containers/apps/flake.nix:69` (stale comment)
- Modify: `containers/apps/scripts/healthcheck.sh:3` (stale comment)
- Modify: `containers/sandbox/scripts/opencode-serve-guard.sh:2` (comment)
- Modify: `containers/sandbox/scripts/git-credential-codery.sh:7` (comment)

**Interfaces:** none. Per repo policy, AGENTS.md files MUST be updated when structures they document change.

- [ ] **Step 1: AGENTS.md edits**

- Sandbox section: "then `launchy` drops to `gem` (uid 1000) for all processes" → "s6-overlay is PID 1; services drop to `gem` (uid 1000) via `s6-setuidgid` in their run scripts".
- SSH section: "sshd runs as a launchy-managed service (`devcontainer.json`, `user: "root"`, `restart: "always"`, `priority: 10`, flags `-D -e`)" → "sshd runs as an s6-rc longrun (`containers/sandbox/s6-overlay/s6-rc.d/sshd`, root, `-D -e`)".
- Project structure: remove `bin/launchy`, `scripts/entrypoint.sh` lines; rename `docker-entrypoint.d/` → `cont-init.d/`; add `s6-overlay/` (s6-rc bundles for the 6 services); remove `.devcontainer/devcontainer.json` from relevant-paths mentions; remove `system/launchy` if listed; update the `customizations.codery.apps` add-app flow to: runtime apps via MCP `add_app` (no rebuild), build-time apps via a bundle in `containers/apps/s6-overlay/s6-rc.d/` + a `source='build'` SQLite row (`codery-ci set-app-source`).
- MCP tool table: `get_app_status` — "(pid, uptime, build vs runtime) from s6-svstat + SQLite" (drop Launchy wording); `restart_app` — "s6-svc -t, Launchy respawns" → "s6-svc -t, s6-supervise respawns".

- [ ] **Step 2: agents_file edits** (`containers/sandbox/agents_file`)

- MCP section: rewrite `add_app`/`remove_app`/`restart_app`/`get_app_status` one-liners for s6 semantics (bundle render + svlink/svunlink/svc -t/svstat; logs via container stdout / `get_container_info`); "Launchy respawns it" → "s6-supervise respawns it"; "Launchy config" → "s6 bundle"; the persistence note → "restored at boot by the runtime-apps oneshot".

- [ ] **Step 3: Small-file edits**

- `SETUP.md:189`: "Look for launchy output showing which service is failing" → "Look at container stdout (`docker logs codery-sandbox-<color>`) and `docker exec <container> /command/s6-svstat /run/service/<name>` to find the failing service".
- `containers/apps/flake.nix:69`: "Infra used by entrypoint / launchy-managed services" → "Infra used by entrypoint / s6-managed services".
- `containers/apps/scripts/healthcheck.sh:3`: delete the stale `# Updated for Launchy migration.` line.
- `containers/sandbox/scripts/opencode-serve-guard.sh:2`: "SIGTERM when it exceeds threshold so launchy respawns" → "SIGTERM when it exceeds threshold so s6-supervise respawns".
- `containers/sandbox/scripts/git-credential-codery.sh:7`: "launchy children" → "s6 services".

- [ ] **Step 4: Final sweep**

```bash
rg -in 'launchy' /home/gem/projects/Codery --glob '!docs/superpowers/**' --glob '!docs/plans/**'
```
Expected: zero hits (historical design/plan docs under `docs/` are intentionally left as record).

- [ ] **Step 5: Commit + push**

```bash
git add -A
git commit -m "docs: Launchy -> s6-overlay sweep (AGENTS.md, agents_file, SETUP, comments)"
github-push master
```

- [ ] **Step 6 (optional, next codery-ci release): remove the Phase-1 caveat line from INSTRUCTIONS** ("sandbox container is migrating…") — fold into whatever release comes next; not worth a dedicated one.

---

## Self-Review Notes (completed by plan author)

- **Spec coverage:** Phase 1 (orchestrator + oneshot + release/deploy + set-app-source ordering) → Tasks 1-10. Phase 2 (sandbox image) → Tasks 11-16. Phase 3 (deletion + docs) → Tasks 17-18. API-equivalence rows each map to a task: bundles (T1), svlink/svunlink (T3/T4), svc -t (T5), svstat status (T2/T6), restart policies (T1 render_finish), user/directory/env (T1 render_run), timeout-kill 10s (T1), logs (T8), persistence layers 1-3 (T9/T10 + source column T1), priority (build-time dependencies — noted as no-runtime-equivalent in spec).
- **Ordering hazard addressed:** `set-app-source` (Task 10 Step 4) runs BEFORE the oneshot image cutover (Step 5) to prevent double-start of baked apps.
- **Type consistency:** `SvcStat` fields (`name/up/pid/uptime_secs`), `sync_s6`/`sync_s6_to_dir`/`set_app_source`/`shell_quote`/`svc_stats` signatures identical across tasks; `APPS_S6_DIR` defined Task 1, consumed Tasks 3-7 (via sync_s6) and Task 9 (bind target).
