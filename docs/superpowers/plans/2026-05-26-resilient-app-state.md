# Resilient App State: SQLite + Layered Routes — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace fragmented file-based app state (apps-routes.json, host-routes.json, sandbox-routes.json) with SQLite for runtime apps + unified routes.yaml for static routes, so that add_app/remove_app always stays in sync with Caddy, Nginx, and Launchy.

**Architecture:** Three-layer route resolution (service.yml → routes.yaml → SQLite). Every mutation to SQLite triggers a full cascade: regenerate Launchy JSONs, Caddyfile, and Nginx config. Static routes (host/sandbox extras) move to a single `proxy/routes.yaml` file that replaces two JSON files.

**Tech Stack:** Rust, rusqlite (bundled), serde_yaml, existing bollard/rmcp/axum stack.

---

## File Structure

### New files
- `system/orchestrator/src/db.rs` — SQLite CRUD, `sync_launchy()`, `build_route_map()`, `AppRecord`
- `proxy/routes.yaml` — unified static routes (replaces host-routes.json + sandbox-routes.json)

### Modified files
- `system/orchestrator/Cargo.toml` — add `rusqlite` crate
- `system/orchestrator/src/main.rs` — add `mod db`, call `db::init()` in startup paths
- `system/orchestrator/src/config.rs` — add `ROUTES_YAML` path, add `DB_PATH`, remove old JSON path constants
- `system/orchestrator/src/caddy.rs` — use `build_route_map()` instead of loading 3 JSON files; `AppRoute` gains `target` field; remove `load_host_routes()`, `load_routes_file()`
- `system/orchestrator/src/nginx.rs` — accept routes from unified map instead of reading `APPS_ROUTES`
- `system/orchestrator/src/mcp.rs` — `add_app`/`remove_app`/`list_apps` use `db::` functions; remove `apps_routes_upsert()`/`apps_routes_remove()`
- `.github/workflows/deploy-apps.yml` — remove gen-apps-routes.py steps, add routes.yaml sync
- `.github/workflows/deploy-sandbox.yml` — replace sandbox-routes.json sync with routes.yaml sync

### Deleted files
- `proxy/host-routes.json` — merged into routes.yaml
- `proxy/sandbox-routes.json` — merged into routes.yaml
- `proxy/apps-routes.json` — eliminated (SQLite owns runtime routes)
- `containers/apps/scripts/gen-apps-routes.py` — unused

---

## Task 1: Add rusqlite dependency

**Files:**
- Modify: `system/orchestrator/Cargo.toml`

- [ ] **Step 1: Add rusqlite with bundled feature to Cargo.toml**

In `system/orchestrator/Cargo.toml`, add after the `libc` line:

```toml
rusqlite = { version = "0.34", features = ["bundled"] }
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check --manifest-path system/orchestrator/Cargo.toml`
Expected: compiles with no errors

- [ ] **Step 3: Commit**

```
feat(codery-ci): add rusqlite dependency
```

---

## Task 2: Create db.rs module — schema, init, CRUD

**Files:**
- Create: `system/orchestrator/src/db.rs`
- Modify: `system/orchestrator/src/main.rs`
- Modify: `system/orchestrator/src/config.rs`

- [ ] **Step 1: Add DB_PATH constant to config.rs**

In `system/orchestrator/src/config.rs`, add after the `HOST_ROUTES` line:

```rust
pub const DB_PATH: &str = "/opt/codery/codery.db";
```

Also add the routes.yaml path constant:

```rust
pub const ROUTES_YAML: &str = "/opt/codery/proxy/routes.yaml";
```

- [ ] **Step 2: Create db.rs with schema, init, insert, delete, list**

Create `system/orchestrator/src/db.rs`:

```rust
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::config;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppRecord {
    pub name: String,
    pub subdomain: String,
    pub internal_port: u16,
    pub command: String,
    pub directory: String,
    pub env: Option<String>,
    pub priority: i64,
    pub user: String,
    pub restart: String,
    pub created_at: String,
}

pub fn open() -> Result<Connection> {
    let path = config::DB_PATH;
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent dir for {}", path))?;
    }
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open {}", path))?;
    Ok(conn)
}

pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS apps (
            name          TEXT PRIMARY KEY,
            subdomain     TEXT NOT NULL UNIQUE,
            internal_port INTEGER NOT NULL,
            command       TEXT NOT NULL,
            directory     TEXT NOT NULL,
            env           TEXT,
            priority      INTEGER NOT NULL DEFAULT 100,
            user          TEXT NOT NULL DEFAULT 'gem',
            restart       TEXT NOT NULL DEFAULT 'always',
            created_at    TEXT NOT NULL DEFAULT (datetime('now'))
        );"
    ).context("failed to create apps table")?;
    Ok(())
}

pub fn insert_app(conn: &Connection, app: &AppRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO apps (name, subdomain, internal_port, command, directory, env, priority, user, restart)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        (
            &app.name,
            &app.subdomain,
            app.internal_port as i64,
            &app.command,
            &app.directory,
            &app.env,
            app.priority,
            &app.user,
            &app.restart,
        ),
    ).with_context(|| format!("failed to insert app '{}'", app.name))?;
    Ok(())
}

pub fn delete_app(conn: &Connection, name: &str) -> Result<bool> {
    let rows = conn.execute("DELETE FROM apps WHERE name = ?1", [name])
        .with_context(|| format!("failed to delete app '{}'", name))?;
    Ok(rows > 0)
}

pub fn list_apps(conn: &Connection) -> Result<Vec<AppRecord>> {
    let mut stmt = conn.prepare(
        "SELECT name, subdomain, internal_port, command, directory, env, priority, user, restart, created_at
         FROM apps ORDER BY name"
    ).context("failed to prepare apps query")?;
    let rows = stmt.query_map([], |row| {
        Ok(AppRecord {
            name: row.get(0)?,
            subdomain: row.get(1)?,
            internal_port: row.get::<_, i64>(2)? as u16,
            command: row.get(3)?,
            directory: row.get(4)?,
            env: row.get(5)?,
            priority: row.get(6)?,
            user: row.get(7)?,
            restart: row.get(8)?,
            created_at: row.get(9)?,
        })
    }).context("failed to query apps")?;
    let mut apps = Vec::new();
    for app in rows {
        apps.push(app.context("failed to read app row")?);
    }
    Ok(apps)
}

pub fn find_app_by_name(conn: &Connection, name: &str) -> Result<Option<AppRecord>> {
    let apps = list_apps(conn)?;
    Ok(apps.into_iter().find(|a| a.name == name))
}

pub fn port_claimed(conn: &Connection, port: u16) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM apps WHERE internal_port = ?1",
        [port as i64],
        |row| row.get(0),
    ).context("failed to check port")?;
    Ok(count > 0)
}
```

