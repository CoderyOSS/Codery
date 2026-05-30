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

    Ok(())
}

pub fn insert_app(conn: &Connection, app: &AppRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO apps (name, subdomain, internal_port, command, directory, env, priority, user, restart, no_cache)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
        "SELECT name, subdomain, internal_port, command, directory, env, priority, user, restart, no_cache, created_at
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
            created_at: row.get(10)?,
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
            if let Ok(env_map) = serde_json::from_str::<HashMap<String, String>>(env_json) {
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
            no_cache: false,
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
