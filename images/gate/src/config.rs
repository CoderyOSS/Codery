use std::env;

use anyhow::Context;

/// Default cap on proxied request bodies (512 MiB), used when
/// `GATE_MAX_BODY_BYTES` is unset, blank or unparsable.
pub const DEFAULT_MAX_BODY_BYTES: usize = 536_870_912;

/// Default registry sweep cadence in seconds (30 min, GATE-010), used when
/// `GATE_REGISTRY_SWEEP_SECS` is unset, blank or unparsable.
pub const DEFAULT_REGISTRY_SWEEP_SECS: u64 = 30 * 60;

/// Parse `GATE_REGISTRY_SWEEP_SECS`; unset/blank/garbage/zero ⇒ the 30-min
/// default (a misparse must not disable the sweep).
fn sweep_secs_from(v: Option<&str>) -> u64 {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .filter(|s| *s > 0)
        .unwrap_or(DEFAULT_REGISTRY_SWEEP_SECS)
}

/// Parse `GATE_MAX_BODY_BYTES`; unset/blank/garbage ⇒ the 512 MiB default
/// (never zero — a misparse must not disable every proxied request).
fn max_body_bytes_from(v: Option<&str>) -> usize {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MAX_BODY_BYTES)
}

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
    /// Cap on proxied request bodies (`GATE_MAX_BODY_BYTES`); over-limit
    /// requests get a 413 before anything is forwarded upstream.
    pub max_body_bytes: usize,
    /// Telegram bot token (`TELEGRAM_BOT_TOKEN`) when set and non-empty —
    /// the only condition under which the control-plane poll loop spawns
    /// (core git/API paths must work without the bot).
    pub telegram_bot_token: Option<String>,
    /// Allowlisted Telegram user ids (`TELEGRAM_ALLOWLIST`, comma-separated
    /// integers; GATE-006). Empty ⇒ the loop (if spawned) drops every
    /// update — fail closed.
    pub telegram_allowlist: Vec<i64>,
    /// Chat id approval asks are delivered to (`TELEGRAM_CHAT_ID`). Unset
    /// with a bot token configured ⇒ asks cannot be delivered: approval
    /// `present` errors and holds are denied — fail closed.
    pub telegram_chat_id: Option<i64>,
    /// Registry sweep cadence in seconds (`GATE_REGISTRY_SWEEP_SECS`,
    /// default 1800).
    pub registry_sweep_secs: u64,
}