- [ ] **Step 3: Register db module in main.rs**

In `system/orchestrator/src/main.rs`, add after `mod caddy;`:

```rust
mod db;
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check --manifest-path system/orchestrator/Cargo.toml`
Expected: compiles

- [ ] **Step 5: Commit**

```
feat(codery-ci): add db.rs module with SQLite schema and CRUD
```

---

## Task 3: Write tests for db.rs CRUD

**Files:**
- Modify: `system/orchestrator/src/db.rs`

- [ ] **Step 1: Add test module at the bottom of db.rs**

Append to `system/orchestrator/src/db.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        conn
    }

    fn sample_app(name: &str) -> AppRecord {
        AppRecord {
            name: name.to_string(),
            subdomain: name.to_string(),
            internal_port: 3001,
            command: "bun run start".to_string(),
            directory: format!("/home/gem/projects/{}", name),
            env: None,
            priority: 100,
            user: "gem".to_string(),
            restart: "always".to_string(),
            created_at: String::new(),
        }
    }

    #[test]
    fn init_creates_table() {
        let conn = test_conn();
        let apps = list_apps(&conn).unwrap();
        assert!(apps.is_empty());
    }

    #[test]
    fn insert_and_list() {
        let conn = test_conn();
        let app = sample_app("myapp");
        insert_app(&conn, &app).unwrap();
        let apps = list_apps(&conn).unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "myapp");
        assert_eq!(apps[0].subdomain, "myapp");
        assert_eq!(apps[0].internal_port, 3001);
    }

    #[test]
    fn delete_existing_app() {
        let conn = test_conn();
        insert_app(&conn, &sample_app("myapp")).unwrap();
        assert!(delete_app(&conn, "myapp").unwrap());
        assert!(list_apps(&conn).unwrap().is_empty());
    }

    #[test]
    fn delete_nonexistent_returns_false() {
        let conn = test_conn();
        assert!(!delete_app(&conn, "nope").unwrap());
    }

    #[test]
    fn find_by_name() {
        let conn = test_conn();
        insert_app(&conn, &sample_app("myapp")).unwrap();
        assert!(find_app_by_name(&conn, "myapp").unwrap().is_some());
        assert!(find_app_by_name(&conn, "nope").unwrap().is_none());
    }

    #[test]
    fn port_claimed_check() {
        let conn = test_conn();
        assert!(!port_claimed(&conn, 3001).unwrap());
        insert_app(&conn, &sample_app("myapp")).unwrap();
        assert!(port_claimed(&conn, 3001).unwrap());
    }

    #[test]
    fn duplicate_name_rejected() {
        let conn = test_conn();
        insert_app(&conn, &sample_app("myapp")).unwrap();
        assert!(insert_app(&conn, &sample_app("myapp")).is_err());
    }

    #[test]
    fn duplicate_subdomain_rejected() {
        let conn = test_conn();
        let mut app1 = sample_app("app1");
        app1.subdomain = "same".to_string();
        app1.internal_port = 3001;
        insert_app(&conn, &app1).unwrap();
        let mut app2 = sample_app("app2");
        app2.subdomain = "same".to_string();
        app2.internal_port = 3002;
        assert!(insert_app(&conn, &app2).is_err());
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test --manifest-path system/orchestrator/Cargo.toml -- db::tests`
Expected: all pass

- [ ] **Step 3: Commit**

```
test(codery-ci): add db.rs CRUD tests
```

---

## Task 4: Create unified routes.yaml

**Files:**
- Create: `proxy/routes.yaml`
- Delete: `proxy/host-routes.json`
- Delete: `proxy/sandbox-routes.json`
- Delete: `proxy/apps-routes.json`
- Delete: `containers/apps/scripts/gen-apps-routes.py`

- [ ] **Step 1: Create proxy/routes.yaml**

Write `proxy/routes.yaml`:

```yaml
# Static routes — host services and container extras.
# Layer 2: overrides service.yml by subdomain, overridden by SQLite runtime apps.
#
# target: "host"    → direct localhost:port (no color offset)
# target: "sandbox" → apply sandbox port_scheme offset
# target: "apps"    → apply apps port_scheme offset + Nginx internal routing

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

  - subdomain: cli
    port: 7681
    target: sandbox

  - subdomain: ops
    port: 3090
    target: sandbox
```

- [ ] **Step 2: Delete old files**

Delete: `proxy/host-routes.json`, `proxy/sandbox-routes.json`, `proxy/apps-routes.json`, `containers/apps/scripts/gen-apps-routes.py`

