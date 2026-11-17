//! Approval backend (GATE-005/006): dangerous operations are persisted as
//! `pending` rows and wait for a human decision. A hold dies with its
//! waiter: at the request's deadline `wait` resolves it `TimedOut`, and at
//! backend init the restart sweep resolves EVERY row still without an
//! outcome as `TimedOut` — the process that would have consumed a later
//! decision is gone, so a post-restart approve/deny can never silently
//! resurrect a hold. Cross-task wake is by polling: `resolve` only writes
//! the row, `wait` re-reads it, so a control plane in another task needs no
//! channel into the waiter.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::audit::{Audit, AuditEntry};
use crate::pktline::{Oid, RefUpdate};
use crate::state::{self, State};
use crate::types::ActionClass;

/// Default approval window (GATE-005): how long a hold waits for a human
/// before it resolves as `TimedOut`. Callers override per request; tests
/// use small durations.
pub const APPROVAL_TIMEOUT_SECS: u64 = 300;

/// Poll interval for `wait`: how often the pending row is re-read for an
/// external decision.
const POLL: Duration = Duration::from_millis(50);

/// What is being held for approval: a receive-pack push (with the parsed
/// ref updates and commit subjects) or a GitHub API call (with a redacted
/// body snippet and its digest).
#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalDetail {
    Push {
        updates: Vec<RefUpdate>,
        commits: Vec<String>,
    },
    Api {
        method: String,
        path: String,
        body_snippet: String,
        body_sha: String,
    },
}

/// A hold on a dangerous operation, addressed by `id` across the pipeline,
/// the control plane, and the persisted row.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    pub id: String,
    pub agent: String,
    pub repo: String,
    pub class: ActionClass,
    pub detail: ApprovalDetail,
    pub timeout: Duration,
}

/// Final disposition of a hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Approved,
    Denied,
    TimedOut,
}

impl ApprovalOutcome {
    /// Stable string spelling used in the `pending.outcome` column and the
    /// audit log.
    pub fn as_str(&self) -> &'static str {
        match self {
            ApprovalOutcome::Approved => "approved",
            ApprovalOutcome::Denied => "denied",
            ApprovalOutcome::TimedOut => "timed_out",
        }
    }
}

/// What the `pending.payload_json` column stores: everything `wait` needs
/// to race the request's own deadline without the request in hand.
#[derive(Serialize, Deserialize)]
struct PendingPayload {
    agent: String,
    repo: String,
    class: String,
    timeout_ms: u64,
    created_ms: u64,
    detail: Value,
}

/// Current unix epoch milliseconds (row deadlines are sub-second precise;
/// the `created_at` column stays in seconds alongside the rest of the
/// schema).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn class_str(class: ActionClass) -> &'static str {
    match class {
        ActionClass::Push => "push",
        ActionClass::ForcePush => "force_push",
        ActionClass::BranchDelete => "branch_delete",
        ActionClass::TagDelete => "tag_delete",
    }
}

fn detail_value(detail: &ApprovalDetail) -> Value {
    match detail {
        ApprovalDetail::Push { updates, commits } => json!({
            "updates": updates
                .iter()
                .map(|u| json!({
                    "old": u.old.to_hex(),
                    "new": u.new.to_hex(),
                    "refname": u.refname,
                }))
                .collect::<Vec<_>>(),
            "commits": commits,
        }),
        ApprovalDetail::Api {
            method,
            path,
            body_snippet,
            body_sha,
        } => json!({
            "method": method,
            "path": path,
            "body_snippet": body_snippet,
            "body_sha": body_sha,
        }),
    }
}

