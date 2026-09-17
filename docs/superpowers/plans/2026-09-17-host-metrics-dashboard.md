# Host Metrics Dashboard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add live host health metering (memory, PSI pressure, OOM counter, per-process worst-offenders) to the codery-ci deploy console at `ci.rancidgrandmas.online`.

**Architecture:** New `host_metrics.rs` module parses `/proc` directly (no subprocesses); a 3 s tokio task in `ui::serve` broadcasts JSON snapshots on a new broadcast channel; `ui.rs` serves `GET /api/metrics` + `GET /api/metrics/stream` (SSE); the React frontend adds a `HostPanel` fed by a second EventSource. Shared `Arc<Mutex<MetricsState>>` keeps CPU/oom deltas consistent across the timer task and HTTP handlers.

**Tech Stack:** Rust (tokio, axum 0.8, bollard, serde, libc), React 18 + Vite 6 + TS (single-file build embedded via `include_str!`), GitHub Actions (release-orchestrator → build-orchestrator).

**Spec:** `docs/superpowers/specs/2026-09-16-host-metrics-dashboard-design.md` (visual design pending OpenDesign mock — this plan implements the functional panel with existing tokens; restyling later is CSS-only)

## Global Constraints

- No new crates beyond what's in `system/orchestrator/Cargo.toml` today (libc, tokio, axum, bollard, serde, serde_json, anyhow, futures-util are already available).
- No subprocesses for metrics collection — read `/proc` and `/sys/fs/cgroup` directly.
- `psi` is `null` when `/proc/pressure/*` is missing; never an error.
- Health derivation constants live in one place (`host_metrics.rs`) as named consts.
- JSON field names are snake_case, matching the existing `ServiceStatus` ↔ `Container` TS mirror pattern.
- Rust unit tests must run without Docker (`Docker::connect_with_socket_defaults()` failure = skip, existing pattern).
- **The sandbox has no Rust compiler.** Run `cargo test` in the apps container: `ssh gem@apps "cd /home/gem/projects/Codery/system/orchestrator && sudo nix shell nixpkgs#rustc nixpkgs#cargo -c cargo test"` (first run downloads crates; slow). CI also runs `cross test` in the release workflow.
- Frontend builds with `npm ci && npm run build` in `system/orchestrator/ui` (fallback in sandbox: `bun install && bun run build`; `tsc -b` must pass either way).
- Version: bump to **0.14.0** (new feature, pre-1.0 minor). Release = tag `codery-ci-v0.14.0` → release workflow builds binaries + runs `cross test` → **Build Orchestrator** workflow deploys (downloads latest release → `/opt/codery/codery-ci` → `supervisorctl restart codery-ci-daemon`).
- Every new UI element gets a `data-od-id` attribute (existing convention).

---

### Task 1: `host_metrics.rs` — types, memory/PSI/swap/oom parsers, health derivation

**Files:**
- Create: `system/orchestrator/src/host_metrics.rs`
- Modify: `system/orchestrator/src/main.rs` (add `mod host_metrics;` beside the other `mod` declarations)

**Interfaces:**
- Produces (used by Tasks 2–4):
  - `pub struct MemoryInfo { pub total_mb: f64, pub available_mb: f64, pub cached_mb: f64, pub buffers_mb: f64, pub swap_total_mb: f64, pub swap_free_mb: f64 }`
  - `pub struct PsiWindow { pub avg10: f64, pub avg60: f64, pub avg300: f64 }`
  - `pub struct PsiResource { pub some: PsiWindow, pub full: Option<PsiWindow> }`
  - `pub struct Psi { pub cpu: PsiResource, pub memory: PsiResource, pub io: PsiResource }`
  - `pub struct Health { pub status: String, pub reasons: Vec<String> }`
  - `pub fn parse_meminfo(text: &str) -> MemoryInfo`
  - `pub fn parse_pressure(text: &str) -> Option<PsiResource>` (None if file empty/malformed)
  - `pub fn parse_memory_events(text: &str) -> u64` (oom_kill count, 0 if missing)
  - `pub fn derive_health(mem: &MemoryInfo, psi: Option<&Psi>, prev_oom: Option<u64>, cur_oom: u64) -> Health`
- All structs derive `Debug, Clone, Serialize` (serde) and `PartialEq` (tests).

- [ ] **Step 1: Write failing tests (fixtures + truth table)**

Create `system/orchestrator/src/host_metrics.rs` containing only the test module first:

```rust
// Host metrics collection for the deploy console: memory, PSI pressure,
// OOM counter, and per-process worst-offenders. All inputs are read from
// /proc and /sys/fs/cgroup directly — no subprocesses.

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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `ssh gem@apps "cd /home/gem/projects/Codery/system/orchestrator && sudo nix shell nixpkgs#rustc nixpkgs#cargo -c cargo test --lib host_metrics"`
Expected: compile error — `parse_meminfo`, `derive_health` etc. not defined. (If the apps-container toolchain is unavailable, proceed and rely on `cross test` in Task 6's release run — but prefer fixing the ssh path first; it is documented in AGENTS.md.)

- [ ] **Step 3: Implement parsers + derivation**

Add above the test module in the same file:

```rust
use serde::Serialize;
use std::collections::BTreeMap;

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
        None => yellow.push("PSI unavailable".to_string()),
    }

    if let Some(prev) = prev_oom {
        if cur_oom > prev {
            red.push(format!("OOM kill detected ({} since boot)", cur_oom));
        }
    }

    if !red.is_empty() {
        Health { status: "red".into(), reasons: red }
    } else if !yellow.is_empty() {
        Health { status: "yellow".into(), reasons: yellow }
    } else {
        Health { status: "green".into(), reasons: Vec::new() }
    }
}

// Keeps BTreeMap import used in Task 2 without a separate edit.
#[allow(dead_code)]
type Unused = BTreeMap<String, String>;
```

Remove the `BTreeMap` import + `Unused` alias if Task 2's code ends up importing it properly — check at commit time.

- [ ] **Step 4: Register the module**

In `system/orchestrator/src/main.rs`, beside the existing `mod` lines (e.g. `mod images;`), add:

```rust
mod host_metrics;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `ssh gem@apps "cd /home/gem/projects/Codery/system/orchestrator && sudo nix shell nixpkgs#rustc nixpkgs#cargo -c cargo test --lib host_metrics"`
Expected: all Task 1 tests PASS, no warnings for unused code (the `Unused` alias prevents that).

- [ ] **Step 6: Commit**

```bash
cd /home/gem/projects/Codery && git add system/orchestrator/src/host_metrics.rs system/orchestrator/src/main.rs && git commit -m "feat(ci): host_metrics module — meminfo/PSI/oom parsers + health derivation"
```