- [ ] **Step 3: Commit**

```
refactor(routes): create unified routes.yaml, delete legacy JSON route files
```

---

## Task 5: Add routes.yaml types and loader to db.rs

**Files:**
- Modify: `system/orchestrator/src/db.rs`
- Modify: `system/orchestrator/src/config.rs`

- [ ] **Step 1: Add routes.yaml types and loader to db.rs**

Append to `system/orchestrator/src/db.rs` (before the `#[cfg(test)]` block):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticRoute {
    pub subdomain: String,
    pub port: u16,
    pub target: String,
}

#[derive(Debug, Deserialize)]
struct RoutesFile {
    routes: Vec<StaticRoute>,
}

pub fn load_static_routes() -> Result<Vec<StaticRoute>> {
    let path = config::ROUTES_YAML;
    if !std::path::Path::new(path).exists() {
        return Ok(vec![]);
    }
    let data = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path))?;
    let file: RoutesFile = serde_yaml::from_str(&data)
        .with_context(|| format!("failed to parse {}", path))?;
    Ok(file.routes)
}

pub fn default_static_routes() -> Vec<StaticRoute> {
    vec![
        StaticRoute {
            subdomain: "mcp".to_string(),
            port: config::MCP_PORT,
            target: "host".to_string(),
        },
        StaticRoute {
            subdomain: "ci".to_string(),
            port: config::UI_PORT,
            target: "host".to_string(),
        },
    ]
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check --manifest-path system/orchestrator/Cargo.toml`
Expected: compiles

- [ ] **Step 3: Commit**

```
feat(codery-ci): add routes.yaml types and loader
```

---

## Task 6: Add build_route_map() and sync_launchy() to db.rs

**Files:**
- Modify: `system/orchestrator/src/db.rs`

- [ ] **Step 1: Add UnifiedRoute type**

Append to `system/orchestrator/src/db.rs` (before `#[cfg(test)]`):

```rust
#[derive(Debug, Clone)]
pub struct UnifiedRoute {
    pub subdomain: String,
    pub port: u16,
    pub target: String,
    pub internal_port: Option<u16>,
}

pub fn build_route_map(conn: &Connection) -> Result<Vec<UnifiedRoute>> {
    let mut map: std::collections::HashMap<String, UnifiedRoute> =
        std::collections::HashMap::new();

    let defs = crate::service_def::ServiceDef::load_all()?;
    for def in &defs {
        for port in &def.ports {
            if let Some(subdomain) = &port.subdomain {
                map.entry(subdomain.clone()).or_insert(UnifiedRoute {
                    subdomain: subdomain.clone(),
                    port: port.container_port,
                    target: def.service.clone(),
                    internal_port: None,
                });
            }
        }
    }

    let static_routes = load_static_routes().unwrap_or_else(|_| default_static_routes());
    for route in &static_routes {
        map.insert(route.subdomain.clone(), UnifiedRoute {
            subdomain: route.subdomain.clone(),
            port: route.port,
            target: route.target.clone(),
            internal_port: None,
        });
    }

    let apps = list_apps(conn)?;
    for app in &apps {
        map.insert(app.subdomain.clone(), UnifiedRoute {
            subdomain: app.subdomain.clone(),
            port: 8080,
            target: "apps".to_string(),
            internal_port: Some(app.internal_port),
        });
    }

    let mut routes: Vec<UnifiedRoute> = map.into_values().collect();
    routes.sort_by(|a, b| a.subdomain.cmp(&b.subdomain));
    Ok(routes)
}

pub fn sync_launchy(conn: &Connection) -> Result<()> {
    let apps = list_apps(conn)?;
    let dir = std::path::Path::new(config::APPS_LAUNCHY_DIR);
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create {}", config::APPS_LAUNCHY_DIR))?;

    if dir.exists() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if !apps.iter().any(|a| a.name == stem) {
                    std::fs::remove_file(&path)?;
                }
            }
        }
    }

    for app in &apps {
        let mut svc = serde_json::json!({
            "name": app.name,
            "command": ["bash", "-c", &app.command],
            "directory": app.directory,
            "user": app.user,
            "restart": app.restart,
            "priority": app.priority
        });
        if let Some(env_json) = &app.env {
            if let Ok(env_map) = serde_json::from_str::<std::collections::HashMap<String, String>>(env_json) {
                if !env_map.is_empty() {
                    svc.as_object_mut().unwrap().insert("env".to_string(), serde_json::json!(env_map));
                }
            }
        }
        let path = dir.join(format!("{}.json", app.name));
        let content = serde_json::to_string_pretty(&svc).unwrap() + "\n";
        std::fs::write(&path, &content)
            .with_context(|| format!("failed to write {:?}", path))?;
    }

    Ok(())
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check --manifest-path system/orchestrator/Cargo.toml`
Expected: compiles

- [ ] **Step 3: Commit**

```
feat(codery-ci): add build_route_map() and sync_launchy()
```

---

## Task 7: Rewrite caddy.rs to use unified route map

**Files:**
- Modify: `system/orchestrator/src/caddy.rs`

This is the biggest single change. The file stops reading JSON files directly and instead receives a `Vec<UnifiedRoute>` from `db::build_route_map()`. It still generates Caddyfile content and reloads Caddy.

- [ ] **Step 1: Replace the top of caddy.rs**

Replace the entire contents of `system/orchestrator/src/caddy.rs` with:

```rust
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::process::Command;

use crate::config;
use crate::service_def::ServiceDef;
use crate::state;
use crate::db::{self, UnifiedRoute};

// ── Public API ────────────────────────────────────────────────────────────────

pub fn apply_all() -> Result<()> {
    let defs = ServiceDef::load_all()?;
    let domain = config::load_domain();
    let colors: HashMap<String, String> = defs
        .iter()
        .map(|d| {
            let color = state::read_active(&d.service).unwrap_or_else(|_| "blue".to_string());
            (d.service.clone(), color)
        })
        .collect();

    let conn = db::open()?;
    let routes = db::build_route_map(&conn)?;

    let content = generate_from_routes(&routes, &colors, &domain)?;
    fs::write(config::CADDY_CONFIG, &content)
        .with_context(|| format!("failed to write {}", config::CADDY_CONFIG))?;
    reload_caddy()?;
    println!("[caddy] Caddyfile written and Caddy reloaded (apply_all)");
    Ok(())
}

// ── Generation ────────────────────────────────────────────────────────────────

pub fn generate_from_routes(
    routes: &[UnifiedRoute],
    colors: &HashMap<String, String>,
    domain: &str,
) -> Result<String> {
    let mut caddy = String::from(
        "{\n    acme_dns cloudflare {$CLOUDFLARE_API_TOKEN}\n}\n",
    );

    for route in routes {
        let fqdn = if route.subdomain.contains('.') {
            route.subdomain.clone()
        } else {
            format!("{}.{}", route.subdomain, domain)
        };

        let host_port = match route.target.as_str() {
            "host" => route.port,
            "sandbox" => {
                let color = colors.get("sandbox").map(|s| s.as_str()).unwrap_or("blue");
                sandbox_host_port(color, route.port)
            }
            target => {
                let color = colors.get(target).map(|s| s.as_str()).unwrap_or("blue");
                let def = ServiceDef::load(target).ok();
                if let Some(def) = def {
                    def.port_scheme.host_port(color, route.port)
                } else {
                    route.port
                }
            }
        };
        caddy.push_str(&caddy_block(&fqdn, host_port));
    }

    Ok(caddy)
}

fn sandbox_host_port(color: &str, container_port: u16) -> u16 {
    let offset: u16 = if color == "blue" { 10000 } else { 20000 };
    container_port + offset
}

fn caddy_block(host: &str, port: u16) -> String {
    format!(
        r#"
{host} {{
    bind {{$TAILSCALE_IP}}
    reverse_proxy localhost:{port}
}}
"#,
        host = host,
        port = port
    )
}

// ── Caddy process management ──────────────────────────────────────────────────

fn reload_caddy() -> Result<()> {
    let env_pairs = load_env_pairs();

    let out = Command::new("caddy")
        .args(["reload", "--config", config::CADDY_CONFIG, "--adapter", "caddyfile"])
        .envs(env_pairs)
        .output()
        .context("failed to run caddy reload")?;

    if !out.status.success() {
        anyhow::bail!(
            "caddy reload failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

fn load_env_pairs() -> Vec<(String, String)> {
    let content = match std::fs::read_to_string(config::ENV_FILE) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let mut pairs: Vec<(String, String)> = content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let pos = l.find('=')?;
            Some((l[..pos].to_string(), l[pos + 1..].to_string()))
        })
        .collect();

    if !pairs.iter().any(|(k, _)| k == "TAILSCALE_IP") {
        if let Ok(ip) = std::fs::read_to_string(config::TAILSCALE_IP_FILE) {
            let ip = ip.trim().to_string();
            if !ip.is_empty() {
                pairs.push(("TAILSCALE_IP".to_string(), ip));
            }
        }
    }

    pairs
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox_route(subdomain: &str, port: u16) -> UnifiedRoute {
        UnifiedRoute {
            subdomain: subdomain.to_string(),
            port,
            target: "sandbox".to_string(),
            internal_port: None,
        }
    }

    fn apps_route(subdomain: &str, internal_port: u16) -> UnifiedRoute {
        UnifiedRoute {
            subdomain: subdomain.to_string(),
            port: 8080,
            target: "apps".to_string(),
            internal_port: Some(internal_port),
        }
    }

    fn host_route(subdomain: &str, port: u16) -> UnifiedRoute {
        UnifiedRoute {
            subdomain: subdomain.to_string(),
            port,
            target: "host".to_string(),
            internal_port: None,
        }
    }

    fn colors(sandbox: &str, apps: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("sandbox".to_string(), sandbox.to_string());
        m.insert("apps".to_string(), apps.to_string());
        m
    }

    #[test]
    fn sandbox_blue_uses_10k_offset() {
        let routes = vec![
            sandbox_route("opencode", 3000),
            sandbox_route("cli", 7681),
        ];
        let caddy = generate_from_routes(&routes, &colors("blue", "blue"), "example.com").unwrap();
        assert!(caddy.contains("reverse_proxy localhost:13000"));
        assert!(caddy.contains("reverse_proxy localhost:17681"));
    }

    #[test]
    fn sandbox_green_uses_20k_offset() {
        let routes = vec![
            sandbox_route("opencode", 3000),
        ];
        let caddy = generate_from_routes(&routes, &colors("green", "blue"), "example.com").unwrap();
        assert!(caddy.contains("reverse_proxy localhost:23000"));
    }

    #[test]
    fn apps_green_uses_10k_offset() {
        let routes = vec![
            apps_route("myapp", 3001),
        ];
        let caddy = generate_from_routes(&routes, &colors("blue", "green"), "example.com").unwrap();
        assert!(caddy.contains("reverse_proxy localhost:18080"));
    }

    #[test]
    fn apps_blue_no_offset() {
        let routes = vec![
            apps_route("myapp", 3001),
        ];
        let caddy = generate_from_routes(&routes, &colors("blue", "blue"), "example.com").unwrap();
        assert!(caddy.contains("reverse_proxy localhost:8080"));
    }

    #[test]
    fn host_routes_no_offset() {
        let routes = vec![
            host_route("mcp", 4040),
        ];
        let caddy = generate_from_routes(&routes, &colors("blue", "blue"), "example.com").unwrap();
        assert!(caddy.contains("reverse_proxy localhost:4040"));
    }

    #[test]
    fn fqdn_subdomain_used_as_is() {
        let routes = vec![
            UnifiedRoute {
                subdomain: "myapp.custom.com".to_string(),
                port: 8080,
                target: "apps".to_string(),
                internal_port: Some(3001),
            },
        ];
        let caddy = generate_from_routes(&routes, &colors("blue", "blue"), "example.com").unwrap();
        assert!(caddy.contains("myapp.custom.com"));
        assert!(!caddy.contains("myapp.custom.com.example.com"));
    }
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check --manifest-path system/orchestrator/Cargo.toml`
Expected: compiles (may have warnings about unused imports in other modules — fix in next tasks)

- [ ] **Step 3: Run new caddy tests**

Run: `cargo test --manifest-path system/orchestrator/Cargo.toml -- caddy::tests`
Expected: all pass

- [ ] **Step 4: Commit**

```
refactor(codery-ci): rewrite caddy.rs to use unified route map
```

---

## Task 8: Rewrite nginx.rs to accept routes from unified map

**Files:**
- Modify: `system/orchestrator/src/nginx.rs`

- [ ] **Step 1: Replace nginx.rs**

Replace the entire contents of `system/orchestrator/src/nginx.rs` with:

```rust
use anyhow::{Context, Result};
use bollard::Docker;
use bollard::exec::{CreateExecOptions, StartExecResults};
use futures_util::StreamExt;
use std::fs;

use crate::db::{self, UnifiedRoute};
use crate::{config, state};

pub async fn generate_and_reload() -> Result<()> {
    let conn = db::open()?;
    let routes = db::build_route_map(&conn)?;
    let domain = config::load_domain();
    let content = generate_config(&routes, &domain);

    let to_write = if content.is_empty() {
        "server {\n    listen 8080 default_server;\n    return 503 \"No apps configured\";\n}\n".to_string()
    } else {
        content
    };

    if let Some(parent) = std::path::Path::new(config::NGINX_CONFIG).parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent dir for {}", config::NGINX_CONFIG))?;
    }
    fs::write(config::NGINX_CONFIG, &to_write)
        .with_context(|| format!("failed to write {}", config::NGINX_CONFIG))?;

    let count = routes.iter().filter(|r| r.internal_port.is_some()).count();
    println!("[nginx] Wrote config with {} server block(s)", count);

    reload_in_active_container().await
}

pub(crate) fn generate_config(routes: &[UnifiedRoute], domain: &str) -> String {
    let blocks: Vec<String> = routes
        .iter()
        .filter_map(|r| {
            let internal_port = r.internal_port?;
            if r.target != "apps" {
                return None;
            }
            let fqdn = if r.subdomain.contains('.') {
                r.subdomain.clone()
            } else {
                format!("{}.{}", r.subdomain, domain)
            };
            Some(format!(
                "server {{\n    listen 8080;\n    server_name {fqdn};\n    location / {{\n        proxy_pass http://127.0.0.1:{internal_port};\n        proxy_set_header Host $host;\n        proxy_set_header X-Real-IP $remote_addr;\n        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;\n    }}\n}}\n"
            ))
        })
        .collect();

    if blocks.is_empty() {
        return String::new();
    }

    let mut out = blocks.join("\n");
    out.push_str("\nserver {\n    listen 8080 default_server;\n    return 404;\n}\n");
    out
}

async fn reload_in_active_container() -> Result<()> {
    let color = match state::read_active("apps") {
        Ok(c) => c,
        Err(_) => {
            println!("[nginx] No active apps state — skipping reload");
            return Ok(());
        }
    };
    let container = config::container_name("apps", &color);

    let docker = Docker::connect_with_socket_defaults()
        .context("failed to connect to Docker")?;

    let running = docker.inspect_container(&container, None).await
        .ok()
        .and_then(|i| i.state)
        .and_then(|s| s.running)
        .unwrap_or(false);

    if !running {
        println!("[nginx] Container {} not running — skipping reload", container);
        return Ok(());
    }

    let exec = docker.create_exec(
        &container,
        CreateExecOptions {
            cmd: Some(vec!["nginx", "-s", "reload"]),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        },
    ).await.with_context(|| format!("failed to create exec in {}", container))?;

    if let StartExecResults::Attached { mut output, .. } = docker.start_exec(&exec.id, None).await? {
        while let Some(msg) = output.next().await {
            if let bollard::container::LogOutput::StdErr { message } = msg? {
                let text = String::from_utf8_lossy(&message);
                if !text.trim().is_empty() {
                    eprint!("[nginx] {}", text);
                }
            }
        }
    }

    println!("[nginx] Reloaded Nginx in {}", container);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apps_route(subdomain: &str, internal_port: Option<u16>) -> UnifiedRoute {
        UnifiedRoute {
            subdomain: subdomain.to_string(),
            port: 8080,
            target: "apps".to_string(),
            internal_port,
        }
    }

    fn host_route(subdomain: &str, port: u16) -> UnifiedRoute {
        UnifiedRoute {
            subdomain: subdomain.to_string(),
            port,
            target: "host".to_string(),
            internal_port: None,
        }
    }

    #[test]
    fn two_apps_produce_two_server_blocks_plus_default() {
        let routes = vec![
            apps_route("myapp", Some(3001)),
            apps_route("otherapp", Some(3002)),
        ];
        let cfg = generate_config(&routes, "example.com");
        assert!(cfg.contains("server_name myapp.example.com;"));
        assert!(cfg.contains("proxy_pass http://127.0.0.1:3001;"));
        assert!(cfg.contains("server_name otherapp.example.com;"));
        assert!(cfg.contains("proxy_pass http://127.0.0.1:3002;"));
        assert!(cfg.contains("listen 8080 default_server;"));
    }

    #[test]
    fn route_without_internal_port_is_skipped() {
        let routes = vec![apps_route("myapp", None)];
        let cfg = generate_config(&routes, "example.com");
        assert!(cfg.is_empty());
    }

    #[test]
    fn empty_routes_returns_empty_string() {
        assert!(generate_config(&[], "example.com").is_empty());
    }

    #[test]
    fn full_fqdn_subdomain_used_as_is() {
        let routes = vec![apps_route("myapp.custom.com", Some(3001))];
        let cfg = generate_config(&routes, "example.com");
        assert!(cfg.contains("server_name myapp.custom.com;"));
        assert!(!cfg.contains("myapp.custom.com.example.com"));
    }

    #[test]
    fn host_routes_excluded_from_nginx() {
        let routes = vec![host_route("mcp", 4040)];
        let cfg = generate_config(&routes, "example.com");
        assert!(cfg.is_empty());
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test --manifest-path system/orchestrator/Cargo.toml -- nginx::tests`
Expected: all pass

- [ ] **Step 3: Commit**

```
refactor(codery-ci): rewrite nginx.rs to use unified route map
```

---

## Task 9: Rewrite MCP add_app, remove_app, list_apps to use SQLite

**Files:**
- Modify: `system/orchestrator/src/mcp.rs`

This is the most invasive change to mcp.rs. Key changes:
- `add_app` → insert into SQLite + cascade (sync_launchy + apply_all + generate_and_reload)
- `remove_app` → delete from SQLite + cascade
- `list_apps` → query SQLite
- `get_routes` → use `db::build_route_map()`
- `reload_routes` → uses new caddy/nginx (no code change needed beyond what Task 7/8 already did)
- Remove `apps_routes_upsert()`, `apps_routes_remove()`, `AppRoute` references
- Remove port check from apps-routes.json → check SQLite instead
- Remove Launchy config file existence check → check SQLite instead

- [ ] **Step 1: Update imports**

In `mcp.rs`, change the use line:

```rust
use crate::{caddy, config, db, deploy, images, nginx, preflight, service_def::ServiceDef, state};
```

- [ ] **Step 2: Rewrite add_app**

Replace the `add_app` method body (lines ~1016-1146) with:

```rust
    async fn add_app(
        &self,
        Parameters(p): Parameters<AddAppParams>,
    ) -> Result<CallToolResult, McpError> {
        if p.name.contains(' ') || p.name.contains('/') || p.name.contains('.') {
            return Err(tool_err(
                "app name must not contain spaces, slashes, or dots",
            ));
        }

        let check = container_exec("apps", &["test", "-d", &p.directory])
            .await
            .map_err(|e| tool_err(format!("failed to check directory: {}", e)))?;
        if check.starts_with("[exited") {
            return Err(tool_err(format!(
                "directory '{}' does not exist in apps container",
                p.directory
            )));
        }

        let conn = db::open().map_err(|e| tool_err(e.to_string()))?;
        db::init(&conn).map_err(|e| tool_err(e.to_string()))?;

        if db::port_claimed(&conn, p.internal_port).map_err(|e| tool_err(e.to_string()))? {
            return Err(tool_err(format!(
                "port {} already claimed by another app",
                p.internal_port
            )));
        }

        if db::find_app_by_name(&conn, &p.name).map_err(|e| tool_err(e.to_string()))?.is_some() {
            return Err(tool_err(format!(
                "app '{}' already exists",
                p.name
            )));
        }

        let env_json = p.env.as_ref().and_then(|e| {
            if e.is_empty() { None } else { Some(serde_json::to_string(e).unwrap()) }
        });

        let app = db::AppRecord {
            name: p.name.clone(),
            subdomain: p.subdomain.clone(),
            internal_port: p.internal_port,
            command: p.command.clone(),
            directory: p.directory.clone(),
            env: env_json,
            priority: 100,
            user: "gem".to_string(),
            restart: "always".to_string(),
            created_at: String::new(),
        };

        db::insert_app(&conn, &app).map_err(|e| tool_err(e.to_string()))?;
        db::sync_launchy(&conn).map_err(|e| tool_err(e.to_string()))?;

        container_exec("apps", &["kill", "-HUP", "1"])
            .await
            .map_err(|e| tool_err(format!("failed to signal Launchy: {}", e)))?;

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        caddy::apply_all().map_err(|e| tool_err(e.to_string()))?;
        nginx::generate_and_reload()
            .await
            .map_err(|e| tool_err(e.to_string()))?;

        let status_output = container_exec("apps", &["cat", "/run/launchy-status.json"])
            .await
            .unwrap_or_else(|e| format!("(status read failed: {})", e));
        let running = if !status_output.starts_with("[exited") && !status_output.starts_with("(") {
            status_output.contains(&format!("\"{}\"", &p.name))
                || status_output.contains(&format!("\"name\":\"{}\"", &p.name))
        } else {
            false
        };

        if !running {
            return Err(tool_err(format!(
                "App '{}' config written and route added, but app not found in Launchy status. \
                 Check logs: read_container_file service='apps' path='/var/log/launchy/{}.log'",
                p.name, p.name
            )));
        }

        let response = json!({
            "name": p.name,
            "subdomain": p.subdomain,
            "internal_port": p.internal_port,
            "directory": p.directory,
            "status": "running",
            "guidance": {
                "what": "App started instantly via Launchy. No container rebuild.",
                "persistence": "Runtime apps persist across container restarts AND blue/green redeploys (stored in SQLite)",
                "to_remove": "remove_app name='{}' — stops process, deletes config, removes route",
                "to_check": "get_app_status shows per-app process state",
                "to_read_logs": "read_container_file service='apps' path='/var/log/launchy/{name}.log'"
            }
        });
        let json = serde_json::to_string_pretty(&response).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }
```

- [ ] **Step 3: Rewrite remove_app**

Replace the `remove_app` method body with:

```rust
    async fn remove_app(
        &self,
        Parameters(p): Parameters<RemoveAppParams>,
    ) -> Result<CallToolResult, McpError> {
        let conn = db::open().map_err(|e| tool_err(e.to_string()))?;
        db::init(&conn).map_err(|e| tool_err(e.to_string()))?;

        let subdomain = p.subdomain.unwrap_or_else(|| p.name.clone());

        let deleted = db::delete_app(&conn, &p.name).map_err(|e| tool_err(e.to_string()))?;
        if !deleted {
            return Err(tool_err(format!(
                "app '{}' not found in database",
                p.name
            )));
        }

        db::sync_launchy(&conn).map_err(|e| tool_err(e.to_string()))?;

        container_exec("apps", &["kill", "-HUP", "1"])
            .await
            .map_err(|e| tool_err(format!("failed to signal Launchy: {}", e)))?;

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        caddy::apply_all().map_err(|e| tool_err(e.to_string()))?;
        nginx::generate_and_reload()
            .await
            .map_err(|e| tool_err(e.to_string()))?;

        let response = json!({
            "name": p.name,
            "subdomain": subdomain,
            "status": "removed",
            "guidance": {
                "what": "App stopped, config deleted, route removed.",
                "to_verify": "list_apps shows remaining apps"
            }
        });
        let json = serde_json::to_string_pretty(&response).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }
```

- [ ] **Step 4: Rewrite list_apps**

Replace the `list_apps` method body with:

```rust
    async fn list_apps(&self) -> Result<CallToolResult, McpError> {
        let conn = db::open().map_err(|e| tool_err(e.to_string()))?;
        db::init(&conn).map_err(|e| tool_err(e.to_string()))?;
        let apps = db::list_apps(&conn).map_err(|e| tool_err(e.to_string()))?;
        let response = json!({
            "apps": apps,
            "guidance": {
                "to_add": "add_app name='myapp' subdomain='myapp' internal_port=3001 command='...' directory='...'",
                "to_remove": "remove_app name='myapp'",
                "to_check_status": "get_app_status shows per-app process state",
                "routing": "Caddy → Nginx (8080) → app process (internal_port)"
            }
        });
        let json = serde_json::to_string_pretty(&response).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }
```

- [ ] **Step 5: Rewrite get_routes to use build_route_map**

Replace the `get_routes` method body with:

```rust
    async fn get_routes(&self) -> Result<CallToolResult, McpError> {
        let defs = ServiceDef::load_all().map_err(|e| tool_err(e.to_string()))?;
        let domain = config::load_domain();

        let mut services_map = HashMap::new();
        for def in &defs {
            let color = state::read_active(&def.service).unwrap_or_else(|_| "blue".to_string());
            services_map.insert(def.service.clone(), color.clone());
        }

        let conn = db::open().map_err(|e| tool_err(e.to_string()))?;
        db::init(&conn).map_err(|e| tool_err(e.to_string()))?;
        let routes = db::build_route_map(&conn).map_err(|e| tool_err(e.to_string()))?;

        let route_entries: Vec<RouteEntry> = routes.iter().map(|r| {
            let fqdn = if r.subdomain.contains('.') {
                r.subdomain.clone()
            } else {
                format!("{}.{}", r.subdomain, domain)
            };
            let color = services_map.get(&r.target).map(|s| s.as_str());
            let host_port = match r.target.as_str() {
                "host" => r.port,
                "sandbox" => {
                    let c = color.unwrap_or("blue");
                    r.port + if c == "blue" { 10000 } else { 20000 }
                }
                _ => {
                    let c = color.unwrap_or("blue");
                    let def = ServiceDef::load(&r.target).ok();
                    if let Some(def) = def {
                        def.port_scheme.host_port(c, r.port)
                    } else {
                        r.port
                    }
                }
            };
            RouteEntry {
                subdomain: fqdn,
                host_port,
                container_port: Some(r.port),
                internal_port: r.internal_port,
                service: r.target.clone(),
                color: color.map(|s| s.to_string()),
                note: None,
            }
        }).collect();

        let table = RoutingTable {
            services: services_map,
            routes: route_entries,
        };
        let response = json!({
            "routing": table,
            "guidance": {
                "routing_model": "Traffic: Internet → Tailscale → Caddy → Nginx (8080) → app (internal_port)",
                "apps_ports": "For apps: container_port is always 8080 (Nginx). internal_port is where app listens.",
                "sandbox_ports": "For sandbox: container_port is the actual service port (e.g. 3000).",
                "to_add_route": "Use add_app for instant routing, or edit routes.yaml for static routes."
            }
        });
        let json = serde_json::to_string_pretty(&response).map_err(|e| tool_err(e.to_string()))?;
        tool_ok(json)
    }
```

- [ ] **Step 6: Remove dead code**

Delete from `mcp.rs`:
- The `apps_routes_upsert()` function (lines ~1304-1320)
- The `apps_routes_remove()` function (lines ~1323-1334)

- [ ] **Step 7: Update reload_routes description string**

Update the `reload_routes` tool description to reference `routes.yaml` instead of `apps-routes.json`:

```rust
    #[tool(
        description = "Reload Caddy routing from all service definitions, routes.yaml, and \
                        SQLite runtime apps without restarting containers. Use after \
                        editing proxy/routes.yaml."
    )]
```

And update its response guidance:

```rust
        let response = json!({
            "status": "ok",
            "guidance": {
                "what": "Caddy and Nginx reloaded. No container restart.",
                "when_to_use": "After editing routes.yaml or service YAMLs",
                "when_not_to_use": "For Dockerfile/service.yml changes — push to main"
            }
        });
```

- [ ] **Step 8: Update INSTRUCTIONS constant**

Update references in the `INSTRUCTIONS` string:
- Replace `apps-routes.json` references with `SQLite` / `routes.yaml`
- Replace `host-routes.json` references with `routes.yaml`
- Update the persistence description to mention SQLite

- [ ] **Step 9: Verify it compiles**

Run: `cargo check --manifest-path system/orchestrator/Cargo.toml`
Expected: compiles

- [ ] **Step 10: Run all tests**

Run: `cargo test --manifest-path system/orchestrator/Cargo.toml`
Expected: all pass

- [ ] **Step 11: Commit**

```
refactor(codery-ci): rewrite MCP tools to use SQLite for app state
```

---

## Task 10: Wire DB init into startup paths

**Files:**
- Modify: `system/orchestrator/src/main.rs`

- [ ] **Step 1: Add db::init + sync_launchy to reload-routes subcommand**

In `main.rs`, in the `Some("reload-routes")` arm, add DB init before `caddy::apply_all()`:

```rust
        Some("reload-routes") => {
            let conn = db::open()?;
            db::init(&conn)?;
            db::sync_launchy(&conn)?;
            caddy::apply_all()?;
            nginx::generate_and_reload().await?;
            println!("[routes] Reloaded Caddyfile and Nginx");
        }
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check --manifest-path system/orchestrator/Cargo.toml`
Expected: compiles

- [ ] **Step 3: Commit**

```
feat(codery-ci): init DB and sync Launchy on reload-routes
```

---

## Task 11: Clean up config.rs — remove old constants

**Files:**
- Modify: `system/orchestrator/src/config.rs`

- [ ] **Step 1: Remove dead constants**

In `config.rs`, remove:
- `pub const APPS_ROUTES` (line 6)
- `pub const SANDBOX_ROUTES` (line 13)
- `pub const HOST_ROUTES` (line 41)

Keep `DB_PATH`, `ROUTES_YAML`, and all other constants.

- [ ] **Step 2: Verify it compiles**

Run: `cargo check --manifest-path system/orchestrator/Cargo.toml`
Expected: compiles (if any references remain, fix them)

- [ ] **Step 3: Run all tests**

Run: `cargo test --manifest-path system/orchestrator/Cargo.toml`
Expected: all pass

- [ ] **Step 4: Commit**

```
refactor(codery-ci): remove legacy JSON route path constants from config.rs
```

---

## Task 12: Update CI workflows

**Files:**
- Modify: `.github/workflows/deploy-apps.yml`
- Modify: `.github/workflows/deploy-sandbox.yml`

- [ ] **Step 1: Update deploy-apps.yml**

Remove these steps:
- "Generate apps-routes.json from devcontainer.json" (lines 65-66)
- The second "Generate apps-routes.json from devcontainer.json" in deploy job (lines 93-94)
- The SCP of `proxy/apps-routes.json` (line 108)

Replace the deploy job steps that sync routes with:

```yaml
      - name: Sync routes.yaml and service YAML
        env:
          DEPLOY_HOST: ${{ secrets.DEPLOY_HOST }}
        run: |
          # (SCP setup lines remain the same)
          $SSH "sudo mkdir -p /opt/codery/services /opt/codery/proxy /opt/codery/apps-launchy.d /opt/codery/state"
          cat containers/apps/service.yml | $SSH "sudo tee /opt/codery/services/apps.yml"
          cat proxy/routes.yaml | $SSH "sudo tee /opt/codery/proxy/routes.yaml"
```

Keep the rest of the deploy job (GHCR creds, nginx touch, codery-ci deploy).

- [ ] **Step 2: Update deploy-sandbox.yml**

Replace the line:
```bash
cat proxy/sandbox-routes.json | $SSH "sudo tee /opt/codery/proxy/sandbox-routes.json" > /dev/null
```

With:
```bash
cat proxy/routes.yaml | $SSH "sudo tee /opt/codery/proxy/routes.yaml" > /dev/null
```

- [ ] **Step 3: Commit**

```
refactor(ci): update workflows to sync routes.yaml instead of legacy JSON files
```

---

## Task 13: Full integration test — cargo test

**Files:** None (verification only)

- [ ] **Step 1: Run full test suite**

Run: `cargo test --manifest-path system/orchestrator/Cargo.toml`
Expected: all tests pass, no compilation warnings about dead code

- [ ] **Step 2: Run cargo clippy**

Run: `cargo clippy --manifest-path system/orchestrator/Cargo.toml -- -D warnings`
Expected: no errors (fix any that appear)

- [ ] **Step 3: Commit any fixes**

If clippy or tests required fixes:
```
fix(codery-ci): address clippy warnings and test failures
```
