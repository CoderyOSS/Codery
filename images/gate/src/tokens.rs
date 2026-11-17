//! Agent token store (GATE-003): opaque, revocable bearer tokens for
//! authenticating coding agents to the gate itself. Tokens are 32 random
//! bytes base62-encoded and returned exactly once; only the SHA-256 hash
//! is persisted, and validation compares digests in constant time.

use anyhow::Result;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::audit::{Audit, AuditEntry};
use crate::state::{self, State};

/// The authenticated agent; injected as a request extension by the auth
/// middleware once a token validates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentIdentity {
    pub name: String,
}

/// Token management layered on the shared SQLite state, with token events
/// mirrored into the audit log.
pub struct TokenStore<'a> {
    state: &'a State,
    audit: &'a Audit,
}

/// One `/token list` row: metadata only — a name and unix-second
/// timestamps. No hash, no raw token: secret material never leaves the
/// store through a listing (and Debug is safe to derive for the same
/// reason).
#[derive(Debug, Clone)]
pub struct TokenInfo {
    pub name: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

impl<'a> TokenStore<'a> {
    pub fn new(state: &'a State, audit: &'a Audit) -> Self {
        Self { state, audit }
    }

    /// Mint a fresh token for `name`. The raw token (32 random bytes,
    /// base62) is returned exactly once; only its SHA-256 hash is stored.
    /// Minting for an existing name rotates the token, invalidating the
    /// previous one.
    pub fn mint(&self, name: &str) -> Result<String> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|e| anyhow::anyhow!("entropy source failed: {e}"))?;
        let raw = base62_encode(&bytes);
        let hash = hex_encode(&sha256(raw.as_bytes()));
        self.state.conn().execute(
            "INSERT INTO agent_tokens(name, hash, created_at, last_used_at)
             VALUES (?1, ?2, ?3, NULL)
             ON CONFLICT(name) DO UPDATE SET
                hash = excluded.hash,
                created_at = excluded.created_at,
                last_used_at = NULL",
            rusqlite::params![name, hash, state::now()],
        )?;
        self.audit.record(AuditEntry {
            ts: state::now(),
            agent: Some(name.to_string()),
            repo: "-".to_string(),
            action: "token.mint".to_string(),
            decision: "allow".to_string(),
            basis: format!("token '{name}' minted (sha-256 hash stored, raw returned once)"),
            approver: None,
        });
        Ok(raw)
    }

    /// Revoke `name`'s token. Returns whether a token existed.
    pub fn revoke(&self, name: &str) -> bool {
        let deleted = self
            .state
            .conn()
            .execute("DELETE FROM agent_tokens WHERE name = ?1", [name])
            .expect("token revoke must not fail");
        let existed = deleted > 0;
        self.audit.record(AuditEntry {
            ts: state::now(),
            agent: Some(name.to_string()),
            repo: "-".to_string(),
            action: "token.revoke".to_string(),
            decision: "allow".to_string(),
            basis: if existed {
                format!("token '{name}' revoked")
            } else {
                format!("no token named '{name}' existed")
            },
            approver: None,
        });
        existed
    }

    /// All tokens, oldest first. Reads only metadata columns — never the
    /// hash, and there is no raw token to read.
    pub fn list(&self) -> Vec<TokenInfo> {
        let Ok(mut stmt) = self.state.conn().prepare(
            "SELECT name, created_at, last_used_at
             FROM agent_tokens ORDER BY created_at, name",
        ) else {
            return Vec::new();
        };
        let mapped = stmt.query_map([], |r| {
            Ok(TokenInfo {
                name: r.get(0)?,
                created_at: r.get(1)?,
                last_used_at: r.get(2)?,
            })
        });
        match mapped {
            Ok(rows) => rows.collect::<Result<Vec<_>, _>>().unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    /// Whether a token row exists under `name` — lets a mint caller tell a
    /// fresh mint from a rotation before it happens.
    pub fn exists(&self, name: &str) -> bool {
        self.state
            .conn()
            .query_row(
                "SELECT 1 FROM agent_tokens WHERE name = ?1",
                [name],
                |r| r.get::<_, i64>(0),
            )
            .is_ok()
    }

    /// Validate a raw token. The stored hex hash is decoded and compared
    /// against the candidate digest in constant time; the raw token (and
    /// any string form of either digest) never enters a variable-time
    /// comparison. On success `last_used_at` is stamped and the agent
    /// identity returned.
    pub fn validate(&self, raw: &str) -> Option<AgentIdentity> {
        let digest = sha256(raw.as_bytes());
        let rows: Vec<(String, String)> = {
            let mut stmt = self
                .state
                .conn()
                .prepare("SELECT name, hash FROM agent_tokens")
                .ok()?;
            let mapped = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .ok()?;
            mapped.collect::<Result<Vec<_>, _>>().ok()?
        };
        for (name, hash) in rows {
            let Some(stored) = hex_decode(&hash) else {
                continue;
            };
            if stored.len() != digest.len() {
                continue;
            }
            if bool::from(stored.as_slice().ct_eq(digest.as_slice())) {
                // Metadata stamp only; must not gate an already-validated
                // agent.
                let _ = self.state.conn().execute(
                    "UPDATE agent_tokens SET last_used_at = ?1 WHERE name = ?2",
                    rusqlite::params![state::now(), name],
                );
                return Some(AgentIdentity { name });
            }
        }
        None
    }
}

/// SHA-256 digest of `data`.
fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// Lowercase hex encoding.
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Hex decoding; rejects odd length or non-hex characters.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Base62 alphabet: digits, upper, lower — URL- and git-credential-safe.
const BASE62: &[u8; 62] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Big-endian base62 encoding of a byte string (bignum divmod by 62).
fn base62_encode(bytes: &[u8]) -> String {
    let mut num = bytes.to_vec();
    let mut out: Vec<u8> = Vec::new();
    while num.iter().any(|&b| b != 0) {
        let mut rem: u32 = 0;
        for digit in num.iter_mut() {
            let cur = (rem << 8) | u32::from(*digit);
            *digit = (cur / 62) as u8;
            rem = cur % 62;
        }
        out.push(BASE62[rem as usize]);
    }
    if out.is_empty() {
        out.push(BASE62[0]);
    }
    out.reverse();
    String::from_utf8(out).expect("base62 alphabet is ascii")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// :memory: state plus a temp-file audit sink; the temp dir is removed
    /// on drop.
    struct Fixture {
        state: State,
        audit: Audit,
        audit_path: PathBuf,
    }

    impl Fixture {
        fn open() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "gate-tokens-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let audit_path = dir.join("audit.jsonl");
            Self {
                state: State::open(":memory:").unwrap(),
                audit: Audit::open(&audit_path),
                audit_path,
            }
        }

        fn store(&self) -> TokenStore<'_> {
            TokenStore::new(&self.state, &self.audit)
        }

        fn token_row(&self, name: &str) -> (String, Option<i64>) {
            self.state
                .conn()
                .query_row(
                    "SELECT hash, last_used_at FROM agent_tokens WHERE name = ?1",
                    [name],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(dir) = self.audit_path.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    #[test]
    fn mint_then_validate_roundtrip() {
        let f = Fixture::open();
        let raw = f.store().mint("claude-code").unwrap();
        let who = f.store().validate(&raw).expect("token must validate");
        assert_eq!(who.name, "claude-code");
    }

    #[test]
    fn validate_rejects_wrong_token() {
        let f = Fixture::open();
        f.store().mint("claude-code").unwrap();
        assert!(f.store().validate("definitely-not-the-token").is_none());
        assert!(f.store().validate("").is_none());
        assert!(
            f.store().validate("claude-code").is_none(),
            "the agent name itself is not a token"
        );
    }

    #[test]
    fn revoke_then_validate_is_none() {
        let f = Fixture::open();
        let raw = f.store().mint("claude-code").unwrap();
        assert!(f.store().revoke("claude-code"), "first revoke hits");
        assert!(f.store().validate(&raw).is_none(), "revoked token dies");
        assert!(!f.store().revoke("claude-code"), "second revoke misses");
        assert!(!f.store().revoke("never-minted"), "unknown agent misses");
    }

    #[test]
    fn store_holds_64_hex_hash_never_raw() {
        let f = Fixture::open();
        let raw = f.store().mint("claude-code").unwrap();
        let (hash, created_at): (String, i64) = f
            .state
            .conn()
            .query_row(
                "SELECT hash, created_at FROM agent_tokens WHERE name = ?1",
                ["claude-code"],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(hash.len(), 64, "sha-256 hex digest length");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(hash, raw, "raw token must never be stored");
        assert_eq!(hash, hex_encode(&sha256(raw.as_bytes())));
        assert!(created_at > 0, "created_at stamped");
    }

    #[test]
    fn mint_rotates_invalidating_previous_token() {
        let f = Fixture::open();
        let old = f.store().mint("claude-code").unwrap();
        let new = f.store().mint("claude-code").unwrap();
        assert_ne!(old, new, "fresh randomness per mint");
        assert!(f.store().validate(&old).is_none(), "old token dead");
        assert!(f.store().validate(&new).is_some(), "new token live");
    }

    #[test]
    fn validate_stamps_last_used_at() {
        let f = Fixture::open();
        let raw = f.store().mint("claude-code").unwrap();
        let before = f.token_row("claude-code").1;
        assert_eq!(before, None, "unvalidated token has no last_used_at");
        f.store().validate(&raw).unwrap();
        let after = f.token_row("claude-code").1;
        assert!(after.unwrap() > 0, "last_used_at stamped on success");
    }

    #[test]
    fn token_is_base62_shape_of_32_random_bytes() {
        let f = Fixture::open();
        let raw = f.store().mint("claude-code").unwrap();
        assert!(
            raw.chars().all(|c| c.is_ascii_alphanumeric()),
            "base62 alphabet only: {raw}"
        );
        // 256 bits is 42-44 base62 digits.
        assert!((42..=44).contains(&raw.chars().count()), "len {raw}");
        let other = f.store().mint("other-agent").unwrap();
        assert_ne!(raw, other, "per-mint randomness");
    }

    #[test]
    fn mint_and_revoke_are_audited_without_raw_material() {
        let f = Fixture::open();
        let raw = f.store().mint("claude-code").unwrap();
        f.store().revoke("claude-code");
        let contents = std::fs::read_to_string(&f.audit_path).unwrap();
        assert!(contents.contains("token.mint"), "contents: {contents}");
        assert!(contents.contains("token.revoke"), "contents: {contents}");
        assert!(
            contents.contains("claude-code"),
            "audit entries carry the token NAME: {contents}"
        );
        assert!(
            !contents.contains(&raw),
            "raw token must never reach the audit log: {contents}"
        );
    }

    #[test]
    fn list_returns_metadata_without_hash_or_raw() {
        let f = Fixture::open();
        let raw = f.store().mint("claude-code").unwrap();
        let second = f.store().mint("other-agent").unwrap();
        f.store().validate(&raw);
        let rows = f.store().list();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "claude-code");
        assert!(rows[0].created_at > 0);
        assert!(rows[0].last_used_at.is_some(), "validated token stamped");
        assert_eq!(rows[1].name, "other-agent");
        assert_eq!(rows[1].last_used_at, None, "unused token unstamped");
        let (_, hash): (String, String) = f
            .state
            .conn()
            .query_row(
                "SELECT name, hash FROM agent_tokens WHERE name = 'claude-code'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        for row in &rows {
            let rendered = format!("{row:?}");
            assert!(!rendered.contains(&hash), "hash never renders: {rendered}");
            assert!(!rendered.contains(&raw), "raw never renders: {rendered}");
            assert!(!rendered.contains(&second), "raw never renders: {rendered}");
        }
    }

    #[test]
    fn exists_tracks_mint_revoke_and_rotation() {
        let f = Fixture::open();
        assert!(!f.store().exists("claude-code"));
        f.store().mint("claude-code").unwrap();
        assert!(f.store().exists("claude-code"));
        f.store().revoke("claude-code");
        assert!(!f.store().exists("claude-code"));
    }
}