/// Bot token as `Some` only when set and non-blank; blank ⇒ `None` (no
/// control plane spawns).
fn telegram_bot_token_from(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Parse the GATE-006 allowlist: comma-separated integer user ids. Blank
/// or absent ⇒ empty (which the transport treats as "drop everything").
/// Unparsable entries are skipped individually — a typo narrows the
/// allowlist rather than disabling or widening it (fail closed).
fn allowlist_from(v: Option<&str>) -> Vec<i64> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.split(',')
                .filter_map(|part| part.trim().parse::<i64>().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Parse `TELEGRAM_CHAT_ID`: trimmed integer, or `None` when unset, blank
/// or unparsable (asks stay undeliverable — the backend fails closed).
fn chat_id_from(v: Option<&str>) -> Option<i64> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
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
            max_body_bytes: max_body_bytes_from(
                env::var("GATE_MAX_BODY_BYTES").ok().as_deref(),
            ),
            telegram_bot_token: telegram_bot_token_from(
                env::var("TELEGRAM_BOT_TOKEN").ok().as_deref(),
            ),
            telegram_allowlist: allowlist_from(
                env::var("TELEGRAM_ALLOWLIST").ok().as_deref(),
            ),
            telegram_chat_id: chat_id_from(env::var("TELEGRAM_CHAT_ID").ok().as_deref()),
            registry_sweep_secs: sweep_secs_from(
                env::var("GATE_REGISTRY_SWEEP_SECS").ok().as_deref(),
            ),
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

    #[test]
    fn max_body_bytes_defaults_when_unset_or_garbage() {
        assert_eq!(max_body_bytes_from(None), DEFAULT_MAX_BODY_BYTES);
        assert_eq!(max_body_bytes_from(Some("")), DEFAULT_MAX_BODY_BYTES);
        assert_eq!(max_body_bytes_from(Some("   ")), DEFAULT_MAX_BODY_BYTES);
        assert_eq!(max_body_bytes_from(Some("junk")), DEFAULT_MAX_BODY_BYTES);
        assert_eq!(
            max_body_bytes_from(Some("512MiB")),
            DEFAULT_MAX_BODY_BYTES,
            "plain integer parsing only"
        );
    }

    #[test]
    fn max_body_bytes_parses_plain_integers() {
        assert_eq!(max_body_bytes_from(Some("1048576")), 1_048_576);
        assert_eq!(max_body_bytes_from(Some(" 2048 ")), 2048);
        assert_eq!(max_body_bytes_from(Some("0")), 0, "explicit zero honored");
    }

    #[test]
    fn default_max_body_bytes_is_512_mib() {
        assert_eq!(DEFAULT_MAX_BODY_BYTES, 536_870_912);
    }

    #[test]
    fn telegram_bot_token_none_when_unset_or_blank() {
        assert_eq!(telegram_bot_token_from(None), None);
        assert_eq!(telegram_bot_token_from(Some("")), None);
        assert_eq!(telegram_bot_token_from(Some("   ")), None);
        assert_eq!(
            telegram_bot_token_from(Some(" 123:abc ")),
            Some("123:abc".to_string())
        );
    }

    #[test]
    fn allowlist_parses_comma_separated_ids() {
        assert_eq!(allowlist_from(Some("42, 7 ,100")), vec![42, 7, 100]);
        assert_eq!(allowlist_from(Some("-5")), vec![-5]);
        assert_eq!(allowlist_from(None), Vec::<i64>::new());
        assert_eq!(allowlist_from(Some("  ")), Vec::<i64>::new());
    }

    #[test]
    fn allowlist_skips_bad_entries_and_stays_fail_closed() {
        assert_eq!(allowlist_from(Some("42,oops,7")), vec![42, 7]);
        assert_eq!(allowlist_from(Some("oops")), Vec::<i64>::new(), "garbage ⇒ empty");
        assert_eq!(allowlist_from(Some(",,")), Vec::<i64>::new());
    }

    #[test]
    fn telegram_chat_id_parses_or_fails_closed_to_none() {
        assert_eq!(chat_id_from(None), None);
        assert_eq!(chat_id_from(Some("")), None);
        assert_eq!(chat_id_from(Some("  ")), None);
        assert_eq!(chat_id_from(Some("junk")), None);
        assert_eq!(chat_id_from(Some(" -100200 ")), Some(-100_200));
        assert_eq!(chat_id_from(Some("42")), Some(42));
    }

    #[test]
    fn sweep_secs_defaults_when_unset_or_garbage_or_zero() {
        assert_eq!(sweep_secs_from(None), DEFAULT_REGISTRY_SWEEP_SECS);
        assert_eq!(sweep_secs_from(Some("")), DEFAULT_REGISTRY_SWEEP_SECS);
        assert_eq!(sweep_secs_from(Some("   ")), DEFAULT_REGISTRY_SWEEP_SECS);
        assert_eq!(sweep_secs_from(Some("junk")), DEFAULT_REGISTRY_SWEEP_SECS);
        assert_eq!(
            sweep_secs_from(Some("0")),
            DEFAULT_REGISTRY_SWEEP_SECS,
            "zero would disable the sweep — never honored"
        );
    }

    #[test]
    fn sweep_secs_parses_plain_integers_and_defaults_to_30_min() {
        assert_eq!(DEFAULT_REGISTRY_SWEEP_SECS, 1800);
        assert_eq!(sweep_secs_from(Some("1800")), 1800);
        assert_eq!(sweep_secs_from(Some(" 5 ")), 5, "test override");
        assert_eq!(sweep_secs_from(Some("3600")), 3600);
    }
}
