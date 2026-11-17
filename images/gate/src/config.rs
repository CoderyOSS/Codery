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
            public_url: env::var("GATE_PUBLIC_URL")
                .context("GATE_PUBLIC_URL is required")?,
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
}
