# Host Metrics Dashboard (Memory Pressure Metering) — Design

Date: 2026-09-16
Status: Approved approach A (backend). **Visual design approved: OpenDesign Option A "Instrument strip"** — 322 px sidebar rail beside the console (band above services ≤1020 px, single stack ≤640 px). Mock: `docs/design/host-health-option-a.html` (deliverable A of `host-health-sidebar-options.html`). §7.5 deliverables 1–4 satisfied by that mock's four state views (nominal, red storm, PSI unavailable, first sample).

## 1. Goal

After the Sep 16 2026 OOM storm (13 kernel kills in one evening, docker build × opencode collision), the deploy console at `ci.rancidgrandmas.online` must answer, at a glance:

1. How much memory headroom does the host have right now? (memory meter)
2. Is the machine healthy? (health status indicator)
3. How hard is the machine working to keep up with demand? (kernel PSI — % of time tasks are stalled waiting for CPU / memory / IO)
4. Who are the worst offenders? (per-process meters, container-attributed)

## 2. Scope

**P0 (this spec):** live snapshot meters, refreshed every ~3 s, host-level, with per-process worst-offenders table. No alerting (planned as a separate follow-up pass), no history/sparklines, no per-container rollup aggregates, no disk/CPU-temperature metrics.

**Out of scope forever for this feature:** metrics inside containers (host kernel sees everything via `/proc`).

## 3. Approved approach (A)

Extend the existing `codery-ci` daemon UI. New `host_metrics.rs` module parses `/proc` directly (no subprocesses). A 3 s timer task in the daemon broadcasts JSON on a new `metrics_tx` broadcast channel. `ui.rs` serves `GET /api/metrics` (fresh snapshot) and `GET /api/metrics/stream` (SSE, same keep-alive pattern as `/api/events`). Frontend adds a `HostPanel.tsx` fed by a second EventSource. Rationale: matches the existing SSE-driven "Live" UX; window-accurate CPU% from `/proc/[pid]/stat` deltas (not `ps` lifetime averages); zero new deploy surface on a 7.6 GiB box.

## 4. Data contract (per sample)

```json
{
  "ts": 1726500000,
  "health": { "status": "green | yellow | red", "reasons": ["MemAvailable 9% of total"] },
  "memory": {
    "total_mb": 7862, "available_mb": 712,
    "cached_mb": 1830, "buffers_mb": 120,
    "swap_total_mb": 0, "swap_free_mb": 0
  },
  "psi": {
    "cpu":    { "some": { "avg10": 3.2, "avg60": 1.1, "avg300": 0.4 } },
    "memory": { "some": { "avg10": 0.0, "avg60": 0.0, "avg300": 0.0 },
                "full": { "avg10": 0.0, "avg60": 0.0, "avg300": 0.0 } },
    "io":     { "some": { "avg10": 1.4, "avg60": 0.2, "avg300": 0.1 },
                "full": { "avg10": 0.0, "avg60": 0.0, "avg300": 0.0 } }
  },
  "oom_kills": 13,
  "top_processes": [
    { "pid": 1234, "comm": "opencode", "rss_mb": 788.4, "cpu_pct": 12.3,
      "container": "codery-sandbox-blue", "state": "S" }
  ]
}
```

- `psi` is `null` when `/proc/pressure/*` is unavailable. CPU has `some` only (kernel emits no `full` line for CPU).
- `oom_kills` from root cgroup `/sys/fs/cgroup/memory.events` (`oom_kill` field), monotonic since boot.
- `top_processes`: top 12 by RSS, kernel threads (ppid 2) excluded. `cpu_pct` = `(Δutime+Δstime)/(Δwalltime×CLK_TCK)×100` between consecutive samples; `0` on the first sample after daemon start. `container` = Docker container name via cgroup path → bollard id map, or `"host"`.

## 5. Health derivation (memory-derived only — approved)

Constants live in one place, easy to tune.

| Status | Trigger (any) |
|---|---|
| red | MemAvailable < 10% of total · swap > 50% used · `psi.memory.some.avg10` > 25 · oom_kill counter increased since previous sample |
| yellow | MemAvailable < 20% of total · swap in use > 100 MB · `psi.memory.some.avg10` > 10 · `psi.memory.some.avg60` > 25 |
| green | none of the above |

Red outranks yellow. `reasons[]` lists every trigger in human form ("swap in use 1.2G", "OOM kill detected"). PSI unavailable → derive from memory/swap only and note "PSI unavailable" in reasons.

## 6. Backend

- `system/orchestrator/src/host_metrics.rs` (new): `HostMetrics` (serde `Serialize`), `collect(prev: &mut Option<ProcSamples>, containers: &HashMap<String,String>) -> HostMetrics`; parsers for meminfo (keyed `kB` lines), pressure (`some/full avg10/60/300`), `/proc/swaps`, `memory.events`, cgroup line (`0::/system.slice/docker-<64hex>.scope` → id). Process scan: `/proc/[0-9]+/{comm,stat,statm,cgroup}`; RSS = statm resident × page size; skip vanished pids.
- Container attribution: one bollard `list_containers` per cycle → `id_prefix → name`; on failure, `container: "unknown"`.
- `daemon.rs`: spawn `metrics_task(metrics_tx)` — sleep 3 s, collect, `serde_json::to_string`, `send`. Owns the previous sample for CPU deltas and oom counter.
- `ui.rs`: `AppState.metrics_tx: broadcast::Sender<String>`; `GET /api/metrics` returns a fresh snapshot (500 on error, like existing handlers); `GET /api/metrics/stream` = SSE, initial snapshot then broadcast messages, `KeepAlive::default()`.

