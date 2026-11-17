//! Audit log (GATE-009): JSON lines, one event per line, appended to a file.
//! Every recorded entry passes through `redact()` so secret-shaped material
//! never reaches disk.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use serde::Serialize;

/// One audit event. `ts` is unix epoch seconds; `agent` and `approver` are
/// optional; `basis` carries the human-readable reason for the decision.
#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub ts: i64,
    pub agent: Option<String>,
    pub repo: String,
    pub action: String,
    pub decision: String,
    pub basis: String,
    pub approver: Option<String>,
}

impl AuditEntry {
    /// Replace the value of every text field that contains a secret-shaped
    /// string (`ghp_` tokens, `Authorization` headers, or a 40-hex token)
    /// with `[REDACTED]`.
    pub fn redact(mut self) -> Self {
        self.agent = self.agent.map(|s| redact_str(&s));
        self.repo = redact_str(&self.repo);
        self.action = redact_str(&self.action);
        self.decision = redact_str(&self.decision);
        self.basis = redact_str(&self.basis);
        self.approver = self.approver.map(|s| redact_str(&s));
        self
    }
}

/// True if `c` is an ASCII lowercase hex digit (`[0-9a-f]`).
fn is_lower_hex(c: u8) -> bool {
    c.is_ascii_digit() || (b'a'..=b'f').contains(&c)
}

/// True if `s` contains a run of exactly 40 lowercase hex characters whose
/// neighbours (if any) are not hex digits — i.e. a bare 40-hex token such as
/// a classic GitHub PAT or a git sha, without matching long hex-ish words
/// (a 41+ character hex run has no delimited 40-window).
fn contains_40_hex_token(s: &str) -> bool {
    let b = s.as_bytes();
    let n = b.len();
    if n < 40 {
        return false;
    }
    for i in 0..=(n - 40) {
        if !b[i..i + 40].iter().all(|&c| is_lower_hex(c)) {
            continue;
        }
        let left_clear = i == 0 || !is_lower_hex(b[i - 1]);
        let right_clear = i + 40 == n || !is_lower_hex(b[i + 40]);
        if left_clear && right_clear {
            return true;
        }
    }
    false
}

fn redact_str(s: &str) -> String {
    if s.contains("ghp_") || s.contains("Authorization") || contains_40_hex_token(s) {
        "[REDACTED]".to_string()
    } else {
        s.to_string()
    }
}

/// Append-only JSONL audit sink.
pub struct Audit {
    file: Mutex<std::fs::File>,
}

impl Audit {
    /// Open (creating if needed) the JSONL file in append mode.
    ///
    /// Panics if the file cannot be created: the gate must not run without
    /// an audit log. `record` likewise fails loudly rather than silently
    /// dropping events.
    pub fn open(path: impl AsRef<Path>) -> Audit {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path.as_ref())
            .expect("audit log must be openable");
        Audit {
            file: Mutex::new(file),
        }
    }

    /// Redact and append one event as a single JSON line.
    pub fn record(&self, entry: AuditEntry) {
        let entry = entry.redact();
        let mut line = serde_json::to_string(&entry).expect("audit entry must serialize");
        line.push('\n');
        let mut file = self.file.lock().expect("audit lock must be poison-free");
        file.write_all(line.as_bytes()).expect("audit write");
        file.flush().expect("audit flush");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audit_entry(basis: &str) -> AuditEntry {
        AuditEntry {
            ts: 1_700_000_000,
            agent: Some("claude-code".to_string()),
            repo: "acme/widget".to_string(),
            action: "push".to_string(),
            decision: "approve".to_string(),
            basis: basis.to_string(),
            approver: None,
        }
    }

    /// Record one entry into a fresh temp file and return its single line.
    fn recorded_line(basis: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "gate-audit-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.jsonl");
        let audit = Audit::open(&path);
        audit.record(audit_entry(basis));
        let contents = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(contents.lines().count(), 1, "one line per event");
        contents
    }

    #[test]
    fn authorization_bearer_ghp_token_is_redacted_on_disk() {
        let line = recorded_line("Authorization: Bearer ghp_secret123");
        assert!(line.contains("[REDACTED]"), "line: {line}");
        assert!(!line.contains("ghp_secret123"), "line: {line}");
        assert!(!line.contains("Authorization"), "line: {line}");
    }

    #[test]
    fn every_text_field_is_redacted_when_secret_shaped() {
        let e = AuditEntry {
            ts: 1,
            agent: Some("sent ghp_agenttok in header".to_string()),
            repo: "acme/widget".to_string(),
            action: "push".to_string(),
            decision: "approve".to_string(),
            basis: "clean".to_string(),
            approver: Some("Authorization: Bearer ghp_othertok".to_string()),
        }
        .redact();
        assert_eq!(e.agent.as_deref(), Some("[REDACTED]"));
        assert_eq!(e.approver.as_deref(), Some("[REDACTED]"));
        assert_eq!(e.basis, "clean");
        // None fields stay None.
        let e2 = audit_entry("plain").redact();
        assert!(e2.approver.is_none());
        assert_eq!(e2.agent.as_deref(), Some("claude-code"));
    }

    #[test]
    fn bare_40_hex_sha_is_redacted() {
        let sha = "5f2e91ab12cd34ef56ab78cd90ef12ab34cd56ef";
        assert_eq!(sha.len(), 40);
        // Embedded in prose, delimited by non-hex boundaries.
        let line = recorded_line(&format!("non-ff update to {sha} on main"));
        assert!(line.contains("[REDACTED]"), "line: {line}");
        assert!(!line.contains(sha), "line: {line}");
        // Standalone value.
        let line = recorded_line(sha);
        assert!(line.contains("[REDACTED]"), "line: {line}");
    }

    #[test]
    fn prose_and_39_hex_survive() {
        let prose = "approval granted by repo owner via setup flow";
        let line = recorded_line(prose);
        assert!(line.contains(prose), "line: {line}");
        assert!(!line.contains("[REDACTED]"), "line: {line}");

        let hex39 = "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6a7b8c9d";
        assert_eq!(hex39.len(), 39);
        let line = recorded_line(&format!("short sha {hex39} truncated"));
        assert!(!line.contains("[REDACTED]"), "line: {line}");
        assert!(line.contains(hex39), "line: {line}");
    }

    #[test]
    fn long_hexish_word_does_not_false_positive() {
        // 60 lowercase-hex chars in one word: every 40-window has hex on
        // both sides, so nothing is delimited — must survive.
        let long = "ab".repeat(30);
        let line = recorded_line(&format!("artifact {long} stored"));
        assert!(!line.contains("[REDACTED]"), "line: {line}");
    }

    #[test]
    fn jsonl_lines_parse_with_expected_fields() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "gate-audit-jsonl-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.jsonl");
        let audit = Audit::open(&path);
        audit.record(audit_entry("first event"));
        audit.record(audit_entry("second event"));
        let contents = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "one line per event");
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["ts"], 1_700_000_000);
        assert_eq!(first["agent"], "claude-code");
        assert_eq!(first["repo"], "acme/widget");
        assert_eq!(first["action"], "push");
        assert_eq!(first["decision"], "approve");
        assert_eq!(first["basis"], "first event");
        assert!(first["approver"].is_null());
        for key in ["ts", "agent", "repo", "action", "decision", "basis", "approver"] {
            assert!(first.get(key).is_some(), "missing key {key}");
        }
    }
}