---

### Task 2: `host_metrics.rs` — process scanner, CPU deltas, container attribution

**Files:**
- Modify: `system/orchestrator/src/host_metrics.rs`

**Interfaces:**
- Consumes: nothing from Task 1 except module co-location.
- Produces (used by Task 3):
  - `pub struct ProcSample { pub pid: i32, pub ppid: i32, pub state: String, pub comm: String, pub utime: u64, pub stime: u64, pub rss_bytes: u64, pub container_id: Option<String> }`
  - `pub struct ProcSnapshot { pub wall_secs: f64, pub procs: Vec<ProcSample> }`
  - `pub struct TopProcess { pub pid: i32, pub comm: String, pub rss_mb: f64, pub cpu_pct: f64, pub container: String, pub state: String }`
  - `pub fn scan_processes() -> std::io::Result<ProcSnapshot>`
  - `pub fn parse_stat(content: &str) -> Option<(i32, i32, String, u64, u64)>` → (pid, ppid, state, utime, stime); comm may contain spaces/parens.
  - `pub fn parse_statm_rss_bytes(content: &str, page_size: f64) -> u64`
  - `pub fn parse_cgroup_container(content: &str) -> Option<String>` → 64-hex id from `0::/system.slice/docker-<id>.scope`
  - `pub fn top_processes(cur: &ProcSnapshot, prev: Option<&ProcSnapshot>, containers: &std::collections::HashMap<String, String>, n: usize) -> Vec<TopProcess>` — CPU% = Δ(utime+stime)/ticks ÷ Δwall × 100; first sample or unseen pid → 0.0; sorted by rss desc, truncated to `n`; `container` = name via longest-prefix match else `"host"`.

- [ ] **Step 1: Write failing tests**

Append to the test module in `host_metrics.rs`:

```rust
    const STAT: &str = "1234 (opencode serve) S 1 0 0 0 -1 4194560 0 0 0 0 100 50 0 0 20 0 1 0 12345 1 1 18446744073709551615 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1\n";
    const STATM: &str = "100000 197100 50000 1000 0 60000 0\n";
    const CGROUP_IN_CONTAINER: &str = "0::/system.slice/docker-abc123def456abc123def456abc123def456abc123def456abc123def456abc1.scope\n";
    const CGROUP_HOST: &str = "0::/init.scope\n";

    #[test]
    fn stat_parses_with_spaces_in_comm() {
        let (pid, ppid, state, utime, stime) = parse_stat(STAT).expect("parse");
        assert_eq!(pid, 1234);
        assert_eq!(ppid, 1);
        assert_eq!(state, "S");
        assert_eq!(utime, 100);
        assert_eq!(stime, 50);
    }

    #[test]
    fn stat_garbage_is_none() {
        assert!(parse_stat("").is_none());
        assert!(parse_stat("justoneword\n").is_none());
    }

    #[test]
    fn statm_rss_multiplies_page_size() {
        // resident = 2nd field = 197100 pages × 4096
        assert_eq!(parse_statm_rss_bytes(STATM, 4096.0), 197100u64 * 4096);
        assert_eq!(parse_statm_rss_bytes("", 4096.0), 0);
    }

    #[test]
    fn cgroup_container_id_extraction() {
        assert_eq!(
            parse_cgroup_container(CGROUP_IN_CONTAINER).as_deref(),
            Some("abc123def456abc123def456abc123def456abc123def456abc123def456abc1")
        );
        assert_eq!(parse_cgroup_container(CGROUP_HOST), None);
    }

    #[test]
    fn cpu_pct_uses_window_delta() {
        // 1 wall second apart; process did (200-100)+(80-50)=130 ticks at CLK_TCK=100
        // → 130/100 ticks/s ÷ 1s × 100 = 130%
        let prev = ProcSnapshot {
            wall_secs: 1000.0,
            procs: vec![proc_sample(7, 100, 50, "0".repeat(64).as_str())],
        };
        let cur = ProcSnapshot {
            wall_secs: 1001.0,
            procs: vec![proc_sample(7, 200, 80, "0".repeat(64).as_str())],
        };
        let containers: std::collections::HashMap<String, String> = HashMap::new();
        let top = top_processes(&cur, Some(&prev), &containers, 12);
        assert_eq!(top.len(), 1);
        assert!((top[0].cpu_pct - 130.0).abs() < 0.5, "got {}", top[0].cpu_pct);
    }

    #[test]
    fn first_sample_reports_zero_cpu() {
        let cur = ProcSnapshot { wall_secs: 1.0, procs: vec![proc_sample(7, 100, 50, "host") ] };
        let containers: std::collections::HashMap<String, String> = HashMap::new();
        let top = top_processes(&cur, None, &containers, 12);
        assert_eq!(top[0].cpu_pct, 0.0);
    }

    #[test]
    fn container_attribution_by_prefix() {
        let id64 = "abc123def456abc123def456abc123def456abc123def456abc123def456abc1";
        let cur = ProcSnapshot { wall_secs: 1.0, procs: vec![
            proc_sample(1, 10, 0, id64),
            proc_sample(2, 20, 0, "host"),
        ]};
        let mut containers = HashMap::new();
        containers.insert(id64.to_string(), "codery-sandbox-blue".to_string());
        let top = top_processes(&cur, None, &containers, 12);
        assert_eq!(top[0].container, "codery-sandbox-blue");
        assert_eq!(top[1].container, "host");
    }

    #[test]
    fn top_is_sorted_by_rss_and_truncated() {
        let mut procs = Vec::new();
        for i in 0..20 {
            procs.push(ProcSample {
                pid: i, ppid: 1, state: "S".into(), comm: format!("p{i}"),
                utime: 0, stime: 0,
                rss_bytes: (i as u64) * 1_000_000,
                container_id: None,
            });
        }
        let cur = ProcSnapshot { wall_secs: 1.0, procs };
        let containers: std::collections::HashMap<String, String> = HashMap::new();
        let top = top_processes(&cur, None, &containers, 12);
        assert_eq!(top.len(), 12);
        assert_eq!(top[0].comm, "p19");
        assert!(top.windows(2).all(|w| w[0].rss_mb >= w[1].rss_mb));
    }

    #[test]
    fn scan_processes_reads_real_proc() {
        let snap = scan_processes().expect("scan");
        assert!(!snap.procs.is_empty(), "a running machine has processes");
        let init = snap.procs.iter().find(|p| p.pid == 1).expect("pid 1 exists");
        assert!(!init.comm.is_empty());
    }
```

Add this test helper beside the consts:

```rust
    fn proc_sample(pid: i32, utime: u64, stime: u64, container_id: &str) -> ProcSample {
        ProcSample {
            pid,
            ppid: 1,
            state: "S".into(),
            comm: format!("p{pid}"),
            utime,
            stime,
            rss_bytes: 1_000_000,
            container_id: if container_id == "host" { None } else { Some(container_id.to_string()) },
        }
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `ssh gem@apps "cd /home/gem/projects/Codery/system/orchestrator && sudo nix shell nixpkgs#rustc nixpkgs#cargo -c cargo test --lib host_metrics"`
Expected: FAIL — `ProcSample`, `scan_processes` etc. not defined.

- [ ] **Step 3: Implement scanner + attribution**

Add to `host_metrics.rs` (replace the `Unused` alias + `#[allow(dead_code)]` and the `BTreeMap` import with `use std::collections::HashMap;`):

```rust
use std::collections::HashMap;

// ── Process scanning ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ProcSample {
    pub pid: i32,
    pub ppid: i32,
    pub state: String,
    pub comm: String,
    pub utime: u64,
    pub stime: u64,
    pub rss_bytes: u64,
    pub container_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProcSnapshot {
    pub wall_secs: f64,
    pub procs: Vec<ProcSample>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TopProcess {
    pub pid: i32,
    pub comm: String,
    pub rss_mb: f64,
    pub cpu_pct: f64,
    pub container: String,
    pub state: String,
}

fn page_size() -> f64 {
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as f64 }
}

fn clock_ticks() -> f64 {
    unsafe { libc::sysconf(libc::_SC_CLK_TCK) as f64 }
}

/// Parse /proc/[pid]/stat. Returns (pid, ppid, state, utime, stime).
/// comm (field 2) may contain spaces and parens — parse after the LAST ')'.
pub fn parse_stat(content: &str) -> Option<(i32, i32, String, u64, u64)> {
    let open = content.find('(')?;
    let close = content.rfind(')')?;
    let pid: i32 = content[..open].trim().parse().ok()?;
    let comm = content[open + 1..close].to_string();
    // After ')' the fields resume at field 3 (state): idx0=state, 1=ppid,
    // 11=utime, 12=stime.
    let rest: Vec<&str> = content[close + 1..].split_whitespace().collect();
    if rest.len() < 13 {
        return None;
    }
    let state = rest[0].to_string();
    let ppid: i32 = rest[1].parse().ok()?;
    let utime: u64 = rest[11].parse().ok()?;
    let stime: u64 = rest[12].parse().ok()?;
    let _ = comm; // comm kept for debugging; TopProcess uses /proc/pid/comm
    Some((pid, ppid, state, utime, stime))
}

/// /proc/[pid]/statm field 2 (resident) × page size = RSS bytes.
pub fn parse_statm_rss_bytes(content: &str, page_size: f64) -> u64 {
    content
        .split_whitespace()
        .nth(1)
        .and_then(|r| r.parse::<u64>().ok())
        .map(|pages| (pages as f64 * page_size) as u64)
        .unwrap_or(0)
}

/// Extract the 64-hex container id from a cgroup v2 path like
/// `0::/system.slice/docker-<64hex>.scope`. None for host processes.
pub fn parse_cgroup_container(content: &str) -> Option<String> {
    for line in content.lines() {
        if let Some(idx) = line.find("docker-") {
            let rest = &line[idx + "docker-".len()..];
            let id: String = rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
            if id.len() == 64 {
                return Some(id);
            }
        }
    }
    None
}

fn read_file(path: &std::path::Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

/// Snapshot every userspace process (kernel threads, ppid==2, excluded).
pub fn scan_processes() -> std::io::Result<ProcSnapshot> {
    let page = page_size();
    let mut procs = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<i32>() else { continue };

        let stat = read_file(&entry.path().join("stat")).ok();
        let Some(stat) = stat else { continue };
        let Some((pid, ppid, state, utime, stime)) = parse_stat(&stat) else { continue };
        if ppid == 2 {
            continue; // kernel thread (child of kthreadd)
        }
        let comm = read_file(&entry.path().join("comm"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let rss_bytes = read_file(&entry.path().join("statm"))
            .map(|s| parse_statm_rss_bytes(&s, page))
            .unwrap_or(0);
        let container_id = read_file(&entry.path().join("cgroup"))
            .ok()
            .and_then(|c| parse_cgroup_container(&c));

        procs.push(ProcSample { pid, ppid, state, comm, utime, stime, rss_bytes, container_id });
    }
    let wall_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    Ok(ProcSnapshot { wall_secs, procs })
}

/// Rank processes by RSS, compute window CPU%, attribute to containers.
pub fn top_processes(
    cur: &ProcSnapshot,
    prev: Option<&ProcSnapshot>,
    containers: &HashMap<String, String>,
    n: usize,
) -> Vec<TopProcess> {
    let ticks = clock_ticks();
    let dt = (cur.wall_secs - prev.map(|p| p.wall_secs).unwrap_or(cur.wall_secs)).max(0.001);
    let mut rows: Vec<TopProcess> = cur
        .procs
        .iter()
        .map(|p| {
            let cpu_pct = match prev {
                Some(prev) => {
                    let prev_cpu = prev
                        .procs
                        .iter()
                        .find(|q| q.pid == p.pid)
                        .map(|q| q.utime + q.stime)
                        .unwrap_or(p.utime + p.stime); // unseen pid → 0% delta
                    ((p.utime + p.stime) - prev_cpu.min(p.utime + p.stime)) as f64
                        / ticks * 100.0 / dt
                }
                None => 0.0,
            };
            let container = match &p.container_id {
                Some(id) => containers
                    .iter()
                    .find(|(cid, _)| id.starts_with(cid.as_str()) || cid.starts_with(id.as_str()))
                    .map(|(_, name)| name.clone())
                    .unwrap_or_else(|| "unknown".to_string()),
                None => "host".to_string(),
            };
            TopProcess {
                pid: p.pid,
                comm: p.comm.clone(),
                rss_mb: p.rss_bytes as f64 / 1024.0 / 1024.0,
                cpu_pct,
                container,
                state: p.state.clone(),
            }
        })
        .collect();
    rows.sort_by(|a, b| b.rss_mb.partial_cmp(&a.rss_mb).unwrap_or(std::cmp::Ordering::Equal));
    rows.truncate(n);
    rows
}
```

Wait — the `cpu_pct` expression above is convoluted and wrong on unseen pids (it would divide by ticks then zero out... no: `prev_cpu.min(cur_total)` when prev unseen makes delta 0 — actually correct, but unreadable). Replace the `Some(prev) =>` block with the clear version:

```rust
                Some(prev) => {
                    let prev_cpu = prev
                        .procs
                        .iter()
                        .find(|q| q.pid == p.pid)
                        .map(|q| q.utime + q.stime)
                        .unwrap_or(p.utime + p.stime); // unseen pid → zero delta
                    let delta = (p.utime + p.stime).saturating_sub(prev_cpu);
                    delta as f64 / ticks * 100.0 / dt
                }
```

Use that version in the file.

- [ ] **Step 4: Run tests to verify they pass**

Run: `ssh gem@apps "cd /home/gem/projects/Codery/system/orchestrator && sudo nix shell nixpkgs#rustc nixpkgs#cargo -c cargo test --lib host_metrics"`
Expected: all PASS (including `scan_processes_reads_real_proc` — works in any Linux container with /proc).

- [ ] **Step 5: Commit**

```bash
cd /home/gem/projects/Codery && git add system/orchestrator/src/host_metrics.rs && git commit -m "feat(ci): process scanner with window CPU% and container attribution"
```

---

### Task 3: `collect()` assembly + bollard container map + 3 s broadcast task

**Files:**
- Modify: `system/orchestrator/src/host_metrics.rs`
- Modify: `system/orchestrator/src/ui.rs` (metrics task spawn + shared state types — routes come in Task 4)

**Interfaces:**
- Consumes: everything from Tasks 1–2.
- Produces (used by Task 4):
  - `pub struct HostMetrics { pub ts: u64, pub health: Health, pub memory: MemoryInfo, pub psi: Option<Psi>, pub oom_kills: u64, pub top_processes: Vec<TopProcess> }`
  - `pub struct MetricsState { pub prev: Option<ProcSnapshot>, pub prev_oom: Option<u64> }`
  - `pub fn collect(state: &mut MetricsState, containers: &HashMap<String, String>) -> HostMetrics`
  - `pub async fn container_map(docker: &bollard::Docker) -> HashMap<String, String>` (id → name; empty on error)
  - `pub fn read_psi() -> Option<Psi>` (None if any of the three files unreadable/empty — per spec, psi is null-or-complete)
  - `pub async fn metrics_task(tx: tokio::sync::broadcast::Sender<String>, state: std::sync::Arc<std::sync::Mutex<MetricsState>>, interval: std::time::Duration)` — loops forever: Docker connect + container map, collect, serialize, `tx.send`; on collect error, log + skip tick; sleeps `interval`.

- [ ] **Step 1: Write failing test**

Append to the test module:

```rust
    #[test]
    fn collect_assembles_full_snapshot() {
        let mut state = MetricsState { prev: None, prev_oom: None };
        let containers: HashMap<String, String> = HashMap::new();
        let m = collect(&mut state, &containers);
        assert!(m.ts > 0);
        assert!(m.memory.total_mb > 0.0, "a real machine has memory");
        assert!(matches!(m.health.status.as_str(), "green" | "yellow" | "red"));
        assert!(!m.top_processes.is_empty());
        // Second collect has a prev sample → cpu deltas active, no panic.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let m2 = collect(&mut state, &containers);
        assert!(m2.ts >= m.ts);
        // oom baseline captured on first collect
        assert!(state.prev_oom.is_some());
        assert!(state.prev.is_some());
    }

    #[test]
    fn host_metrics_serializes_snake_case() {
        let mut state = MetricsState { prev: None, prev_oom: None };
        let m = collect(&mut state, &HashMap::new());
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"top_processes\""));
        assert!(json.contains(\"\"oom_kills\"\".trim_matches('"')));
        assert!(json.contains("\"health\""));
    }
```

(Note: write that second assert plainly as `assert!(json.contains("\"oom_kills\""));` — the escaped oddity above is just to keep the plan valid; use the plain form.)

- [ ] **Step 2: Run test to verify it fails**

Run: `ssh gem@apps "cd /home/gem/projects/Codery/system/orchestrator && sudo nix shell nixpkgs#rustc nixpkgs#cargo -c cargo test --lib host_metrics"`
Expected: FAIL — `collect`, `MetricsState` not defined.

- [ ] **Step 3: Implement**

Add to `host_metrics.rs`:

```rust
// ── Assembly ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct HostMetrics {
    pub ts: u64,
    pub health: Health,
    pub memory: MemoryInfo,
    pub psi: Option<Psi>,
    pub oom_kills: u64,
    pub top_processes: Vec<TopProcess>,
}

/// Window state shared between the timer task and HTTP handlers so every
/// consumer sees the same CPU deltas and oom baseline.
#[derive(Debug, Default)]
pub struct MetricsState {
    pub prev: Option<ProcSnapshot>,
    pub prev_oom: Option<u64>,
}

/// Read all three PSI files; None unless all three parse (spec: null-or-complete).
pub fn read_psi() -> Option<Psi> {
    let cpu = std::fs::read_to_string("/proc/pressure/cpu").ok()?;
    let memory = std::fs::read_to_string("/proc/pressure/memory").ok()?;
    let io = std::fs::read_to_string("/proc/pressure/io").ok()?;
    Some(Psi {
        cpu: parse_pressure(&cpu)?,
        memory: parse_pressure(&memory)?,
        io: parse_pressure(&io)?,
    })
}

/// Docker container id → name map for process attribution. Empty on error.
pub async fn container_map(docker: &bollard::Docker) -> HashMap<String, String> {
    let list = docker.list_containers(Some(bollard::container::ListContainersOptions::<String> {
        all: true,
        ..Default::default()
    })).await.unwrap_or_default();
    let mut map = HashMap::new();
    for c in list {
        let Some(id) = c.id.as_deref().map(str::to_string) else { continue };
        let name = c.names.unwrap_or_default().into_iter().next()
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_else(|| "unknown".to_string());
        map.insert(id, name);
    }
    map
}

/// Build one snapshot. Mutates `state` (prev sample, oom baseline).
pub fn collect(state: &mut MetricsState, containers: &HashMap<String, String>) -> HostMetrics {
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let memory = parse_meminfo(&meminfo);
    let psi = read_psi();
    let oom_kills = std::fs::read_to_string("/sys/fs/cgroup/memory.events")
        .map(|t| parse_memory_events(&t))
        .unwrap_or(0);

    let snapshot = scan_processes().ok();
    let top = match (&snapshot, &state.prev) {
        (Some(cur), prev) => top_processes(cur, prev.as_ref(), containers, 12),
        (None, _) => Vec::new(),
    };

    let health = derive_health(&memory, psi.as_ref(), state.prev_oom, oom_kills);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    state.prev = snapshot;
    state.prev_oom = Some(oom_kills);

    HostMetrics { ts, health, memory, psi, oom_kills, top_processes }
}

/// Forever-loop: every `interval`, broadcast a fresh snapshot as JSON.
pub async fn metrics_task(
    tx: tokio::sync::broadcast::Sender<String>,
    state: std::sync::Arc<std::sync::Mutex<MetricsState>>,
    interval: std::time::Duration,
) {
    loop {
        tokio::time::sleep(interval).await;
        let docker = match bollard::Docker::connect_with_socket_defaults() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[metrics] docker connect failed: {e}");
                continue;
            }
        };
        let containers = container_map(&docker).await;
        let json = {
            let mut st = state.lock().unwrap();
            let m = collect(&mut st, &containers);
            serde_json::to_string(&m)
        };
        match json {
            Ok(json) => {
                let _ = tx.send(json); // no subscribers = fine
            }
            Err(e) => eprintln!("[metrics] serialize failed: {e}"),
        }
    }
}
```

