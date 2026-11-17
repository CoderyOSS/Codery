//! Machine-token custody (GATE-004): the machine account's classic PAT is
//! the crown jewel. It is stored ONLY as AES-256-GCM ciphertext in the
//! `kv` table; the encryption key is derived from `GATE_TOKEN_ENC_KEY`
//! (never stored alongside, never in the database). Decryption happens in
//! memory, in the request path only, and the token is never logged.
//!
//! Key derivation: SHA-256 over the raw env bytes. This accepts either a
//! hex string or an arbitrary passphrase — both are hashed the same way,
//! so an operator just has to stay consistent with the value they set.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::state::State;

/// kv key holding `base64(nonce(12) || ciphertext)`.
pub const KV_CT: &str = "machine_token_ct";

const NONCE_LEN: usize = 12;

/// 32-byte AES-256 key from the env value: SHA-256 of the raw bytes, so
/// hex-encoded or passphrase-style `GATE_TOKEN_ENC_KEY` values both work.
pub fn derive_key(enc_key: &str) -> [u8; 32] {
    Sha256::digest(enc_key.as_bytes()).into()
}

/// Encrypted-at-rest custody for the machine credential, layered on the
/// shared SQLite state.
pub struct MachineTokenStore<'a> {
    state: &'a State,
    key: [u8; 32],
}

impl<'a> MachineTokenStore<'a> {
    pub fn new(state: &'a State, enc_key: &str) -> Self {
        Self {
            state,
            key: derive_key(enc_key),
        }
    }

    /// Encrypt `plaintext` under a fresh random 12-byte nonce and persist
    /// `base64(nonce || ciphertext)` in the kv table.
    pub fn store(&self, plaintext: &str) -> anyhow::Result<()> {
        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|e| anyhow::anyhow!("custody cipher init failed: {e}"))?;
        let mut nonce_bytes = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce_bytes)
            .map_err(|e| anyhow::anyhow!("entropy source failed: {e}"))?;
        let nonce = Nonce::try_from(&nonce_bytes[..]).expect("12-byte nonce");
        let ciphertext = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|e| anyhow::anyhow!("custody encryption failed: {e}"))?;
        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ciphertext);
        self.state
            .kv_set(KV_CT, &base64::engine::general_purpose::STANDARD.encode(&blob))
    }

    /// Decrypt the stored token. `Ok(None)` when nothing is stored; `Err`
    /// when decryption fails — a wrong `GATE_TOKEN_ENC_KEY` must NEVER
    /// look like "no token".
    pub fn load(&self) -> anyhow::Result<Option<String>> {
        let Some(b64) = self.state.kv_get(KV_CT)? else {
            return Ok(None);
        };
        let blob = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| anyhow::anyhow!("stored custody blob is corrupt: {e}"))?;
        if blob.len() <= NONCE_LEN {
            anyhow::bail!("stored custody blob is truncated");
        }
        let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|e| anyhow::anyhow!("custody cipher init failed: {e}"))?;
        let nonce = Nonce::try_from(nonce_bytes).map_err(|_| {
            anyhow::anyhow!("stored custody blob has a malformed nonce")
        })?;
        let plaintext = cipher
            .decrypt(&nonce, ciphertext)
            .map_err(|_| {
                anyhow::anyhow!("custody decrypt failed (wrong GATE_TOKEN_ENC_KEY?)")
            })?;
        String::from_utf8(plaintext)
            .map(Some)
            .map_err(|e| anyhow::anyhow!("stored token is not utf-8: {e}"))
    }

    /// Erase the stored token (`/disconnect` path).
    pub fn erase(&self) -> anyhow::Result<()> {
        self.state.kv_delete(KV_CT)?;
        Ok(())
    }

    /// Whether custody currently holds a ciphertext (no decryption).
    pub fn has_token(&self) -> bool {
        self.state.kv_get(KV_CT).map(|v| v.is_some()).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use crate::state::State;

    use super::*;

    const KEY_A: &str = "0123456789abcdef";
    const KEY_B: &str = "fedcba9876543210";
    const TOKEN: &str = "ghp_16charsEXAMPLEtoken00";

    fn mem_state() -> State {
        State::open(":memory:").unwrap()
    }

    #[test]
    fn store_load_roundtrip() {
        let s = mem_state();
        let store = MachineTokenStore::new(&s, KEY_A);
        assert!(!store.has_token(), "empty custody");
        store.store(TOKEN).unwrap();
        assert!(store.has_token(), "token present after store");
        assert_eq!(store.load().unwrap().as_deref(), Some(TOKEN));
    }

    #[test]
    fn load_without_token_is_none_not_error() {
        let s = mem_state();
        let store = MachineTokenStore::new(&s, KEY_A);
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn erase_removes_the_token() {
        let s = mem_state();
        let store = MachineTokenStore::new(&s, KEY_A);
        store.store(TOKEN).unwrap();
        store.erase().unwrap();
        assert!(!store.has_token());
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn wrong_key_is_err_not_none() {
        let s = mem_state();
        MachineTokenStore::new(&s, KEY_A).store(TOKEN).unwrap();
        // A different GATE_TOKEN_ENC_KEY must surface as an ERROR (auth
        // failure), never silently as "no token stored".
        let other = MachineTokenStore::new(&s, KEY_B);
        assert!(other.load().is_err(), "wrong key must be an error");
        assert_eq!(
            MachineTokenStore::new(&s, KEY_A).load().unwrap().as_deref(),
            Some(TOKEN),
            "right key still decrypts"
        );
    }

    #[test]
    fn stored_value_is_ciphertext_never_plaintext() {
        let s = mem_state();
        MachineTokenStore::new(&s, KEY_A).store(TOKEN).unwrap();
        let b64 = s.kv_get("machine_token_ct").unwrap().expect("blob");
        assert!(
            !b64.contains(TOKEN),
            "kv value must not contain the plaintext token"
        );
        let blob = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .expect("base64 blob");
        assert!(
            !blob
                .windows(TOKEN.len())
                .any(|w| w == TOKEN.as_bytes()),
            "raw stored bytes must never contain the plaintext bytes"
        );
    }

    #[test]
    fn fresh_nonce_per_store() {
        let s = mem_state();
        let store = MachineTokenStore::new(&s, KEY_A);
        store.store(TOKEN).unwrap();
        let first = s.kv_get("machine_token_ct").unwrap().unwrap();
        store.store(TOKEN).unwrap();
        let second = s.kv_get("machine_token_ct").unwrap().unwrap();
        assert_ne!(first, second, "random 12-byte nonce per encryption");
    }

    #[test]
    fn key_derivation_is_stable_and_32_bytes() {
        let a1 = derive_key(KEY_A);
        let a2 = derive_key(KEY_A);
        let b = derive_key(KEY_B);
        assert_eq!(a1, a2, "deterministic derivation");
        assert_ne!(a1, b, "different key material derives differently");
        assert_eq!(a1.len(), 32, "AES-256 key size");
        // Hex or raw string: both are hashed from the raw bytes, so either
        // representation works as long as the operator stays consistent.
        let as_hex = derive_key(&hex(KEY_A.as_bytes()));
        assert_eq!(a1, derive_key(KEY_A));
        assert_ne!(a1, as_hex, "distinct material derives distinct keys");
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