/// Persist a hold as an unresolved `pending` row: the row is the durable
/// truth a restart sees, so it carries the request's own timeout.
pub fn persist_request(state: &State, req: &ApprovalRequest) -> Result<()> {
    let kind = match req.detail {
        ApprovalDetail::Push { .. } => "approval.push",
        ApprovalDetail::Api { .. } => "approval.api",
    };
    let payload = PendingPayload {
        agent: req.agent.clone(),
        repo: req.repo.clone(),
        class: class_str(req.class).to_string(),
        timeout_ms: req.timeout.as_millis() as u64,
        created_ms: now_ms(),
        detail: detail_value(&req.detail),
    };
    state.conn().execute(
        "INSERT INTO pending(id, kind, payload_json, created_at, decided_at, outcome)
         VALUES (?1, ?2, ?3, ?4, NULL, NULL)",
        rusqlite::params![
            req.id,
            kind,
            serde_json::to_string(&payload)?,
            (payload.created_ms / 1000) as i64,
        ],
    )?;
    Ok(())
}

/// Resolve a hold. Idempotent: a decided or absent row is left untouched and
/// reported as `false` (the control plane replies "already timed out" from
/// the row state). Every actual change is audited with `basis = "approval"`.
pub fn resolve_pending(
    state: &State,
    audit: &Audit,
    id: &str,
    outcome: ApprovalOutcome,
) -> Result<bool> {
    let stored: Option<(String, Option<String>)> = state
        .conn()
        .query_row(
            "SELECT payload_json, outcome FROM pending WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((payload_json, existing)) = stored else {
        return Ok(false);
    };
    if existing.is_some() {
        return Ok(false);
    }
    let payload: PendingPayload = serde_json::from_str(&payload_json).unwrap_or(PendingPayload {
        agent: "-".to_string(),
        repo: "-".to_string(),
        class: "unknown".to_string(),
        timeout_ms: APPROVAL_TIMEOUT_SECS * 1000,
        created_ms: 0,
        detail: Value::Null,
    });
    let changed = state
        .conn()
        .execute(
            "UPDATE pending SET decided_at = ?2, outcome = ?3
             WHERE id = ?1 AND outcome IS NULL",
            rusqlite::params![id, state::now(), outcome.as_str()],
        )
        ? > 0;
    if changed {
        audit.record(AuditEntry {
            ts: state::now(),
            agent: Some(payload.agent),
            repo: payload.repo,
            action: "approval.resolve".to_string(),
            decision: outcome.as_str().to_string(),
            basis: "approval".to_string(),
            approver: None,
        });
    }
    Ok(changed)
}

/// Restart sweep (GATE-005, fail closed): EVERY approval row left without
/// an outcome resolves as `TimedOut`. The waiter died in the restart and no
/// live consumer exists, so any later decision on a surviving row would be
/// a phantom approval in the audit trail. Returns how many rows were
/// resolved.
pub fn sweep_unresolved_pending(state: &State, audit: &Audit) -> Result<usize> {
    let ids: Vec<String> = {
        let mut stmt = state
            .conn()
            .prepare("SELECT id FROM pending WHERE outcome IS NULL AND kind LIKE 'approval%'")?;
        let mapped = stmt.query_map([], |r| r.get(0))?;
        mapped.collect::<Result<Vec<_>, _>>()?
    };
    let mut swept = 0;
    for id in ids {
        swept += resolve_pending(state, audit, &id, ApprovalOutcome::TimedOut)? as usize;
    }
    Ok(swept)
}

/// Insert a default-window row for `id` if `present` never ran, so `wait`
/// always races a persisted row.
fn ensure_row(state: &State, id: &str) {
    let created_ms = now_ms();
    let payload = PendingPayload {
        agent: "-".to_string(),
        repo: "-".to_string(),
        class: "unknown".to_string(),
        timeout_ms: APPROVAL_TIMEOUT_SECS * 1000,
        created_ms,
        detail: Value::Null,
    };
    let _ = state.conn().execute(
        "INSERT OR IGNORE INTO pending(id, kind, payload_json, created_at, decided_at, outcome)
         VALUES (?1, 'approval', ?2, ?3, NULL, NULL)",
        rusqlite::params![
            id,
            serde_json::to_string(&payload).unwrap_or_default(),
            (created_ms / 1000) as i64
        ],
    );
}

