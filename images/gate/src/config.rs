use std::env;

use anyhow::Context;

/// Default cap on proxied request bodies (512 MiB), used when
/// `GATE_MAX_BODY_BYTES` is unset, blank or unparsable.
pub const DEFAULT_MAX_BODY_BYTES: usize = 536_870_912;

/// Default registry sweep cadence in seconds (30 min, GATE-010), used when
/// `GATE_REGISTRY_SWEEP_SECS` is unset, blank or unparsable.
pub const DEFAULT_REGISTRY_SWEEP_SECS: u64 = 30 * 60;

/// Default grant-flow poll cadence (5 s, GATE-010) for
/// `GATE_FLOW_POLL_SECS`.
pub const DEFAULT_FLOW_POLL_SECS: u64 = 5;
/// Default grant-flow invitation reminder delay (1 h, GATE-010) for
/// `GATE_FLOW_REMINDER_SECS`.
pub const DEFAULT_FLOW_REMINDER_SECS: u64 = 3600;
/// Default grant-flow give-up window (7 d — GitHub invitations expire
/// after 7 days) for `GATE_FLOW_GIVE_UP_SECS`.
pub const DEFAULT_FLOW_GIVE_UP_SECS: u64 = 7 * 24 * 3600;

/// Default elevation batching window (~10 s, GATE-010 §8) for
/// `GATE_ELEVATION_BATCH_SECS`: concurrent role gaps inside the window
/// present as ONE combined consent message.
pub const DEFAULT_ELEVATION_BATCH_SECS: u64 = 10;
/// Default elevation silence before the ONE reminder (10 min, GATE-010 §9)
/// for `GATE_ELEVATION_REMINDER_SECS`.
pub const DEFAULT_ELEVATION_REMINDER_SECS: u64 = 600;
/// Default elevation no-progress expiry (24 h, GATE-010 §9) for
/// `GATE_ELEVATION_EXPIRE_SECS`.
pub const DEFAULT_ELEVATION_EXPIRE_SECS: u64 = 24 * 3600;

/// Parse a seconds override; unset/blank/garbage/zero ⇒ the default (a
/// misparse must never disable a cadence).
fn secs_from(v: Option<&str>, default: u64) -> u64 {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .filter(|s| *s > 0)
        .unwrap_or(default)
}

/// Parse `GATE_REGISTRY_SWEEP_SECS`; unset/blank/garbage/zero ⇒ the 30-min
/// default (a misparse must not disable the sweep).
fn sweep_secs_from(v: Option<&str>) -> u64 {
    secs_from(v, DEFAULT_REGISTRY_SWEEP_SECS)
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
    /// ntfy control channel (GATE-013): broker URL + control/answers
    /// topics + access token. All-or-nothing — a partial set refuses to
    /// boot. Broker auth IS the operator identity (deny-all + token).
    pub ntfy: Option<NtfyConfig>,
    /// Registry sweep cadence in seconds (`GATE_REGISTRY_SWEEP_SECS`,
    /// default 1800).
    pub registry_sweep_secs: u64,
    /// Grant-flow poll cadence in seconds (`GATE_FLOW_POLL_SECS`,
    /// default 5, GATE-010).
    pub flow_poll_secs: u64,
    /// Grant-flow invitation reminder delay in seconds
    /// (`GATE_FLOW_REMINDER_SECS`, default 3600).
    pub flow_reminder_secs: u64,
    /// Grant-flow give-up window in seconds (`GATE_FLOW_GIVE_UP_SECS`,
    /// default 7 days — GitHub invitations expire after 7 days).
    pub flow_give_up_secs: u64,
    /// Elevation batching window in seconds (`GATE_ELEVATION_BATCH_SECS`,
    /// default 10, GATE-010).
    pub elevation_batch_secs: u64,
    /// Elevation reminder delay in seconds (`GATE_ELEVATION_REMINDER_SECS`,
    /// default 600 — the ONE reminder, GATE-010).
    pub elevation_reminder_secs: u64,
    /// Elevation no-progress expiry in seconds
    /// (`GATE_ELEVATION_EXPIRE_SECS`, default 24 h, GATE-010).
    pub elevation_expire_secs: u64,
}

