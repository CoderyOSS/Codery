// Host metrics collection for the deploy console: memory, PSI pressure,
// OOM counter, and per-process worst-offenders. All inputs are read from
// /proc and /sys/fs/cgroup directly — no subprocesses.

use serde::Serialize;

// ── Health thresholds (single tuning point) ──────────────────────────────────
const RED_AVAIL_PCT: f64 = 10.0;
const YELLOW_AVAIL_PCT: f64 = 20.0;
const RED_SWAP_USED_MB: f64 = 2048.0;   // >50% of a 4G swap
const YELLOW_SWAP_USED_MB: f64 = 100.0;
const RED_PSI_MEM_AVG10: f64 = 25.0;
const YELLOW_PSI_MEM_AVG10: f64 = 10.0;
const YELLOW_PSI_MEM_AVG60: f64 = 25.0;

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MemoryInfo {
    pub total_mb: f64,
    pub available_mb: f64,
    pub cached_mb: f64,
    pub buffers_mb: f64,
    pub swap_total_mb: f64,
    pub swap_free_mb: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PsiWindow { pub avg10: f64, pub avg60: f64, pub avg300: f64 }

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PsiResource { pub some: PsiWindow, pub full: Option<PsiWindow> }

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Psi { pub cpu: PsiResource, pub memory: PsiResource, pub io: PsiResource }

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Health { pub status: String, pub reasons: Vec<String> }

// ── Parsers ──────────────────────────────────────────────────────────────────

fn kb_to_mb(kb: f64) -> f64 { kb / 1024.0 }

/// Parse /proc/meminfo (keyed "Field:  N kB" lines). Missing fields → 0.
pub fn parse_meminfo(text: &str) -> MemoryInfo {
    let get = |key: &str| -> f64 {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix(key) {
                let kb: f64 = rest
                    .trim_start_matches(':')
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0.0);
                return kb_to_mb(kb);
            }
        }
        0.0
    };
    MemoryInfo {
        total_mb:     get("MemTotal"),
        available_mb: get("MemAvailable"),
        cached_mb:    get("Cached"),
        buffers_mb:   get("Buffers"),
        swap_total_mb: get("SwapTotal"),
        swap_free_mb:  get("SwapFree"),
    }
}

/// Parse one /proc/pressure file: `some avg10=… avg60=… avg300=… total=…`
/// and optionally a `full …` line. CPU files have no `full` line.
pub fn parse_pressure(text: &str) -> Option<PsiResource> {
    let parse_line = |line: &str| -> Option<PsiWindow> {
        let mut w = PsiWindow { avg10: 0.0, avg60: 0.0, avg300: 0.0 };
        let mut seen = false;
        for part in line.split_whitespace().skip(1) {
            if let Some((k, v)) = part.split_once('=') {
                match k {
                    "avg10"  => { w.avg10  = v.parse().ok()?; seen = true; }
                    "avg60"  => { w.avg60  = v.parse().ok()?; seen = true; }
                    "avg300" => { w.avg300 = v.parse().ok()?; seen = true; }
                    _ => {}
                }
            }
        }
        if seen { Some(w) } else { None }
    };
    let mut some: Option<PsiWindow> = None;
    let mut full: Option<PsiWindow> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("some ") {
            some = parse_line(&format!("some {rest}"));
        } else if let Some(rest) = line.strip_prefix("full ") {
            full = parse_line(&format!("full {rest}"));
        }
    }
    some.map(|some| PsiResource { some, full })
}

/// Parse /sys/fs/cgroup/memory.events → the `oom_kill` counter (0 if absent).
pub fn parse_memory_events(text: &str) -> u64 {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("oom_kill ") {
            return rest.trim().parse().unwrap_or(0);
        }
    }
    0
}

// ── Health derivation ────────────────────────────────────────────────────────