And in `ui.rs` add the shared-state alias (near the other type aliases at the top):

```rust
pub type MetricsStateShared = std::sync::Arc<std::sync::Mutex<crate::host_metrics::MetricsState>>;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `ssh gem@apps "cd /home/gem/projects/Codery/system/orchestrator && sudo nix shell nixpkgs#rustc nixpkgs#cargo -c cargo test --lib host_metrics"`
Expected: PASS including the two new tests (they exercise real `/proc` — fine in any Linux container).

- [ ] **Step 5: Commit**

```bash
cd /home/gem/projects/Codery && git add system/orchestrator/src/host_metrics.rs system/orchestrator/src/ui.rs && git commit -m "feat(ci): host metrics snapshot assembly + broadcast task"
```

---

### Task 4: HTTP surface — `/api/metrics` + `/api/metrics/stream` (SSE)

**Files:**
- Modify: `system/orchestrator/src/ui.rs`

**Interfaces:**
- Consumes: `host_metrics::{HostMetrics, MetricsState, metrics_task, collect}`; existing SSE pattern in `get_events`.
- Produces:
  - `AppState.metrics: MetricsStateShared` and `AppState.metrics_tx: Arc<broadcast::Sender<String>>`
  - `pub fn make_router_with_metrics(events_tx, ops, metrics_tx, metrics) -> Router` (testable entry; `make_router` keeps its signature and delegates with fresh channels so existing callers/tests are untouched)
  - `pub async fn serve(...)` (signature unchanged — creates metrics channels + spawns `metrics_task` internally so daemon.rs and `serve-ui` in main.rs need no edits)
  - Routes: `GET /api/metrics` → fresh JSON snapshot; `GET /api/metrics/stream` → SSE, initial snapshot then broadcast messages.

- [ ] **Step 1: Write failing integration test**

Append inside the existing `#[cfg(test)] mod tests` in `ui.rs`:

```rust
    #[tokio::test]
    async fn metrics_endpoints_serve_and_stream() {
        let (tx, _) = tokio::sync::broadcast::channel::<String>(16);
        let tx = Arc::new(tx);
        let metrics: crate::ui::MetricsStateShared =
            Arc::new(std::sync::Mutex::new(crate::host_metrics::MetricsState::default()));
        let ops: Ops = Arc::new(Mutex::new(HashMap::new()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, make_router_with_metrics(
                Arc::new(tokio::sync::broadcast::channel::<String>(16).0),
                ops, tx.clone(), metrics,
            )).await.unwrap()
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Snapshot endpoint: 200 + required snake_case fields.
        let resp = reqwest::get(format!("http://127.0.0.1:{port}/api/metrics"))
            .await.expect("GET /api/metrics");
        assert!(resp.status().is_success());
        let body: serde_json::Value = resp.json().await.expect("json");
        assert!(body["health"]["status"].as_str().is_some(), "got {body}");
        assert!(body["memory"]["total_mb"].as_f64().is_some());
        assert!(body["top_processes"].as_array().is_some());
        assert!(body["oom_kills"].as_u64().is_some());
        // psi may be null (kernel-dependent) — that is valid.

        // SSE endpoint: first delivered event is a metrics snapshot.
        use futures_util::StreamExt;
        let mut es = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/api/metrics/stream"))
            .send().await.expect("SSE connect")
            .bytes_stream();
        let mut first = Vec::new();
        while first.len() < 3 {
            let Some(chunk) = es.next().await else { break };
            first.extend_from_slice(&chunk.unwrap());
            if first.windows(2).any(|w| w == b"\n\n") { break; }
        }
        let text = String::from_utf8_lossy(&first);
        assert!(text.contains("data:"), "no SSE data frame in: {text}");
        let payload = text.split("data: ").nth(1).unwrap_or("");
        let v: serde_json::Value = serde_json::from_str(payload.trim_end())
            .expect("initial SSE frame is a metrics JSON");
        assert!(v["health"]["status"].as_str().is_some());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `ssh gem@apps "cd /home/gem/projects/Codery/system/orchestrator && sudo nix shell nixpkgs#rustc nixpkgs#cargo -c cargo test --lib metrics_endpoints"`
Expected: compile FAIL — `make_router_with_metrics`, `MetricsStateShared` path wrong (`crate::ui::`), routes missing.

- [ ] **Step 3: Implement routes + wiring**

In `ui.rs`:

a) Extend `AppState` and both router builders. Replace the existing `AppState` struct and `make_router`:

```rust
#[derive(Clone)]
pub struct AppState {
    pub rollback_lock: RollbackLock,
    pub events_tx:    Arc<broadcast::Sender<String>>,
    pub ops:          Ops,
    pub metrics_tx:   Arc<broadcast::Sender<String>>,
    pub metrics:      MetricsStateShared,
}

pub fn make_router(events_tx: Arc<broadcast::Sender<String>>, ops: Ops) -> Router {
    let (metrics_tx, _) = broadcast::channel::<String>(32);
    let metrics: MetricsStateShared =
        Arc::new(std::sync::Mutex::new(crate::host_metrics::MetricsState::default()));
    make_router_with_metrics(events_tx, ops, Arc::new(metrics_tx), metrics)
}