/// GATE-013 ntfy control-channel configuration. Present (`Some`) only
/// when ALL FOUR keys are set; any ntfy key present demands a COMPLETE
/// configuration — partial configs refuse to boot, never half-wire a
/// channel. Broker auth (deny-all + token) IS the operator identity.
#[derive(Debug, Clone)]
pub struct NtfyConfig {
    /// Broker base URL — MUST be byte-identical to the address the app
    /// subscribes with (ntfy matches the push signal on it).
    pub url: String,
    /// The control topic: asks, notices, and operator compositions.
    pub topic: String,
    /// The answers topic: action buttons POST here; the phone does NOT
    /// subscribe to it.
    pub answers_topic: String,
    /// Broker access token (`tk_…`).
    pub token: String,
}

/// A required key "counts as set" when trimmed non-empty.
fn trimmed_non_empty(v: Option<&str>) -> Option<&str> {
    v.map(str::trim).filter(|s| !s.is_empty())
}

fn ntfy_config_from(
    url: Option<&str>,
    topic: Option<&str>,
    answers_topic: Option<&str>,
    token: Option<&str>,
) -> anyhow::Result<Option<NtfyConfig>> {
    let u = trimmed_non_empty(url);
    let t = trimmed_non_empty(topic);
    let a = trimmed_non_empty(answers_topic);
    let k = trimmed_non_empty(token);
    let any_present = u.is_some() || t.is_some() || a.is_some() || k.is_some();
    if !any_present {
        return Ok(None);
    }
    let mut missing = Vec::new();
    if u.is_none() {
        missing.push("NTFY_URL");
    }
    if t.is_none() {
        missing.push("NTFY_TOPIC");
    }
    if a.is_none() {
        missing.push("NTFY_ANSWERS_TOPIC");
    }
    if k.is_none() {
        missing.push("NTFY_TOKEN");
    }
    if !missing.is_empty() {
        anyhow::bail!(
            "GATE-013: partial ntfy configuration — {} must also be set",
            missing.join(", ")
        );
    }
    // Trailing slash would make the push-signal host differ from the app's
    // subscription address (ntfy's #1 self-hosting gotcha) — normalize.
    Ok(Some(NtfyConfig {
        url: u.unwrap().trim_end_matches('/').to_string(),
        topic: t.unwrap().to_string(),
        answers_topic: a.unwrap().to_string(),
        token: k.unwrap().to_string(),
    }))
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
            ntfy: ntfy_config_from(
                env::var("NTFY_URL").ok().as_deref(),
                env::var("NTFY_TOPIC").ok().as_deref(),
                env::var("NTFY_ANSWERS_TOPIC").ok().as_deref(),
                env::var("NTFY_TOKEN").ok().as_deref(),
            )?,
            registry_sweep_secs: sweep_secs_from(
                env::var("GATE_REGISTRY_SWEEP_SECS").ok().as_deref(),
            ),
            flow_poll_secs: secs_from(
                env::var("GATE_FLOW_POLL_SECS").ok().as_deref(),
                DEFAULT_FLOW_POLL_SECS,
            ),
            flow_reminder_secs: secs_from(
                env::var("GATE_FLOW_REMINDER_SECS").ok().as_deref(),
                DEFAULT_FLOW_REMINDER_SECS,
            ),
            flow_give_up_secs: secs_from(
                env::var("GATE_FLOW_GIVE_UP_SECS").ok().as_deref(),
                DEFAULT_FLOW_GIVE_UP_SECS,
            ),
            elevation_batch_secs: secs_from(
                env::var("GATE_ELEVATION_BATCH_SECS").ok().as_deref(),
                DEFAULT_ELEVATION_BATCH_SECS,
            ),
            elevation_reminder_secs: secs_from(
                env::var("GATE_ELEVATION_REMINDER_SECS").ok().as_deref(),
                DEFAULT_ELEVATION_REMINDER_SECS,
            ),
            elevation_expire_secs: secs_from(
                env::var("GATE_ELEVATION_EXPIRE_SECS").ok().as_deref(),
                DEFAULT_ELEVATION_EXPIRE_SECS,
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

    #[test]
    fn flow_secs_defaults_are_the_gate_010_values() {
        assert_eq!(DEFAULT_FLOW_POLL_SECS, 5);
        assert_eq!(DEFAULT_FLOW_REMINDER_SECS, 3600);
        assert_eq!(DEFAULT_FLOW_GIVE_UP_SECS, 7 * 24 * 3600);
        // Elevation values (task 8), pinned verbatim from GATE-010 §8–9.
        assert_eq!(DEFAULT_ELEVATION_BATCH_SECS, 10);
        assert_eq!(DEFAULT_ELEVATION_REMINDER_SECS, 600);
        assert_eq!(DEFAULT_ELEVATION_EXPIRE_SECS, 24 * 3600);
    }

    #[test]
    fn flow_secs_parse_or_default_like_the_sweep() {
        for (default, env, want) in [
            (DEFAULT_FLOW_POLL_SECS, None, DEFAULT_FLOW_POLL_SECS),
            (DEFAULT_FLOW_POLL_SECS, Some(""), DEFAULT_FLOW_POLL_SECS),
            (DEFAULT_FLOW_POLL_SECS, Some("junk"), DEFAULT_FLOW_POLL_SECS),
            (DEFAULT_FLOW_POLL_SECS, Some("0"), DEFAULT_FLOW_POLL_SECS),
            (DEFAULT_FLOW_REMINDER_SECS, Some(" 2 "), 2),
            (DEFAULT_FLOW_GIVE_UP_SECS, Some("600"), 600),
        ] {
            assert_eq!(secs_from(env, default), want, "{env:?}");
        }
    }

    // --- GATE-013: ntfy control channel (all-or-nothing) ---

    const NU: Option<&str> = Some("https://ntfy.example.com/");
    const NT: Option<&str> = Some("gate-control-7f3a");
    const NA: Option<&str> = Some("gate-answers-91bd");
    const NK: Option<&str> = Some("tk_gate_XXXXXXXX");

    #[test]
    fn ntfy_config_parses_and_normalizes_url() {
        let cfg = ntfy_config_from(NU, NT, NA, NK)
            .expect("complete ntfy config must load")
            .expect("all keys set ⇒ Some");
        assert_eq!(cfg.url, "https://ntfy.example.com", "trailing slash normalized");
        assert_eq!(cfg.topic, "gate-control-7f3a");
        assert_eq!(cfg.answers_topic, "gate-answers-91bd");
        assert_eq!(cfg.token, "tk_gate_XXXXXXXX");
    }

    #[test]
    fn ntfy_config_absent_when_no_keys() {
        let cfg = ntfy_config_from(None, None, None, None)
            .expect("no keys at all is a valid no-ntfy config");
        assert!(cfg.is_none());
    }

    #[test]
    fn partial_ntfy_config_refuses() {
        // token-only: missing url + both topics
        let err = ntfy_config_from(None, None, None, NK)
            .expect_err("token without url/topics must refuse");
        let msg = err.to_string();
        assert!(msg.contains("NTFY_URL"), "{msg}");
        assert!(msg.contains("NTFY_TOPIC"), "{msg}");
        assert!(msg.contains("NTFY_ANSWERS_TOPIC"), "{msg}");

        // url-only
        let err = ntfy_config_from(NU, None, None, None)
            .expect_err("url alone must refuse");
        assert!(err.to_string().contains("NTFY_TOKEN"));
    }

    #[test]
    fn blank_values_count_as_unset_for_presence_but_url_slash_still_trims() {
        let cfg = ntfy_config_from(Some("https://ntfy.example.com"), NT, NA, NK)
            .expect("complete config")
            .expect("all keys set ⇒ Some");
        assert_eq!(cfg.url, "https://ntfy.example.com");
    }
}
