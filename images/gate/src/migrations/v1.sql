-- GitAuthGate state schema v1 (GATE-009). Idempotent by construction:
-- the migration runner applies each step at most once, guarded by the
-- schema_version row in _meta. All timestamps are unix epoch seconds.

CREATE TABLE IF NOT EXISTS agent_tokens (
    name TEXT PRIMARY KEY,
    hash TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_used_at INTEGER
);

CREATE TABLE IF NOT EXISTS registry (
    repo TEXT PRIMARY KEY,
    granted_role TEXT NOT NULL,
    intended_role TEXT NOT NULL,
    via TEXT NOT NULL,
    last_verified_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS blocked_repos (
    repo TEXT PRIMARY KEY,
    blocked_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS pending (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    decided_at INTEGER,
    outcome TEXT
);

CREATE TABLE IF NOT EXISTS setup_progress (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS flows (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    state TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS kv (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
