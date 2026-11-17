//! Durable gate state (GATE-009): rusqlite handle wrapper with ordered
//! migrations. Timestamps are unix epoch seconds (integers).

use rusqlite::{Connection, OptionalExtension};

/// A registry row: what role the gate actually holds in a repo, what role
/// the agent intends to exercise, and how the grant was obtained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryRow {
    pub repo: String,
    pub granted_role: String,
    pub intended_role: String,
    pub via: String,
    pub last_verified_at: i64,
}

/// Ordered migration SQL steps; index + 1 is the schema version.
const MIGRATIONS: &[&str] = &[include_str!("migrations/v1.sql")];

/// Current unix epoch seconds.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Wrapper over the SQLite connection holding all gate state.
pub struct State {
    conn: Connection,
}

impl State {
    /// Open (creating if needed) the state database at `path`, apply pending
    /// migrations, and enable WAL journaling.
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let state = Self { conn };
        state.migrate()?;
        Ok(state)
    }

    /// Apply migration steps with a schema version higher than the recorded
    /// one, then record the new version. Safe to run any number of times.
    fn migrate(&self) -> anyhow::Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS _meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )",
        )?;
        let current: i64 = self.conn.query_row(
            "SELECT COALESCE(
                (SELECT CAST(value AS INTEGER) FROM _meta WHERE key = 'schema_version'),
                0
            )",
            [],
            |r| r.get(0),
        )?;
        for (idx, step) in MIGRATIONS.iter().enumerate() {
            let version = (idx + 1) as i64;
            if version <= current {
                continue;
            }
            let tx = self.conn.unchecked_transaction()?;
            tx.execute_batch(step)?;
            tx.execute(
                "INSERT INTO _meta(key, value) VALUES ('schema_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [version.to_string()],
            )?;
            tx.commit()?;
        }
        Ok(())
    }

    /// Kill-switch: is pushes to `repo` blocked?
    pub fn is_blocked(&self, repo: &str) -> anyhow::Result<bool> {
        let hit: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM blocked_repos WHERE repo = ?1",
                [repo],
                |r| r.get(0),
            )
            .optional()?;
        Ok(hit.is_some())
    }

    /// Set or clear the kill-switch for `repo`. Blocking an already blocked
    /// repo keeps the original `blocked_at`.
    pub fn set_blocked(&self, repo: &str, blocked: bool) -> anyhow::Result<()> {
        if blocked {
            self.conn.execute(
                "INSERT INTO blocked_repos(repo, blocked_at) VALUES (?1, ?2)
                 ON CONFLICT(repo) DO NOTHING",
                rusqlite::params![repo, now()],
            )?;
        } else {
            self.conn
                .execute("DELETE FROM blocked_repos WHERE repo = ?1", [repo])?;
        }
        Ok(())
    }

    pub fn registry_get(&self, repo: &str) -> anyhow::Result<Option<RegistryRow>> {
        self.conn
            .query_row(
                "SELECT repo, granted_role, intended_role, via, last_verified_at
                 FROM registry WHERE repo = ?1",
                [repo],
                |r| {
                    Ok(RegistryRow {
                        repo: r.get(0)?,
                        granted_role: r.get(1)?,
                        intended_role: r.get(2)?,
                        via: r.get(3)?,
                        last_verified_at: r.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Insert or refresh the registry row for `repo`, stamping
    /// `last_verified_at` with the current time.
    pub fn registry_upsert(
        &self,
        repo: &str,
        granted_role: &str,
        intended_role: &str,
        via: &str,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO registry(repo, granted_role, intended_role, via, last_verified_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(repo) DO UPDATE SET
                granted_role = excluded.granted_role,
                intended_role = excluded.intended_role,
                via = excluded.via,
                last_verified_at = excluded.last_verified_at",
            rusqlite::params![repo, granted_role, intended_role, via, now()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_tmp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "gate-state-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn migrations_run_twice_idempotently_and_create_v1_tables() {
        let s = State::open(":memory:").expect("open");
        // Second migration pass over the same connection: must be a no-op.
        s.migrate().expect("migrate twice");

        let mut stmt = s
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        for table in [
            "agent_tokens",
            "registry",
            "blocked_repos",
            "pending",
            "setup_progress",
            "flows",
            "kv",
        ] {
            assert!(names.iter().any(|n| n == table), "missing table {table}");
        }
        let version: String = s
            .conn
            .query_row(
                "SELECT value FROM _meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(version, "1");
    }

    #[test]
    fn blocked_set_get_roundtrip() {
        let s = State::open(":memory:").unwrap();
        // Unknown repos are not blocked (fail open for the kill-switch check
        // itself; the policy engine fails closed separately).
        assert!(!s.is_blocked("acme/widget").unwrap());
        s.set_blocked("acme/widget", true).unwrap();
        assert!(s.is_blocked("acme/widget").unwrap());
        // Re-blocking must not error nor clear the row.
        s.set_blocked("acme/widget", true).unwrap();
        assert!(s.is_blocked("acme/widget").unwrap());
        s.set_blocked("acme/widget", false).unwrap();
        assert!(!s.is_blocked("acme/widget").unwrap());
        // Other repos unaffected.
        assert!(!s.is_blocked("acme/other").unwrap());
    }

    #[test]
    fn registry_upsert_get_roundtrip() {
        let s = State::open(":memory:").unwrap();
        assert!(s.registry_get("acme/widget").unwrap().is_none());

        s.registry_upsert("acme/widget", "admin", "admin", "device-flow")
            .unwrap();
        let row = s.registry_get("acme/widget").unwrap().expect("row present");
        assert_eq!(row.repo, "acme/widget");
        assert_eq!(row.granted_role, "admin");
        assert_eq!(row.intended_role, "admin");
        assert_eq!(row.via, "device-flow");
        assert!(row.last_verified_at > 0, "verification stamped");

        // Upsert overwrites the grant in place.
        s.registry_upsert("acme/widget", "write", "maintain", "oauth-web")
            .unwrap();
        let row = s.registry_get("acme/widget").unwrap().unwrap();
        assert_eq!(row.granted_role, "write");
        assert_eq!(row.intended_role, "maintain");
        assert_eq!(row.via, "oauth-web");
    }

    #[test]
    fn file_backed_state_persists_across_reopen() {
        let dir = unique_tmp_dir("persist");
        let path = dir.join("state.db");
        {
            let s = State::open(path.to_str().unwrap()).unwrap();
            s.set_blocked("acme/widget", true).unwrap();
            s.registry_upsert("acme/widget", "admin", "admin", "device-flow")
                .unwrap();
        }
        // Reopen re-runs migrations against the existing schema: idempotent.
        let s2 = State::open(path.to_str().unwrap()).unwrap();
        assert!(s2.is_blocked("acme/widget").unwrap());
        let row = s2.registry_get("acme/widget").unwrap().unwrap();
        assert_eq!(row.granted_role, "admin");
        std::fs::remove_dir_all(&dir).ok();
    }
}