pub fn make_router_with_metrics(
    events_tx: Arc<broadcast::Sender<String>>,
    ops: Ops,
    metrics_tx: Arc<broadcast::Sender<String>>,
    metrics: MetricsStateShared,
) -> Router {
    let state = AppState {
        rollback_lock: Arc::new(Mutex::new(HashSet::new())),
        events_tx,
        ops,
        metrics_tx,
        metrics,
    };
    Router::new()
        .route("/", get(serve_index))
        .route("/api/status", get(get_status))
        .route("/api/events", get(get_events))
        .route("/api/metrics", get(get_metrics))
        .route("/api/metrics/stream", get(get_metrics_stream))
        .route("/api/stop/{container}",    post(post_stop))
        .route("/api/start/{container}",  post(post_start))
        .route("/api/kill/{container}",   post(post_kill))
        .route("/api/restart/{container}", post(post_restart))
        .route("/api/rollback/{service}", post(post_rollback))
        .with_state(state)
}
```

b) Update `serve` to create the channels, spawn the task, and pass through:

```rust
pub async fn serve(port: u16, events_tx: Arc<broadcast::Sender<String>>, ops: Ops) -> Result<()> {
    let (metrics_tx, _) = broadcast::channel::<String>(32);
    let metrics_tx = Arc::new(metrics_tx);
    let metrics: MetricsStateShared =
        Arc::new(std::sync::Mutex::new(crate::host_metrics::MetricsState::default()));
    tokio::spawn(crate::host_metrics::metrics_task(
        (*metrics_tx).clone(), Arc::clone(&metrics),
        std::time::Duration::from_secs(3),
    ));
    let addr = format!("127.0.0.1:{}", port);
    println!("[ui {}] Listening on http://{} (metrics every 3s)", ts(), addr);
    let listener = TcpListener::bind(&addr).await?;
    axum::serve(listener, make_router_with_metrics(events_tx, ops, metrics_tx, metrics)).await?;
    Ok(())
}
```

(`metrics_task` takes `Sender` by value — clone out of the Arc; adjust the call to `metrics_task(metrics_tx.as_ref().clone(), ...)` if the borrow checker prefers it.)

c) Handlers (place beside `get_events`):

```rust
// ── Metrics ──────────────────────────────────────────────────────────────────

async fn metrics_snapshot(state: &AppState) -> String {
    let containers = {
        match Docker::connect_with_socket_defaults() {
            Ok(d) => crate::host_metrics::container_map(&d).await,
            Err(_) => HashMap::new(),
        }
    };
    let mut m = state.metrics.lock().unwrap();
    let snap = crate::host_metrics::collect(&mut m, &containers);
    serde_json::to_string(&snap).unwrap_or_else(|_| "{}".to_string())
}

async fn get_metrics(State(state): State<AppState>) -> impl IntoResponse {
    let json = metrics_snapshot(&state).await;
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))],
        json,
    ).into_response()
}

