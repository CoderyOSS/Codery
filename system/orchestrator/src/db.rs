use std::collections::HashMap;

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
    pub no_cache: bool,
    pub source: String,
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

    let _ = conn.execute_batch(
        "ALTER TABLE apps ADD COLUMN no_cache INTEGER NOT NULL DEFAULT 0;"
    );

    let _ = conn.execute_batch(
        "ALTER TABLE apps ADD COLUMN source TEXT NOT NULL DEFAULT 'runtime';"
    );

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS previews (
            service     TEXT PRIMARY KEY,
            subdomain   TEXT NOT NULL UNIQUE,
            host_port   INTEGER NOT NULL,
            sha         TEXT NOT NULL,
            color       TEXT NOT NULL,
            created_at  TEXT NOT NULL DEFAULT (datetime('now'))
        );"
    ).context("failed to create previews table")?;

    Ok(())
}

pub fn insert_app(conn: &Connection, app: &AppRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO apps (name, subdomain, internal_port, command, directory, env, priority, user, restart, no_cache, source)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
            app.no_cache as i64,
            &app.source,
        ),
    ).with_context(|| format!("failed to insert app '{}'", app.name))?;
    Ok(())
}

pub fn delete_app(conn: &Connection, name: &str) -> Result<bool> {
    let rows = conn.execute("DELETE FROM apps WHERE name = ?1", [name])
        .with_context(|| format!("failed to delete app '{}'", name))?;
    Ok(rows > 0)
}

pub fn set_app_source(conn: &Connection, name: &str, source: &str) -> Result<bool> {
    let rows = conn
        .execute("UPDATE apps SET source = ?1 WHERE name = ?2", (source, name))
        .with_context(|| format!("failed to set source for app '{}'", name))?;
    Ok(rows > 0)
}

pub fn list_apps(conn: &Connection) -> Result<Vec<AppRecord>> {
    let mut stmt = conn.prepare(
        "SELECT name, subdomain, internal_port, command, directory, env, priority, user, restart, no_cache, source, created_at
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
            no_cache: row.get::<_, i64>(9)? != 0,
            source: row.get(10)?,
            created_at: row.get(11)?,
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

// ── Preview routes ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewRecord {
    pub service: String,
    pub subdomain: String,
    pub host_port: u16,
    pub sha: String,
    pub color: String,
    pub created_at: String,
}

/// Insert or replace a preview route for a service.
/// Subdomain defaults to "{service}-preview" if not provided.
pub fn upsert_preview(conn: &Connection, preview: &PreviewRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO previews (service, subdomain, host_port, sha, color)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(service) DO UPDATE SET
            subdomain = excluded.subdomain,
            host_port = excluded.host_port,
            sha       = excluded.sha,
            color     = excluded.color",
        (
            &preview.service,
            &preview.subdomain,
            preview.host_port as i64,
            &preview.sha,
            &preview.color,
        ),
    )
    .with_context(|| format!("failed to upsert preview for '{}'", preview.service))?;
    Ok(())
}

pub fn delete_preview(conn: &Connection, service: &str) -> Result<bool> {
    let rows = conn.execute("DELETE FROM previews WHERE service = ?1", [service])
        .with_context(|| format!("failed to delete preview for '{}'", service))?;
    Ok(rows > 0)
}

pub fn list_previews(conn: &Connection) -> Result<Vec<PreviewRecord>> {
    let mut stmt = conn.prepare(
        "SELECT service, subdomain, host_port, sha, color, created_at
         FROM previews ORDER BY service"
    ).context("failed to prepare previews query")?;
    let rows = stmt.query_map([], |row| {
        Ok(PreviewRecord {
            service:   row.get(0)?,
            subdomain: row.get(1)?,
            host_port: row.get::<_, i64>(2)? as u16,
            sha:       row.get(3)?,
            color:     row.get(4)?,
            created_at:row.get(5)?,
        })
    }).context("failed to query previews")?;
    let mut previews = Vec::new();
    for p in rows {
        previews.push(p.context("failed to read preview row")?);
    }
    Ok(previews)
}

pub fn find_preview(conn: &Connection, service: &str) -> Result<Option<PreviewRecord>> {
    let previews = list_previews(conn)?;
    Ok(previews.into_iter().find(|p| p.service == service))
}

/// Default preview subdomain for a service: "{service}-preview".
pub fn preview_subdomain(service: &str) -> String {
    format!("{}-preview", service)
}

// ── Routes.yaml types and loader ──────────────────────────────────────────────

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

