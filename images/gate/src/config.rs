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
    /// Matrix control channel (GATE-012), mutually exclusive with the
    /// Telegram one — `matrix_config_from` refuses dual/partial configs.
    pub matrix: Option<MatrixConfig>,
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

/// GATE-012 Matrix control-channel configuration. Present (`Some`) only
/// when the required keys are set; any Matrix key present demands a
/// COMPLETE configuration — partial configs refuse to boot, never
/// half-wire a channel.
#[derive(Debug, Clone)]
pub struct MatrixConfig {
    pub homeserver_url: String,
    pub access_token: String,
    pub room_id: String,
    /// mxids allowed to command the gate; empty ⇒ drop everything
    /// (fail closed, mirrors the Telegram allowlist rule).
    pub user_allowlist: Vec<String>,
}

/// Parse `MATRIX_USER_ALLOWLIST`: comma-separated mxids, trimmed, blanks
/// skipped. There is no "bad entry" notion at this layer — mxid shape is
/// validated by the transport (GATE-012).
fn mxids_from(v: Option<&str>) -> Vec<String> {
    v.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect()
    })
    .unwrap_or_default()
}

/// GATE-012 channel resolution. A key "counts as set" when trimmed
/// non-empty — including the allowlist (a blank allowlist value counts as
/// UNSET for presence: composes export it set-but-empty everywhere; when
/// the trio IS complete, a blank allowlist is a valid empty fail-closed
/// list). Rules, in order:
/// no Matrix key ⇒ `Ok(None)`; any Matrix key alongside
/// `TELEGRAM_BOT_TOKEN` ⇒ refuse (mutually exclusive channels); any
/// Matrix key without all three required ones ⇒ refuse naming the
/// missing keys. The error strings name the offending env keys so the
/// operator can fix the `.env` without reading code.
/// A required key "counts as set" when trimmed non-empty.
fn trimmed_non_empty(v: Option<&str>) -> Option<&str> {
    v.map(str::trim).filter(|s| !s.is_empty())
}