async fn get_metrics_stream(
    State(state): State<AppState>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let mut rx = state.metrics_tx.subscribe();
    let initial = metrics_snapshot(&state).await;

    let stream = futures_util::stream::unfold(
        (rx, Some(initial)),
        |(mut rx, initial)| async move {
            if let Some(json) = initial {
                return Some((Ok(Event::default().data(json)), (rx, None)));
            }
            loop {
                match rx.recv().await {
                    Ok(json) => return Some((Ok(Event::default().data(json)), (rx, None))),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );

    Sse::new(stream).keep_alive(KeepAlive::default())
}
```

- [ ] **Step 4: Run full test suite**

Run: `ssh gem@apps "cd /home/gem/projects/Codery/system/orchestrator && sudo nix shell nixpkgs#rustc nixpkgs#cargo -c cargo test"`
Expected: ALL tests pass (existing suites untouched; `status_lists_running_containers` skips without Docker). Fix any compile warnings (unused imports etc.) before committing.

- [ ] **Step 5: Commit**

```bash
cd /home/gem/projects/Codery && git add system/orchestrator/src/ui.rs && git commit -m "feat(ci): /api/metrics + SSE stream endpoints"
```

---

### Task 5: Frontend — `HostPanel` + wiring

**Files:**
- Modify: `system/orchestrator/ui/src/types.ts`
- Create: `system/orchestrator/ui/src/HostPanel.tsx`
- Modify: `system/orchestrator/ui/src/App.tsx`
- Modify: `system/orchestrator/ui/src/App.css`

**Interfaces:**
- Consumes: `GET /api/metrics/stream` JSON shape = Task 3 `HostMetrics` (snake_case).
- Produces: `<HostPanel metrics={...} />` component rendered above the sections in `App.tsx`; TS type `HostMetrics` in `types.ts`.

- [ ] **Step 1: Add types**

Append to `types.ts`:

```typescript
export interface PsiWindow { avg10: number; avg60: number; avg300: number; }
export interface PsiResource { some: PsiWindow; full: PsiWindow | null; }
export interface Psi { cpu: PsiResource; memory: PsiResource; io: PsiResource; }
export interface Health { status: 'green' | 'yellow' | 'red'; reasons: string[]; }
export interface TopProcess {
  pid: number; comm: string; rss_mb: number; cpu_pct: number;
  container: string; state: string;
}
export interface HostMetrics {
  ts: number;
  health: Health;
  memory: {
    total_mb: number; available_mb: number; cached_mb: number; buffers_mb: number;
    swap_total_mb: number; swap_free_mb: number;
  };
  psi: Psi | null;
  oom_kills: number;
  top_processes: TopProcess[];
}
```

- [ ] **Step 2: Build `HostPanel.tsx`**

```tsx
import { HostMetrics, TopProcess } from './types';

function fmtMb(mb: number): string {
  return mb >= 1024 ? `${(mb / 1024).toFixed(1)}G` : `${Math.round(mb)}M`;
}

function PsiBar({ label, resource, odId }: { label: string; resource: { some: { avg10: number; avg60: number; avg300: number } } | undefined; odId: string }) {
  if (!resource) return null;
  const { avg10, avg60, avg300 } = resource.some;
  return (
    <div className="psi-row" data-od-id={odId} title={`avg10 ${avg10.toFixed(1)}% · avg60 ${avg60.toFixed(1)}% · avg300 ${avg300.toFixed(1)}%`}>
      <span className="psi-label">{label}</span>
      <div className="psi-track">
        <div className="psi-fill" style={{ width: `${Math.min(avg10, 100)}%` }} />
        <div className="psi-marker" style={{ left: `${Math.min(avg60, 100)}%` }} />
      </div>
      <span className="psi-value">{avg10.toFixed(1)}%</span>
    </div>
  );
}

function OffenderRow({ p, maxRss }: { p: TopProcess; maxRss: number }) {
  const badge = p.container.startsWith('codery-')
    ? p.container.replace(/^codery-/, '').replace(/-blue$/, '-blue').replace(/-green$/, '-green')
    : p.container;
  return (
    <tr data-od-id="host-offender-row">
      <td className="off-comm">{p.comm}</td>
      <td className="off-rss">
        <div className="rss-track"><div className="rss-fill" style={{ width: `${maxRss > 0 ? (p.rss_mb / maxRss) * 100 : 0}%` }} /></div>
        <span className="num">{p.rss_mb >= 1024 ? `${(p.rss_mb / 1024).toFixed(2)}G` : `${Math.round(p.rss_mb)}M`}</span>
      </td>
      <td className="off-cpu num">{p.cpu_pct.toFixed(1)}%</td>
      <td><span className="container-badge">{badge}</span></td>
    </tr>
  );
}

export function HostPanel({ metrics }: { metrics: HostMetrics | null }) {
  if (!metrics) return <section className="section host-panel" data-od-id="host-health-panel"><div className="section-label">Host Health</div><p className="loading">Loading metrics…</p></section>;
  const { health, memory, psi, oom_kills, top_processes } = metrics;
  const usedMb = Math.max(memory.total_mb - memory.available_mb, 0);
  const usedPct = memory.total_mb > 0 ? (usedMb / memory.total_mb) * 100 : 0;
  const cachePct = memory.total_mb > 0 ? ((memory.cached_mb + memory.buffers_mb) / memory.total_mb) * 100 : 0;
  const swapUsed = Math.max(memory.swap_total_mb - memory.swap_free_mb, 0);
  const swapPct = memory.swap_total_mb > 0 ? (swapUsed / memory.swap_total_mb) * 100 : 0;
  const maxRss = Math.max(...top_processes.map(p => p.rss_mb), 1);

  return (
    <section className="section host-panel" data-od-id="host-health-panel">
      <div className="section-label">Host Health</div>
      <div className="card host-card">
        <div className="host-top">
          <span className={`conn-pill health-${health.status}`} data-od-id="host-health-pill" title={health.reasons.join(' · ') || 'all clear'}>
            <span className="conn-dot" />{health.status}
          </span>
          {health.reasons.length > 0 && <span className="health-reasons">{health.reasons.join(' · ')}</span>}
          <span className={`oom-badge ${oom_kills > 0 ? 'oom-hot' : ''}`} data-od-id="host-oom-badge">
            {oom_kills} OOM kills since boot
          </span>
        </div>

        <div className="host-grid">
          <div className="mem-block" data-od-id="host-memory-block">
            <div className="mem-numbers num">
              {fmtMb(usedMb)} used · {fmtMb(memory.available_mb)} free · {fmtMb(memory.total_mb)} total
            </div>
            <div className="mem-track" data-od-id="host-memory-bar">
              <div className="mem-cache" style={{ width: `${Math.min(cachePct, 100)}%` }} />
              <div className="mem-fill" style={{ width: `${Math.min(usedPct, 100)}%` }} />
            </div>
            {memory.swap_total_mb > 0 ? (
              <>
                <div className="mem-numbers num swap-numbers" data-od-id="host-swap-bar">
                  swap {fmtMb(swapUsed)} / {fmtMb(memory.swap_total_mb)}
                </div>
                <div className="swap-track"><div className="swap-fill" style={{ width: `${swapPct}%` }} /></div>
              </>
            ) : (
              <div className="no-swap-tag" data-od-id="host-no-swap">no swap</div>
            )}
          </div>

          <div className="psi-block" data-od-id="host-psi-block">
            {psi ? (
              <>
                <PsiBar label="CPU" resource={psi.cpu.some ? psi.cpu : undefined} odId="host-psi-cpu" />
                <PsiBar label="Mem" resource={psi.memory} odId="host-psi-memory" />
                <PsiBar label="IO" resource={psi.io} odId="host-psi-io" />
              </>
            ) : (
              <div className="psi-unavailable">PSI unavailable</div>
            )}
          </div>
        </div>

        <table className="offenders" data-od-id="host-offenders-table">
          <thead>
            <tr><th>process</th><th>rss</th><th className="off-cpu">cpu</th><th>container</th></tr>
          </thead>
          <tbody>
            {top_processes.map(p => <OffenderRow key={`${p.pid}-${p.comm}`} p={p} maxRss={maxRss} />)}
          </tbody>
        </table>
      </div>
    </section>
  );
}
```

(Note: `PsiBar` receives `psi.cpu` whose `full` is null for CPU — the prop type only reads `.some`; pass `psi.cpu` directly for all three rows: `resource={psi.cpu}` works since the prop type is structural. Simplify: change the `resource` prop type to `{ some: { avg10: number; avg60: number; avg300: number } } | undefined` and pass `psi.cpu`, `psi.memory`, `psi.io` directly.)

- [ ] **Step 3: Wire into `App.tsx`**

```tsx
import { useEffect, useState } from 'react';
import { Container, HostMetrics } from './types';
import { ContainerCard } from './ContainerCard';
import { HostPanel } from './HostPanel';
import './App.css';
```

Inside `App()`, add state + a second EventSource (below the existing one):

```tsx
  const [hostMetrics, setHostMetrics] = useState<HostMetrics | null>(null);

  useEffect(() => {
    const es = new EventSource('/api/metrics/stream');
    es.onmessage = (e) => {
      try { setHostMetrics(JSON.parse(e.data as string)); } catch { /* ignore malformed */ }
    };
    es.onerror = () => { /* browser auto-reconnects; keep last sample */ };
    return () => es.close();
  }, []);
```

Render it above the sections (between header and the sections map):

```tsx
      <HostPanel metrics={hostMetrics} />
```

- [ ] **Step 4: Add styles to `App.css`**

Append (existing tokens only — final look subject to the OpenDesign mock):

```css
/* ── Host Health panel ──────────────────────────────────────────────────────── */

.host-panel .host-card { display: flex; flex-direction: column; gap: 14px; padding: 14px 16px; }
.host-top { display: flex; align-items: center; gap: 12px; flex-wrap: wrap; }
.health-green .conn-dot { background: var(--ok); }
.health-yellow .conn-dot { background: var(--warn); }
.health-red .conn-dot { background: var(--danger); }
.health-reasons { color: var(--muted); font-size: 12px; }
.oom-badge { margin-left: auto; font-family: var(--mono); font-size: 12px; color: var(--muted); }
.oom-badge.oom-hot { color: var(--danger); font-weight: 650; }

.host-grid { display: grid; grid-template-columns: 1fr 1fr; gap: 20px; }
@media (max-width: 800px) { .host-grid { grid-template-columns: 1fr; } }

.mem-numbers { font-family: var(--mono); font-size: 12px; color: var(--fg); margin-bottom: 6px; }
.mem-track, .swap-track, .psi-track, .rss-track { position: relative; height: 10px; background: var(--surface-2); border-radius: 5px; overflow: hidden; }
.mem-cache { position: absolute; inset: 0 auto 0 0; background: var(--surface-2); background: color-mix(in oklab, var(--accent) 25%, var(--surface-2)); }
.mem-fill { position: absolute; inset: 0 auto 0 0; background: var(--accent); opacity: 0.85; }
.swap-track { height: 6px; margin-top: 4px; }
.swap-fill { position: absolute; inset: 0 auto 0 0; background: var(--warn); }
.no-swap-tag { font-family: var(--mono); font-size: 11px; color: var(--muted); margin-top: 6px; }

.psi-row { display: grid; grid-template-columns: 40px 1fr 52px; align-items: center; gap: 8px; margin: 6px 0; }
.psi-label { font-size: 12px; color: var(--muted); }
.psi-fill { position: absolute; inset: 0 auto 0 0; background: var(--accent); opacity: 0.7; }
.psi-marker { position: absolute; top: 0; bottom: 0; width: 2px; background: var(--fg); opacity: 0.6; }
.psi-value { font-family: var(--mono); font-size: 12px; text-align: right; color: var(--fg); }
.psi-unavailable { font-size: 12px; color: var(--muted); font-style: italic; }

.offenders { width: 100%; border-collapse: collapse; font-size: 12px; }
.offenders th { text-align: left; color: var(--muted); font-weight: 500; padding: 4px 8px 4px 0; border-bottom: 1px solid var(--border); }
.offenders td { padding: 5px 8px 5px 0; border-bottom: 1px solid color-mix(in oklab, var(--border) 40%, transparent); }
.off-comm { font-family: var(--mono); color: var(--fg); }
.off-cpu { text-align: right; font-family: var(--mono); }
.rss-track { display: inline-block; vertical-align: middle; width: 120px; height: 6px; margin-right: 8px; }
.rss-fill { position: absolute; inset: 0 auto 0 0; background: var(--warn); opacity: 0.6; }
.num { font-family: var(--mono); }
.container-badge { font-family: var(--mono); font-size: 11px; color: var(--muted); background: var(--surface-2); border-radius: 4px; padding: 2px 6px; }
```

- [ ] **Step 5: Build the frontend**

Run: `cd /home/gem/projects/Codery/system/orchestrator/ui && npm ci && npm run build` (sandbox fallback: `bun install && bun run build`)
Expected: `tsc -b` passes, `vite build` emits `dist/index.html`. Fix all TS errors — none may be suppressed with `any` casts beyond the existing style.

- [ ] **Step 6: Commit**

```bash
cd /home/gem/projects/Codery && git add system/orchestrator/ui/src && git commit -m "feat(ui): Host Health panel — memory/PSI/OOM meters + worst-offenders table"
```

---

### Task 6: Release 0.14.0 + deploy + live verification

**Files:**
- Modify: `system/orchestrator/Cargo.toml` (version)

**Interfaces:**
- Consumes: everything; the release workflow runs `cross test` (both targets) + `npm run build` + `cross build --release`; the deploy workflow installs the binary and restarts `codery-ci-daemon`.

- [ ] **Step 1: Bump version**

In `system/orchestrator/Cargo.toml`, `[package]` section: `version = "0.14.0"`. Update `Cargo.lock` via the apps-container toolchain: `ssh gem@apps "cd /home/gem/projects/Codery/system/orchestrator && sudo nix shell nixpkgs#rustc nixpkgs#cargo -c cargo update -p codery-ci --precise 0.14.0 2>/dev/null || sudo nix shell nixpkgs#rustc nixpkgs#cargo -c cargo check -q"` (any command that refreshes the lockfile version entry).

- [ ] **Step 2: Commit + tag + push**

```bash
cd /home/gem/projects/Codery && git add system/orchestrator/Cargo.toml system/orchestrator/Cargo.lock && git commit -m "codery-ci: bump to v0.14.0" && git tag codery-ci-v0.14.0 && github-push && github-push codery-ci-v0.14.0
```

- [ ] **Step 3: Watch the release workflow**

```bash
github-app-token '' CoderyOSS > /tmp/tok
GH_TOKEN=$(cat /tmp/tok) gh run watch --repo CoderyOSS/Codery $(GH_TOKEN=$(cat /tmp/tok) gh run list --repo CoderyOSS/Codery --workflow release-orchestrator.yml --limit 1 --json databaseId -q '.[0].databaseId')
```
Expected: both `cross test` jobs and the release creation succeed. **If `cross test` fails here, fix and re-tag (patch bump 0.14.1 semantics apply only after a successful release — a failed run can be re-run as-is).**

- [ ] **Step 4: Deploy (Build Orchestrator workflow)**

```bash
GH_TOKEN=$(cat /tmp/tok) gh workflow run build-orchestrator.yml --repo CoderyOSS/Codery --ref master
```
Wait ~2 min, then verify on the host via the run status:
Expected: run success; workflow output shows `codery-ci --version` = 0.14.0 and `codery-ci-daemon RUNNING`.

- [ ] **Step 5: Verify live**

```bash
sleep 20
curl -s -m 10 https://ci.rancidgrandmas.online/api/metrics | python3 -m json.tool | head -30
```
Expected: JSON with `health.status` ∈ green/yellow/red, memory numbers matching a 7.6 GiB box, 12 `top_processes` rows (opencode near the top of RSS). Then open `https://ci.rancidgrandmas.online/` in the Playwright browser: Host Health panel visible above Sandbox/Apps sections, values updating in place every ~3 s, health pill colored, offender table rendering with container badges (`sandbox-blue` etc.). Screenshot for the user.

- [ ] **Step 6: Fold-in note + final commit**

If the OpenDesign mock for the panel has been approved by then, restyle `App.css` + `HostPanel.tsx` markup to match it (functional behavior unchanged) in a follow-up commit `feat(ui): adopt OpenDesign mock for Host Health panel`. Otherwise note in the plan that visual adoption is pending and stop.

---

## Self-Review (done at plan-writing time)

- **Spec coverage:** §4 data contract → Tasks 1–3; §5 health → Task 1; §6 backend → Tasks 3–4; §7.2 slots + §7.4 edge states → Task 5 (PSI-unavailable → `psi: null` render, no-swap tag, first-sample cpu 0 handled by `cpu_pct: 0.0` semantics); §8 degradation → parsers default to 0/None, `collect` never panics on missing files; §9 testing → unit tests Tasks 1–3, integration Task 4, CI `cross test` Task 6; §10 deployment → Task 6. §7.5 deliverable 5 (data-od-id mapping) → Task 5 markup. Visual mock adoption (§7.5 deliverables 1–4) deliberately deferred — CSS-only follow-up.
- **Placeholders:** none — all code shown.
- **Type consistency:** `MetricsStateShared` defined Task 3 (ui.rs) used Task 4; `HostMetrics` field names snake_case end-to-end (serde default ↔ TS interfaces ↔ tests); `make_router_with_metrics(events_tx, ops, metrics_tx, metrics)` signature consistent between Task 4 test and impl; `collect(&mut MetricsState, &HashMap<String,String>)` consistent Tasks 3–4.
