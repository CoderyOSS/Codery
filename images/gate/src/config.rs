use std::env;

use anyhow::Context;

/// Gate runtime configuration (GATE-009 env keys).
pub struct Config {
    pub public_url: String,
    pub token_enc_key: String,
    pub listen_addr: String,
    pub policy_path: String,
    pub state_path: String,
    pub audit_path: String,
    /// Machine account username (GATE-004): the Basic-auth login used for
    /// git passthrough to upstream, `base64("{login}:{token}")`.
    pub machine_login: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            // May be empty ⇒ fallback/internal mode: agents use the
            // codery-net alias and the page still renders (GATE-001).
            public_url: env::var("GATE_PUBLIC_URL").unwrap_or_default(),
            token_enc_key: env::var("GATE_TOKEN_ENC_KEY")
                .context("GATE_TOKEN_ENC_KEY is required")?,
            listen_addr: env::var("GATE_LISTEN_ADDR")
                .unwrap_or_else(|_| "0.0.0.0:8080".into()),
            policy_path: env::var("GATE_POLICY_PATH")
                .unwrap_or_else(|_| "/data/policy.toml".into()),
            state_path: env::var("GATE_STATE_PATH")
                .unwrap_or_else(|_| "/data/gate.db".into()),
            audit_path: env::var("GATE_AUDIT_PATH")
                .unwrap_or_else(|_| "/data/audit.jsonl".into()),
            machine_login: env::var("GATE_MACHINE_LOGIN")
                .context("GATE_MACHINE_LOGIN is required")?,
        })
    }

    /// Host the instruction page renders into its commands:
    /// `GATE_PUBLIC_URL` when configured, else the internal codery-net
    /// alias `http://gitauthgate:<port>` (GATE-001 fallback mode).
    pub fn gate_host(&self) -> String {
        gate_host(&self.public_url, &self.listen_addr)
    }
}

/// Derive the page host from a (possibly empty) public URL and the listen
/// address. Pure so tests can pin both shapes.
pub fn gate_host(public_url: &str, listen_addr: &str) -> String {
    let trimmed = public_url.trim();
    if !trimmed.is_empty() {
        return trimmed.trim_end_matches('/').to_string();
    }
    let port = listen_addr
        .rsplit_once(':')
        .map(|(_, p)| p)
        .unwrap_or("")
        .trim();
    let port = if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
        "8080"
    } else {
        port
    };
    format!("http://gitauthgate:{port}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_host_uses_public_url_when_set() {
        assert_eq!(
            gate_host("https://gag.example.com", "0.0.0.0:8080"),
            "https://gag.example.com"
        );
        assert_eq!(
            gate_host("https://gag.example.com/", "0.0.0.0:8080"),
            "https://gag.example.com",
            "trailing slash trimmed"
        );
    }

    #[test]
    fn gate_host_falls_back_to_internal_alias_when_public_url_empty() {
        // GATE-001 fallback: empty GATE_PUBLIC_URL ⇒ internal codery-net
        // alias with the port from GATE_LISTEN_ADDR.
        assert_eq!(gate_host("", "0.0.0.0:8080"), "http://gitauthgate:8080");
        assert_eq!(gate_host("  ", "127.0.0.1:9999"), "http://gitauthgate:9999");
        assert_eq!(gate_host("", "0.0.0.0"), "http://gitauthgate:8080");
    }
}