/// Read `(created_ms, timeout_ms, decided_outcome)` for a row; a missing row
/// or corrupt payload falls back to the default window (fail closed).
fn row_state(state: &State, id: &str) -> (u64, u64, Option<ApprovalOutcome>) {
    let row: Option<(String, i64, Option<String>)> = state
        .conn()
        .query_row(
            "SELECT payload_json, created_at, outcome FROM pending WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .ok()
        .flatten();
    let Some((payload_json, created_at, outcome)) = row else {
        return (now_ms(), APPROVAL_TIMEOUT_SECS * 1000, None);
    };
    let decided = outcome.map(|o| match o.as_str() {
        "approved" => ApprovalOutcome::Approved,
        "denied" => ApprovalOutcome::Denied,
        _ => ApprovalOutcome::TimedOut,
    });
    let parsed: Option<PendingPayload> = serde_json::from_str(&payload_json).ok();
    let (created_ms, timeout_ms) = match parsed {
        Some(p) => (p.created_ms, p.timeout_ms),
        None => (created_at as u64 * 1000, APPROVAL_TIMEOUT_SECS * 1000),
    };
    (created_ms, timeout_ms, decided)
}

/// Test double (and default backend): `present` persists the hold without
/// notifying a human; `wait` races the row's deadline against an automatic
/// decision arriving after `max(timeout/10, 1s)`. The floor keeps the auto
/// decision from beating sub-second timeouts, which is what lets the
/// timeout test exercise `wait`'s own timeout path.
pub struct StubBackend {
    pub auto: ApprovalOutcome,
    pub state: Arc<Mutex<State>>,
    pub audit: Arc<Audit>,
}

impl StubBackend {
    /// Build the stub, sweeping every unresolved hold first (GATE-005: on
    /// backend init, restart resolves them as `TimedOut` — no live waiter
    /// required).
    pub fn new(
        state: Arc<Mutex<State>>,
        audit: Arc<Audit>,
        auto: ApprovalOutcome,
    ) -> Result<Self> {
        {
            let guard = state.lock().expect("state lock");
            sweep_unresolved_pending(&guard, &audit)?;
        }
        Ok(Self { auto, state, audit })
    }

    /// Resolve and return the row's settled outcome: whoever won the race
    /// wrote the row first, so the row — not the racer — is the truth.
    async fn finish(&self, id: &str, proposed: ApprovalOutcome) -> ApprovalOutcome {
        {
            let guard = self.state.lock().expect("state lock");
            let _ = resolve_pending(&guard, &self.audit, id, proposed);
        }
        let guard = self.state.lock().expect("state lock");
        row_state(&guard, id).2.unwrap_or(proposed)
    }
}

#[async_trait]
pub trait ApprovalBackend: Send + Sync {
    /// Notify a human that a hold needs a decision.
    async fn present(&self, req: &ApprovalRequest) -> anyhow::Result<()>;
    /// Block until the hold is decided or its deadline passes.
    async fn wait(&self, id: &str) -> ApprovalOutcome;
    /// Control-plane entry point; idempotent on decided/absent rows.
    async fn resolve(&self, id: &str, outcome: ApprovalOutcome) -> anyhow::Result<()>;
}

/// Delay before the stub's automatic decision lands: `timeout/10`, floored
/// at 1s so an auto decision cannot beat sub-second request timeouts.
fn auto_delay(timeout: Duration) -> Duration {
    timeout.mul_f64(0.1).max(Duration::from_secs(1))
}

#[async_trait]
impl ApprovalBackend for StubBackend {
    async fn present(&self, req: &ApprovalRequest) -> anyhow::Result<()> {
        {
            let guard = self.state.lock().expect("state lock");
            persist_request(&guard, req)?;
        }
        self.audit.record(AuditEntry {
            ts: state::now(),
            agent: Some(req.agent.clone()),
            repo: req.repo.clone(),
            action: "approval.request".to_string(),
            decision: "pending".to_string(),
            basis: "hold pending human decision".to_string(),
            approver: None,
        });
        Ok(())
    }

    async fn wait(&self, id: &str) -> ApprovalOutcome {
        ensure_row(&self.state.lock().expect("state lock"), id);
        let started = std::time::Instant::now();
        loop {
            let (created_ms, timeout_ms, decided) = {
                let guard = self.state.lock().expect("state lock");
                row_state(&guard, id)
            };
            if let Some(outcome) = decided {
                return outcome;
            }
            let now = now_ms();
            let deadline_ms = created_ms.saturating_add(timeout_ms);
            if now >= deadline_ms {
                return self.finish(id, ApprovalOutcome::TimedOut).await;
            }
            let auto_at = started + auto_delay(Duration::from_millis(timeout_ms));
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(deadline_ms - now)) => {
                    return self.finish(id, ApprovalOutcome::TimedOut).await;
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(auto_at)) => {
                    return self.finish(id, self.auto).await;
                }
                _ = tokio::time::sleep(POLL) => continue,
            }
        }
    }

    async fn resolve(&self, id: &str, outcome: ApprovalOutcome) -> anyhow::Result<()> {
        let guard = self.state.lock().expect("state lock");
        resolve_pending(&guard, &self.audit, id, outcome)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn unique_tmp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "gate-approval-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn temp_audit(tag: &str) -> (std::path::PathBuf, Arc<Audit>) {
        let dir = unique_tmp_dir(tag);
        let path = dir.join("audit.jsonl");
        let audit = Arc::new(Audit::open(&path));
        (path, audit)
    }

    fn shared_state() -> Arc<Mutex<State>> {
        Arc::new(Mutex::new(State::open(":memory:").unwrap()))
    }

    fn test_req(id: &str, timeout: Duration) -> ApprovalRequest {
        ApprovalRequest {
            id: id.to_string(),
            agent: "claude-code".to_string(),
            repo: "acme/widget".to_string(),
            class: ActionClass::Push,
            detail: ApprovalDetail::Push {
                updates: vec![RefUpdate {
                    old: Oid::from_hex(&"0".repeat(40)).unwrap(),
                    new: Oid::from_hex("5f2e91ab12cd34ef56ab78cd90ef12ab34cd56ef")
                        .unwrap(),
                    refname: "refs/heads/main".to_string(),
                }],
                commits: vec!["add feature".to_string()],
            },
            timeout,
        }
    }

    fn audit_lines(path: &std::path::Path) -> Vec<serde_json::Value> {
        let contents = std::fs::read_to_string(path).unwrap();
        contents
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn stub_auto_approve_resolves_approved() {
        let state = shared_state();
        let (audit_path, audit) = temp_audit("approve");
        let backend = StubBackend::new(state.clone(), audit.clone(), ApprovalOutcome::Approved)
            .unwrap();
        let req = test_req("hold-approve", Duration::from_secs(3));
        backend.present(&req).await.unwrap();

        let outcome = backend.wait("hold-approve").await;
        assert_eq!(outcome, ApprovalOutcome::Approved);

        let (created_ms, timeout_ms, decided) = {
            let guard = state.lock().unwrap();
            row_state(&guard, "hold-approve")
        };
        assert_eq!(decided, Some(ApprovalOutcome::Approved));
        assert!(created_ms > 0);
        assert_eq!(timeout_ms, 3000);

        let lines = audit_lines(&audit_path);
        assert_eq!(lines.len(), 2, "request + resolve events: {lines:?}");
        assert_eq!(lines[0]["action"], "approval.request");
        assert_eq!(lines[0]["decision"], "pending");
        assert_eq!(lines[0]["agent"], "claude-code");
        assert_eq!(lines[0]["repo"], "acme/widget");
        assert_eq!(lines[1]["action"], "approval.resolve");
        assert_eq!(lines[1]["decision"], "approved");
        assert_eq!(lines[1]["basis"], "approval");
    }

    #[tokio::test]
    async fn stub_auto_deny_resolves_denied() {
        let state = shared_state();
        let (_audit_path, audit) = temp_audit("deny");
        let backend = StubBackend::new(state.clone(), audit.clone(), ApprovalOutcome::Denied)
            .unwrap();
        backend
            .present(&test_req("hold-deny", Duration::from_secs(3)))
            .await
            .unwrap();

        let outcome = backend.wait("hold-deny").await;
        assert_eq!(outcome, ApprovalOutcome::Denied);

        let (_, _, decided) = {
            let guard = state.lock().unwrap();
            row_state(&guard, "hold-deny")
        };
        assert_eq!(decided, Some(ApprovalOutcome::Denied));
    }

    #[tokio::test]
    async fn wait_times_out_at_request_deadline_exercising_race() {
        let state = shared_state();
        let (audit_path, audit) = temp_audit("timeout");
        // Auto decision (denial) lands at max(200ms/10, 1s) = 1s: only
        // wait's own deadline race can produce the outcome.
        let backend = StubBackend::new(state.clone(), audit.clone(), ApprovalOutcome::Denied)
            .unwrap();
        backend
            .present(&test_req("hold-timeout", Duration::from_millis(200)))
            .await
            .unwrap();

        let started = Instant::now();
        let outcome = backend.wait("hold-timeout").await;
        let elapsed = started.elapsed();
        assert_eq!(outcome, ApprovalOutcome::TimedOut);
        assert!(
            elapsed < Duration::from_millis(900),
            "deadline must win the race before the auto decision: {elapsed:?}"
        );

        let (_, _, decided) = {
            let guard = state.lock().unwrap();
            row_state(&guard, "hold-timeout")
        };
        assert_eq!(decided, Some(ApprovalOutcome::TimedOut));

        let lines = audit_lines(&audit_path);
        let resolved: Vec<_> = lines
            .iter()
            .filter(|l| l["action"] == "approval.resolve")
            .collect();
        assert_eq!(resolved.len(), 1, "timed-out fate audited: {lines:?}");
        assert_eq!(resolved[0]["decision"], "timed_out");
        assert_eq!(resolved[0]["basis"], "approval");
    }

    #[tokio::test]
    async fn pending_row_persists_then_resolves_idempotently() {
        let state = shared_state();
        let (_audit_path, audit) = temp_audit("idem");

        // Before present: nothing.
        {
            let guard = state.lock().unwrap();
            let (_, _, decided) = row_state(&guard, "hold-row");
            assert_eq!(decided, None, "absent row reads as unresolved default");
        }

        let backend = StubBackend::new(state.clone(), audit.clone(), ApprovalOutcome::Approved)
            .unwrap();
        backend
            .present(&test_req("hold-row", Duration::from_secs(300)))
            .await
            .unwrap();

        {
            let guard = state.lock().unwrap();
            let stored: Option<(i64, Option<i64>, Option<String>)> = guard
                .conn()
                .query_row(
                    "SELECT created_at, decided_at, outcome FROM pending WHERE id = 'hold-row'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .unwrap();
            let (created_at, decided_at, outcome) = stored.expect("row persisted by present");
            assert!(created_at > 0);
            assert_eq!(decided_at, None, "hold starts undecided");
            assert_eq!(outcome, None);
        }

        let changed =
            resolve_pending(&state.lock().unwrap(), &audit, "hold-row", ApprovalOutcome::Denied)
                .unwrap();
        assert!(changed);
        {
            let guard = state.lock().unwrap();
            let (decided_at, outcome): (Option<i64>, Option<String>) = guard
                .conn()
                .query_row(
                    "SELECT decided_at, outcome FROM pending WHERE id = 'hold-row'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(outcome.as_deref(), Some("denied"));
            assert!(decided_at.is_some(), "resolution stamped");
        }

        // Idempotent: second resolve and unknown id are Ok, no state change.
        let again =
            resolve_pending(&state.lock().unwrap(), &audit, "hold-row", ApprovalOutcome::Approved)
                .unwrap();
        assert!(!again);
        let absent =
            resolve_pending(&state.lock().unwrap(), &audit, "no-such-id", ApprovalOutcome::Approved)
                .unwrap();
        assert!(!absent);
        {
            let guard = state.lock().unwrap();
            let outcome: String = guard
                .conn()
                .query_row(
                    "SELECT outcome FROM pending WHERE id = 'hold-row'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(outcome, "denied", "first decision stands");
        }
    }

    #[tokio::test]
    async fn sweep_on_init_resolves_all_unresolved_rows_fail_closed() {
        let dir = unique_tmp_dir("restart");
        let db_path = dir.join("state.db");
        let (audit_path, audit) = temp_audit("restart");

        // "Previous run": an expired hold and a fresh, unexpired one, both
        // unresolved — neither has a live waiter after the restart.
        {
            let state = State::open(db_path.to_str().unwrap()).unwrap();
            for (id, created_ms, timeout_ms) in [
                ("hold-expired", now_ms().saturating_sub(10_000), 1_000u64),
                ("hold-fresh", now_ms(), APPROVAL_TIMEOUT_SECS * 1000),
            ] {
                let payload = PendingPayload {
                    agent: "claude-code".to_string(),
                    repo: "acme/widget".to_string(),
                    class: "push".to_string(),
                    timeout_ms,
                    created_ms,
                    detail: Value::Null,
                };
                state
                    .conn()
                    .execute(
                        "INSERT INTO pending(id, kind, payload_json, created_at, decided_at, outcome)
                         VALUES (?1, 'approval.push', ?2, ?3, NULL, NULL)",
                        rusqlite::params![
                            id,
                            serde_json::to_string(&payload).unwrap(),
                            (created_ms / 1000) as i64
                        ],
                    )
                    .unwrap();
            }
        }

        // "Restart": backend init sweeps every unresolved hold, expired or
        // not — the waiter died, so no decision can resurrect either.
        let state = Arc::new(Mutex::new(
            State::open(db_path.to_str().unwrap()).unwrap(),
        ));
        let backend =
            StubBackend::new(state.clone(), audit.clone(), ApprovalOutcome::Approved).unwrap();

        {
            let guard = state.lock().unwrap();
            let (_, _, expired) = row_state(&guard, "hold-expired");
            let (_, _, fresh) = row_state(&guard, "hold-fresh");
            assert_eq!(expired, Some(ApprovalOutcome::TimedOut), "fail closed");
            assert_eq!(
                fresh, Some(ApprovalOutcome::TimedOut),
                "unexpired hold is orphaned by the restart too"
            );
            // A post-restart control-plane approve must be a no-op: the
            // sweep already decided the row (phantom approval prevented).
            let phantom =
                resolve_pending(&guard, &audit, "hold-fresh", ApprovalOutcome::Approved).unwrap();
            assert!(!phantom, "resolved row is not resolvable after restart");
        }

        // The expired hold's wait concludes immediately as TimedOut.
        let outcome = backend.wait("hold-expired").await;
        assert_eq!(outcome, ApprovalOutcome::TimedOut);

        // Sweeping an already-swept store resolves nothing further.
        let swept = {
            let guard = state.lock().unwrap();
            sweep_unresolved_pending(&guard, &audit).unwrap()
        };
        assert_eq!(swept, 0, "idempotent");

        let lines = audit_lines(&audit_path);
        let timed_out: Vec<_> = lines
            .iter()
            .filter(|l| l["decision"] == "timed_out")
            .collect();
        assert_eq!(
            timed_out.len(),
            2,
            "both restart-resolved holds are audited: {lines:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn wait_wakes_on_cross_task_resolve() {
        let state = shared_state();
        let (_audit_path, audit) = temp_audit("cross-task");
        // 5s window: neither the deadline (5s) nor the stub auto decision
        // (1s) may win — only the external resolve at ~100ms can.
        let backend = Arc::new(
            StubBackend::new(state.clone(), audit.clone(), ApprovalOutcome::Approved).unwrap(),
        );
        backend
            .present(&test_req("hold-cross", Duration::from_secs(5)))
            .await
            .unwrap();

        let resolver = backend.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            resolver
                .resolve("hold-cross", ApprovalOutcome::Denied)
                .await
                .unwrap();
        });

        let started = Instant::now();
        let outcome = backend.wait("hold-cross").await;
        let elapsed = started.elapsed();
        assert_eq!(outcome, ApprovalOutcome::Denied);
        assert!(
            elapsed < Duration::from_millis(900),
            "polling must wake on the external resolve: {elapsed:?}"
        );
    }

    #[test]
    fn payload_round_trips_push_and_api_details() {
        let state = shared_state();
        let (_audit_path, audit) = temp_audit("payload");
        let guard = state.lock().unwrap();

        let push = test_req("req-push", Duration::from_secs(300));
        persist_request(&guard, &push).unwrap();
        let api = ApprovalRequest {
            id: "req-api".to_string(),
            agent: "claude-code".to_string(),
            repo: "acme/widget".to_string(),
            class: ActionClass::ForcePush,
            detail: ApprovalDetail::Api {
                method: "DELETE".to_string(),
                path: "/repos/acme/widget/hooks/1".to_string(),
                body_snippet: "{}".to_string(),
                body_sha: "ab12cd34".to_string(),
            },
            timeout: Duration::from_millis(1500),
        };
        persist_request(&guard, &api).unwrap();

        let push_row: (String, String) = guard
            .conn()
            .query_row(
                "SELECT kind, payload_json FROM pending WHERE id = 'req-push'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(push_row.0, "approval.push");
        let push_payload: serde_json::Value = serde_json::from_str(&push_row.1).unwrap();
        assert_eq!(push_payload["agent"], "claude-code");
        assert_eq!(push_payload["repo"], "acme/widget");
        assert_eq!(push_payload["class"], "push");
        assert_eq!(push_payload["timeout_ms"], 300_000);
        assert_eq!(
            push_payload["detail"]["updates"][0]["old"],
            "0".repeat(40)
        );
        assert_eq!(
            push_payload["detail"]["updates"][0]["new"],
            "5f2e91ab12cd34ef56ab78cd90ef12ab34cd56ef"
        );
        assert_eq!(push_payload["detail"]["updates"][0]["refname"], "refs/heads/main");
        assert_eq!(push_payload["detail"]["commits"][0], "add feature");

        let api_row: (String, String) = guard
            .conn()
            .query_row(
                "SELECT kind, payload_json FROM pending WHERE id = 'req-api'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(api_row.0, "approval.api");
        let api_payload: serde_json::Value = serde_json::from_str(&api_row.1).unwrap();
        assert_eq!(api_payload["class"], "force_push");
        assert_eq!(api_payload["timeout_ms"], 1500);
        assert_eq!(api_payload["detail"]["method"], "DELETE");
        assert_eq!(api_payload["detail"]["path"], "/repos/acme/widget/hooks/1");
        assert_eq!(api_payload["detail"]["body_snippet"], "{}");
        assert_eq!(api_payload["detail"]["body_sha"], "ab12cd34");

        // Timeout metadata is what wait races; a stored row must parse back.
        let parsed: PendingPayload = serde_json::from_str(&push_row.1).unwrap();
        assert_eq!(parsed.timeout_ms, 300_000);
        let _ = audit; // audit unused in this test
    }
}