## 7. Functional requirements for the UI — OpenDesign handoff

> **This section is the brief for OpenDesign.** The visual design of the panel is produced in OpenDesign and folded back here before implementation planning. Data, states, and constraints below are fixed; layout, hierarchy, and micro-interactions are what OpenDesign decides.

### 7.1 What the panel is

A live "Host Health" section in the existing Codery Deploy Console (`system/orchestrator/ui`, React + Vite, dark theme). It sits **above the service sections** (Sandbox / Apps / Other), full content width (max 1100 px page). Updates arrive every ~3 s via SSE and must update **in place** — no layout shift, no flash-of-empty on refresh.

### 7.2 Data the design must accommodate (see §4 for exact shapes)

| Slot | Content | Required states |
|---|---|---|
| Health pill | `green / yellow / red` + reasons text | pill colors map to existing `--ok / --warn / --danger`; reasons visible on hover/focus or as a sub-line |
| Memory meter | used vs available out of total (MB numbers, mono font) | bar fill for used (total−available); cached+buffers may be shown as a soft secondary segment; numeric readout |
| Swap | swap_used (= swap_total − swap_free) of swap_total | distinct thin bar **or** badge; if `swap_total_mb = 0` show a "no swap" tag — this is operationally important on this host |
| PSI meters | three rows: CPU, Memory, IO; avg10 value | each 0–100% scale; must show avg10 as the primary fill and avg60 as a secondary marker (tick/ghost fill); avg300 available on hover tooltip |
| OOM badge | `N OOM kills since boot` | neutral when 0; danger emphasis when > 0; visual "bump" state when the count increments while watching |
| Offenders table | 12 rows max: process name, RSS MB, CPU %, container badge | RSS mini-bar per row scaled to the max RSS in the current sample; CPU % mono, right-aligned; container badge shortened (`codery-sandbox-blue` → `sandbox-blue`, `"host"` for host processes) |

### 7.3 Design constraints (fixed — match existing console)

- Tokens: `--bg/--surface/--surface-2/--fg/--muted/--border/--accent/--ok/--warn/--danger/--off` (oklch, defined in `App.css`); mono font for all numbers (`--mono`); sans for labels (`--sans`).
- Base font 14 px; page rhythm and card look consistent with existing `ContainerCard` sections.
- Existing patterns to reuse, not reinvent: the `conn-pill` (status dot + label) idiom for the health pill; section label + card grid structure.
- Every interactive/hover affordance must degrade gracefully to static display (no information visible only on hover may be critical).
- Responsive: page is a fixed-width 1100 px column; panel should stack its sub-blocks gracefully below ~800 px viewport.
- Accessibility: status never encoded by color alone (always paired with text/badge); contrast per existing tokens.

### 7.4 Edge cases the design must look intentional in

- PSI unavailable (`psi: null`) → the three PSI meters collapse or render as an explicit "PSI unavailable" placeholder, not empty boxes.
- First sample after daemon start (all `cpu_pct: 0`) → table must not look broken.
- Swap present and heavily used (red health) → the swap element and health pill should compose into one clear story.
- 12-row table on a quiet machine (many tiny processes) → design should still read cleanly (whether tiny-RSS rows get visually de-emphasized is OpenDesign's call).

### 7.5 OpenDesign deliverables

1. Mock of the full panel at nominal state (green, moderate load).
2. Mock at red state (low memory + swap pressure + an OOM increment).
3. Mock of the offenders table with container badges.
4. Edge-state variants: PSI unavailable, no swap, first-sample.
5. Mapping of every element to a `data-od-id` for implementation (existing convention in this codebase).

### 7.6 Explicitly deferred (P1+)

Alerting (threshold notifications, toast/502-style warnings), history sparklines, per-container rollup rows, click-through from offender row to container card actions.

## 8. Error handling / degradation

- `/proc/pressure` missing → `psi: null`; UI hides PSI meters with placeholder (§7.4); health falls back to memory/swap.
- No swap → zeros in swap fields; UI shows "no swap" tag.
- pid disappears mid-scan → skip that pid.
- bollard list failure → `container: "unknown"`; meters unaffected.
- Collector error → skip that broadcast tick (UI keeps last sample); `GET /api/metrics` → 500 like existing handlers. EventSource auto-reconnect handles daemon restarts.

## 9. Testing

- Rust unit tests with fixture strings: meminfo parser, pressure parser, `memory.events` parser, cgroup-id extraction, CPU-delta math (incl. clock-wrap), health derivation truth table, RSS scaling.
- Frontend: manual verification against the running daemon (no FE test framework in this repo); component renders each §7.4 edge state via fixture JSON in dev mode.
- Constraint: no compiler in the sandbox — Rust tests run in the apps container (`ssh gem@apps` + nix `rustc`/`cargo`, documented path) and in the Build Orchestrator workflow (add `cargo test` step).

## 10. Deployment

Push → **Build Orchestrator** workflow (builds `ui/dist` into the binary via Vite, compiles musl binaries, uploads to `/opt/codery/codery-ci`, restarts host services). Exact restart coverage of `codery-ci-daemon` vs `codery-mcp` to be confirmed in the implementation plan; both serve the UI path so both must end up running the new binary.