fn matrix_config_from(
    telegram_token: Option<&str>,
    homeserver: Option<&str>,
    access_token: Option<&str>,
    room_id: Option<&str>,
    user_allowlist: Option<&str>,
) -> anyhow::Result<Option<MatrixConfig>> {
    let hs = trimmed_non_empty(homeserver);
    let token = trimmed_non_empty(access_token);
    let room = trimmed_non_empty(room_id);
    // Presence rule (C1): a BLANK allowlist counts as unset for presence —
    // composes export `MATRIX_USER_ALLOWLIST=` set-but-empty on every
    // deployment, and a blank value must never make a Telegram/no-channel
    // deployment look Matrix-configured. The VALUE still flows through
    // `mxids_from` when the trio is complete (blank ⇒ empty fail-closed list).
    let allowlist_present =
        user_allowlist.map(str::trim).is_some_and(|s| !s.is_empty());
    let any_present =
        hs.is_some() || token.is_some() || room.is_some() || allowlist_present;
    if !any_present {
        return Ok(None);
    }
    if trimmed_non_empty(telegram_token).is_some() {
        anyhow::bail!(
            "GATE-012: TELEGRAM_BOT_TOKEN and MATRIX_ACCESS_TOKEN are mutually exclusive — \
             configure exactly one control channel"
        );
    }
    let mut missing = Vec::new();
    if hs.is_none() {
        missing.push("MATRIX_HOMESERVER_URL");
    }
    if token.is_none() {
        missing.push("MATRIX_ACCESS_TOKEN");
    }
    if room.is_none() {
        missing.push("MATRIX_ROOM_ID");
    }
    if !missing.is_empty() {
        anyhow::bail!(
            "GATE-012: partial Matrix configuration — {} must also be set",
            missing.join(", ")
        );
    }
    Ok(Some(MatrixConfig {
        homeserver_url: hs.unwrap().to_string(),
        access_token: token.unwrap().to_string(),
        room_id: room.unwrap().to_string(),
        user_allowlist: mxids_from(user_allowlist),
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
            telegram_bot_token: telegram_bot_token_from(
                env::var("TELEGRAM_BOT_TOKEN").ok().as_deref(),
            ),
            telegram_allowlist: allowlist_from(
                env::var("TELEGRAM_ALLOWLIST").ok().as_deref(),
            ),
            telegram_chat_id: chat_id_from(env::var("TELEGRAM_CHAT_ID").ok().as_deref()),
            matrix: matrix_config_from(
                env::var("TELEGRAM_BOT_TOKEN").ok().as_deref(),
                env::var("MATRIX_HOMESERVER_URL").ok().as_deref(),
                env::var("MATRIX_ACCESS_TOKEN").ok().as_deref(),
                env::var("MATRIX_ROOM_ID").ok().as_deref(),
                env::var("MATRIX_USER_ALLOWLIST").ok().as_deref(),
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

    // --- GATE-012: Matrix control channel (mutually exclusive) ---

    const HS: Option<&str> = Some("https://matrix.example.org");
    const TOKEN: Option<&str> = Some("syt_gate_XXXXXXXX");
    const ROOM: Option<&str> = Some("!control:example.org");
    const MXIDS: Option<&str> = Some("@rob:example.org, @ops:example.org ");

    #[test]
    fn matrix_config_parses_all_keys() {
        let cfg = matrix_config_from(None, HS, TOKEN, ROOM, MXIDS)
            .expect("complete matrix config must load");
        let cfg = cfg.expect("all keys set ⇒ Some");
        assert_eq!(cfg.homeserver_url, "https://matrix.example.org");
        assert_eq!(cfg.access_token, "syt_gate_XXXXXXXX");
        assert_eq!(cfg.room_id, "!control:example.org");
        assert_eq!(
            cfg.user_allowlist,
            vec!["@rob:example.org".to_string(), "@ops:example.org".to_string()]
        );
    }

    #[test]
    fn matrix_config_absent_when_no_keys() {
        let cfg = matrix_config_from(None, None, None, None, None)
            .expect("no keys at all is a valid no-matrix config");
        assert!(cfg.is_none());
    }

    #[test]
    fn both_channels_configured_refuses() {
        let err = matrix_config_from(Some("123:abc"), HS, TOKEN, ROOM, None)
            .expect_err("dual channels must refuse to boot");
        let msg = err.to_string();
        assert!(msg.contains("TELEGRAM_BOT_TOKEN"), "{msg}");
        assert!(msg.contains("MATRIX_ACCESS_TOKEN"), "{msg}");
    }

    #[test]
    fn partial_matrix_config_refuses() {
        // token-only: missing homeserver + room
        let err = matrix_config_from(None, None, TOKEN, None, None)
            .expect_err("token without homeserver/room must refuse");
        let msg = err.to_string();
        assert!(msg.contains("MATRIX_HOMESERVER_URL"), "{msg}");
        assert!(msg.contains("MATRIX_ROOM_ID"), "{msg}");

        // homeserver-only
        let err = matrix_config_from(None, HS, None, None, None)
            .expect_err("homeserver alone must refuse");
        assert!(err.to_string().contains("MATRIX_ACCESS_TOKEN"));

        // room-only
        let err = matrix_config_from(None, None, None, ROOM, None)
            .expect_err("room alone must refuse");
        assert!(err.to_string().contains("MATRIX_ACCESS_TOKEN"));
    }

    #[test]
    fn blank_allowlist_is_valid_empty_fail_closed() {
        let cfg = matrix_config_from(None, HS, TOKEN, ROOM, Some(""))
            .expect("blank allowlist is a complete config")
            .expect("all required keys set ⇒ Some");
        assert!(cfg.user_allowlist.is_empty(), "empty allowlist ⇒ drop everything");
    }

    #[test]
    fn blank_allowlist_alone_is_not_a_matrix_config() {
        // C1 regression guard: composes export `MATRIX_USER_ALLOWLIST=`
        // (set-but-empty) for Telegram/no-channel deployments — a blank
        // value must count as UNSET for presence, or every deployment
        // without a full Matrix trio would refuse to boot.
        let cfg = matrix_config_from(None, None, None, None, Some(""))
            .expect("blank allowlist alone must not be a partial config");
        assert!(cfg.is_none(), "no Matrix keys ⇒ no Matrix config");
    }

    #[test]
    fn blank_allowlist_with_telegram_does_not_trigger_exclusivity() {
        let cfg = matrix_config_from(Some("123:abc"), None, None, None, Some(""))
            .expect("blank allowlist beside Telegram must not refuse");
        assert!(cfg.is_none(), "still no Matrix config");
    }
}