// ── Unified route map ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct UnifiedRoute {
    pub subdomain: String,
    pub port: u16,
    pub target: String,
    pub internal_port: Option<u16>,
    pub no_cache: bool,
}

pub fn build_route_map(conn: &Connection) -> Result<Vec<UnifiedRoute>> {
    let mut map: HashMap<String, UnifiedRoute> = HashMap::new();

    let defs = crate::service_def::ServiceDef::load_all()?;
    for def in &defs {
        for port in &def.ports {
            if let Some(subdomain) = &port.subdomain {
                map.entry(subdomain.clone()).or_insert(UnifiedRoute {
                    subdomain: subdomain.clone(),
                    port: port.container_port,
                    target: def.service.clone(),
                    internal_port: None,
                    no_cache: false,
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
            no_cache: false,
        });
    }

    let apps = list_apps(conn)?;
    for app in &apps {
        map.insert(app.subdomain.clone(), UnifiedRoute {
            subdomain: app.subdomain.clone(),
            port: 8080,
            target: "apps".to_string(),
            internal_port: Some(app.internal_port),
            no_cache: app.no_cache,
        });
    }

    let previews = list_previews(conn)?;
    for p in &previews {
        map.insert(p.subdomain.clone(), UnifiedRoute {
            subdomain: p.subdomain.clone(),
            port: p.host_port,
            target: "host".to_string(),
            internal_port: None,
            no_cache: true,
        });
    }

    let mut routes: Vec<UnifiedRoute> = map.into_values().collect();
    routes.sort_by(|a, b| a.subdomain.cmp(&b.subdomain));
    Ok(routes)
}

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

#[cfg(test)]
mod tests {
    use super::*;

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
            no_cache: false,
            source: "runtime".to_string(),
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

    // ── Preview tests ──────────────────────────────────────────────────────────

    fn sample_preview(service: &str) -> PreviewRecord {
        PreviewRecord {
            service: service.to_string(),
            subdomain: preview_subdomain(service),
            host_port: 23000,
            sha: "abc123".to_string(),
            color: "green".to_string(),
            created_at: String::new(),
        }
    }

    #[test]
    fn preview_subdomain_format() {
        assert_eq!(preview_subdomain("sandbox"), "sandbox-preview");
        assert_eq!(preview_subdomain("apps"), "apps-preview");
    }

    #[test]
    fn preview_insert_and_list() {
        let conn = test_conn();
        upsert_preview(&conn, &sample_preview("sandbox")).unwrap();
        let previews = list_previews(&conn).unwrap();
        assert_eq!(previews.len(), 1);
        assert_eq!(previews[0].service, "sandbox");
        assert_eq!(previews[0].subdomain, "sandbox-preview");
        assert_eq!(previews[0].host_port, 23000);
    }

    #[test]
    fn preview_upsert_replaces() {
        let conn = test_conn();
        upsert_preview(&conn, &sample_preview("sandbox")).unwrap();
        let mut updated = sample_preview("sandbox");
        updated.host_port = 13000;
        updated.color = "blue".to_string();
        upsert_preview(&conn, &updated).unwrap();

        let previews = list_previews(&conn).unwrap();
        assert_eq!(previews.len(), 1, "upsert should replace not insert");
        assert_eq!(previews[0].host_port, 13000);
        assert_eq!(previews[0].color, "blue");
    }

    #[test]
    fn preview_delete() {
        let conn = test_conn();
        upsert_preview(&conn, &sample_preview("sandbox")).unwrap();
        assert!(delete_preview(&conn, "sandbox").unwrap());
        assert!(list_previews(&conn).unwrap().is_empty());
    }

    #[test]
    fn preview_delete_nonexistent_returns_false() {
        let conn = test_conn();
        assert!(!delete_preview(&conn, "nope").unwrap());
    }

    #[test]
    fn preview_find_by_service() {
        let conn = test_conn();
        upsert_preview(&conn, &sample_preview("sandbox")).unwrap();
        assert!(find_preview(&conn, "sandbox").unwrap().is_some());
        assert!(find_preview(&conn, "apps").unwrap().is_none());
    }

    #[test]
    fn preview_appears_in_route_map() {
        let conn = test_conn();
        upsert_preview(&conn, &sample_preview("sandbox")).unwrap();
        let routes = build_route_map(&conn).unwrap();
        let p = routes.iter().find(|r| r.subdomain == "sandbox-preview");
        assert!(p.is_some(), "preview route should appear in route map");
        let p = p.unwrap();
        assert_eq!(p.port, 23000);
        assert_eq!(p.target, "host");
        assert!(p.no_cache, "preview routes should be no_cache");
    }
}