/// Memory-derived health. Red outranks yellow. PSI-absent degrades to
/// memory/swap-only with a "PSI unavailable" reason.
pub fn derive_health(mem: &MemoryInfo, psi: Option<&Psi>, prev_oom: Option<u64>, cur_oom: u64) -> Health {
    let mut red: Vec<String> = Vec::new();
    let mut yellow: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    if mem.total_mb > 0.0 {
        let avail_pct = mem.available_mb / mem.total_mb * 100.0;
        if avail_pct < RED_AVAIL_PCT {
            red.push(format!("MemAvailable {:.0}% of total", avail_pct));
        } else if avail_pct < YELLOW_AVAIL_PCT {
            yellow.push(format!("MemAvailable {:.0}% of total", avail_pct));
        }
    }

    let swap_used = (mem.swap_total_mb - mem.swap_free_mb).max(0.0);
    if mem.swap_total_mb > 0.0 {
        if swap_used > RED_SWAP_USED_MB {
            red.push(format!("swap in use {:.1}G", swap_used / 1024.0));
        } else if swap_used > YELLOW_SWAP_USED_MB {
            yellow.push(format!("swap in use {:.0}M", swap_used));
        }
    }

    let mem_psi = psi.map(|p| &p.memory.some);
    match mem_psi {
        Some(w) => {
            if w.avg10 > RED_PSI_MEM_AVG10 {
                red.push(format!("memory pressure avg10 {:.0}%", w.avg10));
            } else if w.avg10 > YELLOW_PSI_MEM_AVG10 || w.avg60 > YELLOW_PSI_MEM_AVG60 {
                yellow.push(format!("memory pressure avg10 {:.0}% avg60 {:.0}%", w.avg10, w.avg60));
            }
        }
        None => notes.push("PSI unavailable".to_string()),
    }

    if let Some(prev) = prev_oom {
        if cur_oom > prev {
            red.push(format!("OOM kill detected ({} since boot)", cur_oom));
        }
    }

    if !red.is_empty() {
        red.extend(notes);
        Health { status: "red".into(), reasons: red }
    } else if !yellow.is_empty() {
        yellow.extend(notes);
        Health { status: "yellow".into(), reasons: yellow }
    } else {
        Health { status: "green".into(), reasons: notes }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    const MEMINFO: &str = "\
MemTotal:       8055024 kB
MemFree:         288100 kB
MemAvailable:    3120004 kB
Buffers:          120332 kB
Cached:          1830220 kB
SwapCached:            0 kB
SwapTotal:       4194300 kB
SwapFree:        2994188 kB
";

    const MEMINFO_NOSWAP: &str = "\
MemTotal:       8055024 kB
MemFree:         288100 kB
MemAvailable:     712004 kB
Buffers:          120332 kB
Cached:          1830220 kB
SwapTotal:             0 kB
SwapFree:              0 kB
";

    const PRESSURE_MEM: &str = "some avg10=1.23 avg60=0.45 avg300=0.12 total=123456789\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
    const PRESSURE_CPU: &str = "some avg10=3.21 avg60=1.10 avg300=0.40 total=987654321\n";
    const MEM_EVENTS: &str = "anon 12\nfile 340\noom_kill 13\noom_victim 5\n";

    #[test]
    fn meminfo_parses_mb() {
        let m = parse_meminfo(MEMINFO);
        assert_eq!(m.total_mb, 8055024.0 / 1024.0);
        assert_eq!(m.available_mb, 3120004.0 / 1024.0);
        assert_eq!(m.cached_mb, 1830220.0 / 1024.0);
        assert_eq!(m.buffers_mb, 120332.0 / 1024.0);
        assert_eq!(m.swap_total_mb, 4194300.0 / 1024.0);
        assert_eq!(m.swap_free_mb, 2994188.0 / 1024.0);
    }

    #[test]
    fn meminfo_tolerates_missing_lines() {
        let m = parse_meminfo("MemTotal: 100 kB\n");
        assert_eq!(m.total_mb, 100.0 / 1024.0);
        assert_eq!(m.available_mb, 0.0);
        assert_eq!(m.swap_total_mb, 0.0);
    }

    #[test]
    fn meminfo_without_swap_parses_zero_swap() {
        let m = parse_meminfo(MEMINFO_NOSWAP);
        assert_eq!(m.total_mb, 8055024.0 / 1024.0);
        assert_eq!(m.swap_total_mb, 0.0);
        assert_eq!(m.swap_free_mb, 0.0);
    }

    #[test]
    fn pressure_parses_some_and_full() {
        let p = parse_pressure(PRESSURE_MEM).expect("parse");
        assert_eq!(p.some.avg10, 1.23);
        assert_eq!(p.some.avg60, 0.45);
        assert_eq!(p.some.avg300, 0.12);
        let full = p.full.expect("cpu-less files have full");
        assert_eq!(full.avg10, 0.0);
    }

    #[test]
    fn pressure_without_full_is_ok() {
        let p = parse_pressure(PRESSURE_CPU).expect("parse");
        assert_eq!(p.some.avg10, 3.21);
        assert!(p.full.is_none(), "CPU PSI has no full line");
    }

    #[test]
    fn pressure_garbage_is_none() {
        assert!(parse_pressure("nonsense\n").is_none());
        assert!(parse_pressure("").is_none());
    }

    #[test]
    fn memory_events_counts_oom_kills() {
        assert_eq!(parse_memory_events(MEM_EVENTS), 13);
        assert_eq!(parse_memory_events("file 1\n"), 0);
        assert_eq!(parse_memory_events(""), 0);
    }

    // ── Health derivation truth table ────────────────────────────────────

    fn mem(avail_pct: f64, swap_used_mb: f64) -> MemoryInfo {
        MemoryInfo {
            total_mb: 7862.0,
            available_mb: 7862.0 * avail_pct / 100.0,
            cached_mb: 0.0,
            buffers_mb: 0.0,
            swap_total_mb: 4096.0,
            swap_free_mb: 4096.0 - swap_used_mb,
        }
    }

    fn psi_mem(avg10: f64, avg60: f64) -> Psi {
        Psi {
            cpu: PsiResource { some: PsiWindow { avg10: 0.0, avg60: 0.0, avg300: 0.0 }, full: None },
            memory: PsiResource { some: PsiWindow { avg10, avg60, avg300: 0.0 }, full: None },
            io: PsiResource { some: PsiWindow { avg10: 0.0, avg60: 0.0, avg300: 0.0 }, full: None },
        }
    }

    #[test]
    fn healthy_host_is_green() {
        let h = derive_health(&mem(50.0, 0.0), Some(&psi_mem(0.0, 0.0)), Some(13), 13);
        assert_eq!(h.status, "green");
        assert!(h.reasons.is_empty());
    }

    #[test]
    fn low_memory_is_red() {
        let h = derive_health(&mem(9.0, 0.0), Some(&psi_mem(0.0, 0.0)), Some(13), 13);
        assert_eq!(h.status, "red");
        assert!(h.reasons.iter().any(|r| r.contains("MemAvailable")), "reasons: {:?}", h.reasons);
    }

    #[test]
    fn heavy_swap_is_red() {
        let h = derive_health(&mem(50.0, 3000.0), Some(&psi_mem(0.0, 0.0)), Some(13), 13);
        assert_eq!(h.status, "red");
    }

    #[test]
    fn high_mem_pressure_avg10_is_red() {
        let h = derive_health(&mem(50.0, 0.0), Some(&psi_mem(30.0, 0.0)), Some(13), 13);
        assert_eq!(h.status, "red");
    }

    #[test]
    fn oom_increase_is_red() {
        let h = derive_health(&mem(50.0, 0.0), Some(&psi_mem(0.0, 0.0)), Some(13), 14);
        assert_eq!(h.status, "red");
        assert!(h.reasons.iter().any(|r| r.contains("OOM kill")), "reasons: {:?}", h.reasons);
    }

    #[test]
    fn moderate_memory_is_yellow() {
        let h = derive_health(&mem(15.0, 0.0), Some(&psi_mem(0.0, 0.0)), Some(13), 13);
        assert_eq!(h.status, "yellow");
    }

    #[test]
    fn small_swap_use_is_yellow() {
        let h = derive_health(&mem(50.0, 150.0), Some(&psi_mem(0.0, 0.0)), Some(13), 13);
        assert_eq!(h.status, "yellow");
    }

    #[test]
    fn moderate_mem_pressure_is_yellow() {
        let h = derive_health(&mem(50.0, 0.0), Some(&psi_mem(12.0, 0.0)), Some(13), 13);
        assert_eq!(h.status, "yellow");
        let h2 = derive_health(&mem(50.0, 0.0), Some(&psi_mem(0.0, 30.0)), Some(13), 13);
        assert_eq!(h2.status, "yellow");
    }

    #[test]
    fn red_outranks_yellow() {
        let h = derive_health(&mem(5.0, 150.0), Some(&psi_mem(0.0, 0.0)), Some(13), 13);
        assert_eq!(h.status, "red");
    }

    #[test]
    fn psi_absent_still_derives_from_memory() {
        let h = derive_health(&mem(9.0, 0.0), None, Some(13), 13);
        assert_eq!(h.status, "red");
        assert!(h.reasons.iter().any(|r| r.contains("PSI unavailable")));
        let g = derive_health(&mem(50.0, 0.0), None, Some(13), 13);
        assert_eq!(g.status, "green");
    }

    #[test]
    fn no_swap_never_triggers_swap_reasons() {
        let mut m = mem(50.0, 0.0);
        m.swap_total_mb = 0.0;
        m.swap_free_mb = 0.0;
        let h = derive_health(&m, Some(&psi_mem(0.0, 0.0)), Some(13), 13);
        assert_eq!(h.status, "green");
        assert!(!h.reasons.iter().any(|r| r.contains("swap")));
    }
}
