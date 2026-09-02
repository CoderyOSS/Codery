//! s6 tooling output parsers + the shell snippets used via `docker exec`.
//! All s6 binaries are addressed by absolute path (/command/...) — the
//! container PATH seen by `docker exec` is not guaranteed to include it.

use serde::Serialize;

/// Services s6-overlay runs for its own plumbing — never report as apps.
pub const S6_INTERNAL_SERVICES: [&str; 3] = [
    "s6-linux-init-shutdownd",
    "s6rc-fdholder",
    "s6rc-oneshot-runner",
];

/// Bash one-liner printing `name|up pid uptime_secs` per supervised service.
/// Run as: docker exec <container> bash -c "$SVSTAT_BATCH_CMD".
pub const SVSTAT_BATCH_CMD: &str = r#"for d in /run/service/*/; do n=$(basename "$d"); case "$n" in s6-linux-init-shutdownd|s6rc-fdholder|s6rc-oneshot-runner) continue;; esac; echo "$n|$(/command/s6-svstat -o up,pid,updownfor "$d")"; done"#;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SvcStat {
    pub name: String,
    pub up: bool,
    pub pid: i64,
    pub uptime_secs: u64,
}

pub fn parse_svstat_batch(output: &str) -> Vec<SvcStat> {
    output
        .lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once('|')?;
            if name.is_empty() {
                return None;
            }
            let mut it = rest.split_whitespace();
            let up = matches!(it.next()?, "true");
            let pid: i64 = it.next()?.parse().ok()?;
            let uptime_secs: u64 = it.next()?.parse().ok()?;
            Some(SvcStat {
                name: name.to_string(),
                up,
                pid,
                uptime_secs,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_up_service() {
        let out = "web|true 1234 42\n";
        let stats = parse_svstat_batch(out);
        assert_eq!(stats, vec![SvcStat { name: "web".into(), up: true, pid: 1234, uptime_secs: 42 }]);
    }

    #[test]
    fn parses_down_service() {
        let out = "worker|false -1 7\n";
        let stats = parse_svstat_batch(out);
        assert_eq!(stats[0].up, false);
        assert_eq!(stats[0].pid, -1);
        assert_eq!(stats[0].uptime_secs, 7);
    }

    #[test]
    fn skips_malformed_lines() {
        let out = "garbage\nweb|true 1 2\n|no-name\nbroken|true x 2\n";
        let stats = parse_svstat_batch(out);
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].name, "web");
    }

    #[test]
    fn handles_empty_output() {
        assert!(parse_svstat_batch("").is_empty());
    }
}
