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

    /// Crate-internal handle on the shared connection, so companion modules
    /// (e.g. the token store) can run their SQL against the same database
    /// without a second handle.
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
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

    /// Raw key-value rows (`kv`): machine-token ciphertext, setup-nonce
    /// state, and other single-slot durable values that don't warrant a
    /// table of their own.
    pub fn kv_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .map_err(Into::into)
    }

    // -- setup_progress (GATE-011): the guided walkthrough's durable state --

    /// One setup-progress value (`None` when absent).
    pub fn setup_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM setup_progress WHERE key = ?1",
                [key],
                |r| r.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// `updated_at` of one setup-progress row (`None` when absent) — the
    /// restart-resume "older than 1 h ⇒ resend" rule reads this on the
    /// `step` row.
    pub fn setup_updated_at(&self, key: &str) -> anyhow::Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT updated_at FROM setup_progress WHERE key = ?1",
                [key],
                |r| r.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Upsert a setup-progress row, stamping `updated_at` with now.
    pub fn setup_set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.setup_set_with_ts(key, value, now())
    }

    /// Upsert with an explicit timestamp (the resume test backdates the
    /// step row past the 1 h resend threshold).
    pub fn setup_set_with_ts(&self, key: &str, value: &str, updated_at: i64) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO setup_progress(key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_at = excluded.updated_at",
            rusqlite::params![key, value, updated_at],
        )?;
        Ok(())
    }

    /// Delete one setup-progress row; true when a row existed.
    pub fn setup_delete(&self, key: &str) -> anyhow::Result<bool> {
        let deleted = self
            .conn
            .execute("DELETE FROM setup_progress WHERE key = ?1", [key])?;
        Ok(deleted > 0)
    }

    /// Clear every setup-progress row (`[Cancel setup]` / finish).
    pub fn setup_clear(&self) -> anyhow::Result<()> {
        self.conn.execute("DELETE FROM setup_progress", [])?;
        Ok(())
    }


    /// Insert or overwrite a kv row.
    pub fn kv_set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO kv(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    /// Delete a kv row; true when a row existed.
    pub fn kv_delete(&self, key: &str) -> anyhow::Result<bool> {
        let deleted = self.conn.execute("DELETE FROM kv WHERE key = ?1", [key])?;
        Ok(deleted > 0)
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

    /// Insert or refresh a control-flow row (`flows`): elevations, setup
    /// runs, and everything a control plane tracks across restarts.
    pub fn flow_insert(
        &self,
        id: &str,
        kind: &str,
        flow_state: &str,
        payload_json: &str,
    ) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT INTO flows(id, kind, state, payload_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                kind = excluded.kind,
                state = excluded.state,
                payload_json = excluded.payload_json,
                updated_at = excluded.updated_at",
            rusqlite::params![id, kind, flow_state, payload_json, now()],
        )?;
        Ok(())
    }

    /// Current `state` string for flow `id` (`None` when the id is
    /// unknown). Plan-B control planes rewrite the state column as a flow
    /// progresses; readers only ever need the latest.
    pub fn flow_state(&self, id: &str) -> anyhow::Result<Option<String>> {
        self.conn()
            .query_row("SELECT state FROM flows WHERE id = ?1", [id], |r| r.get(0))
            .optional()
            .map_err(Into::into)
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
    fn flow_insert_get_roundtrip() {
        let s = State::open(":memory:").unwrap();
        assert_eq!(s.flow_state("flow-1").unwrap(), None);
        s.flow_insert("flow-1", "elevation", "DETECTED", r#"{"repo":"octo/x"}"#)
            .unwrap();
        assert_eq!(s.flow_state("flow-1").unwrap().as_deref(), Some("DETECTED"));
        // Upsert refreshes state in place (Plan B writes transitions).
        s.flow_insert("flow-1", "elevation", "GRANTED", r#"{"repo":"octo/x"}"#)
            .unwrap();
        assert_eq!(s.flow_state("flow-1").unwrap().as_deref(), Some("GRANTED"));
        // Other ids untouched; unknown ids stay None.
        assert_eq!(s.flow_state("flow-2").unwrap(), None);
    }

    #[test]
    fn kv_set_get_delete_roundtrip() {
        let s = State::open(":memory:").unwrap();
        assert_eq!(s.kv_get("setup_nonce").unwrap(), None);
        s.kv_set("setup_nonce", "abc").unwrap();
        // Upsert overwrites.
        s.kv_set("setup_nonce", "def").unwrap();
        assert_eq!(s.kv_get("setup_nonce").unwrap().as_deref(), Some("def"));
        assert!(s.kv_delete("setup_nonce").unwrap());
        assert_eq!(s.kv_get("setup_nonce").unwrap(), None);
        assert!(!s.kv_delete("setup_nonce").unwrap(), "second delete misses");
    }

    #[test]
    fn setup_progress_set_get_backdate_and_clear() {
        let s = State::open(":memory:").unwrap();
        assert_eq!(s.setup_get("step").unwrap(), None);
        s.setup_set("step", "3").unwrap();
        assert_eq!(s.setup_get("step").unwrap().as_deref(), Some("3"));
        let ts = s.setup_updated_at("step").unwrap().expect("stamped now");
        assert!(ts > 0, "upsert stamps the current time");

        // Explicit timestamp (resume test backdates past the 1 h mark).
        s.setup_set_with_ts("step", "3", ts - 7200).unwrap();
        assert_eq!(s.setup_updated_at("step").unwrap(), Some(ts - 7200));

        // Cancel/finish clears everything at once.
        s.setup_set("username", "rob-agent").unwrap();
        s.setup_clear().unwrap();
        assert_eq!(s.setup_get("step").unwrap(), None);
        assert_eq!(s.setup_get("username").unwrap(), None);
        assert!(!s.setup_delete("step").unwrap(), "nothing left to delete");
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
