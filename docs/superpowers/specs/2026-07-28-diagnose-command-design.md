# `codery-ci diagnose` — Mismatch Detector

**Status:** Approved
**Date:** 2026-07-28
**Goal:** Give the AI agent (and operators) a single command that surfaces
infrastructure mismatches and prints the exact command to fix each one.

## Problem

The sandbox MCP tools report state from multiple sources that can disagree:

| Tool | Source |
|------|--------|
| `get_status` | Docker container inspection (live reality) |
| `get_routes` | `/opt/codery/state/{service}.color` (state file) |
| `reload_routes` | Same state file (regenerates Caddyfile from it) |
| Caddyfile | Written by `reload_routes` from state file |

When the state file disagrees with Docker reality (e.g., a `deploy-preview`
was started but never `cutover`-ed), the result is silent breakage:

- `get_status` reports the running container as active.
- `get_routes` and the live Caddyfile still route to the dead color.
- `reload_routes` returns `{"status": "ok"}` despite changing nothing.

In a recent incident, this caused the agent to suggest manual state-file
edits instead of the existing `cutover` subcommand. The docs were correct;
the failure was that no tool surfaced the mismatch or pointed to the fix.

## Design

A new read-only subcommand `codery-ci diagnose` and matching MCP tool
`diagnose` that:

1. Cross-checks the state file against running Docker containers.
2. Checks each route target port has a TCP listener (`ss -tlnp`).
3. Lists uncut-over previews (`deploy-preview` without `cutover`).
4. Returns a structured report with a `fix` field per issue containing
   the exact shell command to resolve it.

Exit code 1 if any unhealthy, 0 otherwise. Lets scripts and CI gate on it.

## Checks

### Service color mismatch

For each service in `ServiceDef::load_all()`:

- Read `state::read_active(svc)` (state file color).
- Inspect `codery-{svc}-blue` and `codery-{svc}-green` via `bollard`.
- Determine the running color (whichever container has `state.running == true`).

| State file | Docker reality | Severity | Fix |
|------------|----------------|----------|-----|
| `blue`     | blue running   | OK       | —   |
| `green`    | green running  | OK       | —   |
| `blue`     | green running (only) | Mismatch | `codery-ci cutover <svc>` |
| `green`    | blue running (only)  | Mismatch | `codery-ci cutover <svc>` |
| either     | neither running | Dead     | `restart_service` MCP, or `deploy <svc> <sha>` |
| either     | both running    | Info     | (transient — usually mid-deploy) |

### Route target listener

For each route in `db::build_route_map()`:

- Compute the host port using `ServiceDef::port_scheme.host_port(color, port)`.
- Look up the port in the parsed `ss -tlnp` output (one call, results cached).
- If no listener matches, mark the route unhealthy.

Fix hint depends on the source service:
- Sandbox/app container color mismatch → `codery-ci cutover <svc>`.
- Host service (Caddy, MCP) → note as host-layer issue.

### Preview stale

For each entry in `db::list_previews()`:

- A preview record exists, meaning `deploy-preview` ran and `cutover`
  has not yet been called for that service.
- Severity: `Stale` (informational, but worth flagging).
- Fix options shown: `codery-ci cutover <svc>` to promote, or
  `codery-ci cancel-preview <svc>` to abort.

## Output shapes

### CLI (human, stderr-friendly)

```
[sandbox] MISMATCH  state=green  running=blue  container=codery-sandbox-blue (up 33h)
[sandbox]   image   sandbox-nixos-v1
[sandbox]   fix     codery-ci cutover sandbox
[routes]   opencode.rancidgrandmas.online → :23000  NO LISTENER
[routes]   opendesign.rancidgrandmas.online → :27456  NO LISTENER
[previews] sandbox-preview  sha=sandbox-nixos-v1  color=blue
[previews]   ready to promote:  codery-ci cutover sandbox
[previews]   or abort:          codery-ci cancel-preview sandbox
[apps]     OK  state=blue  running=blue

3 issues found. Run with --json for machine-readable output.
```

Exit code 1.

### MCP / `--json`

```json
{
  "all_healthy": false,
  "issue_count": 3,
  "services": [
    {
      "service": "sandbox",
      "severity": "mismatch",
      "state_color": "green",
      "running_color": "blue",
      "running_container": "codery-sandbox-blue",
      "image": "sandbox-nixos-v1",
      "uptime": "33h",
      "fix": ["codery-ci cutover sandbox"]
    },
    {
      "service": "apps",
      "severity": "ok",
      "state_color": "blue",
      "running_color": "blue",
      "running_container": "codery-apps-blue"
    }
  ],
  "routes": [
    {"subdomain": "opencode.rancidgrandmas.online", "host_port": 23000, "listening": false},
    {"subdomain": "opendesign.rancidgrandmas.online", "host_port": 27456, "listening": false}
  ],
  "previews": [
    {
      "service": "sandbox",
      "sha": "sandbox-nixos-v1",
      "color": "blue",
      "fix": ["codery-ci cutover sandbox", "codery-ci cancel-preview sandbox"]
    }
  ],
  "guidance": {
    "what": "Found 3 issues. Each issue has a 'fix' array with exact shell commands.",
    "to_run_fix": "Run the listed command on the host shell. cutover/cancel-preview are never exposed via MCP."
  }
}
```

## Implementation

### Files

| File | Change |
|------|--------|
| `system/orchestrator/src/diagnose.rs` | **NEW** — core logic + tests |
| `system/orchestrator/src/main.rs` | `mod diagnose;`, subcommand arm, `--help` dispatch |
| `system/orchestrator/src/mcp.rs` | `diagnose` MCP tool, enrich `reload_routes` / `get_routes` / `get_status` responses |
| `AGENTS.md` | Quick-reference card at top |

### `diagnose.rs` structure

```rust
pub struct DiagnoseReport {
    pub services: Vec<ServiceIssue>,
    pub routes: Vec<RouteIssue>,
    pub previews: Vec<PreviewIssue>,
    pub all_healthy: bool,
    pub issue_count: usize,
}

pub enum Severity { Ok, Mismatch, Dead, Stale, Info }

pub struct ServiceIssue {
    pub service: String,
    pub severity: Severity,
    pub state_color: String,
    pub running_color: Option<String>,
    pub running_container: Option<String>,
    pub image: Option<String>,
    pub uptime: Option<String>,
    pub fix: Vec<String>,
}

pub struct RouteIssue {
    pub subdomain: String,
    pub host_port: u16,
    pub listening: bool,
    pub fix: Vec<String>,
}

pub struct PreviewIssue {
    pub service: String,
    pub sha: String,
    pub color: String,
    pub fix: Vec<String>,
}

pub async fn run() -> Result<DiagnoseReport>;
impl DiagnoseReport {
    pub fn format_human(&self) -> String;
}
```

### Logic

1. Load all service defs.
2. One `ss -tlnp` call via `tokio::process::Command`, parse to `HashSet<u16>` of listening ports.
3. For each service:
   - Read state color (`state::read_active`).
   - Inspect blue + green containers via `bollard`.
   - Determine running color from Docker state.
   - Build `ServiceIssue`.
4. For each route in `db::build_route_map`:
   - Compute host_port using service's `port_scheme.host_port(state_color, port)`.
   - `listening = ports.contains(host_port)`.
   - Build `RouteIssue` (only push if `!listening`).
5. For each preview in `db::list_previews`:
   - Build `PreviewIssue` with cutover + cancel-preview fix options.
6. `all_healthy = routes.is_empty() && !services.iter().any(|s| !matches!(s.severity, Severity::Ok | Severity::Info)) && previews.is_empty()`.

### Testing

Unit tests in `diagnose.rs` covering pure logic via injected state:

- `detects_color_mismatch_when_state_green_running_blue`
- `detects_dead_route_target`
- `flags_uncutovered_preview`
- `clean_state_returns_all_healthy`
- `both_containers_down_reports_dead`
- `both_containers_up_reports_info_only`

Pure-logic parts (severity classification, fix generation, host_port math)
factored into free functions taking primitives — Docker/socket interaction
wrapped in injectable traits so tests don't need real Docker.

## CLI subcommand

```rust
Some("diagnose") => {
    let as_json = args.iter().any(|a| a == "--json");
    let report = diagnose::run().await?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{}", report.format_human());
    }
    if !report.all_healthy { std::process::exit(1); }
}
```

Usage: `codery-ci diagnose [--json]`

## MCP tool

```rust
#[tool(description = "Diagnose mismatches between state file, running containers, \
                       route targets, and preview deploys. Returns structured report \
                       with fix commands for each issue. Read-only — run anytime \
                       something looks wrong.")]
async fn diagnose(&self) -> Result<CallToolResult, McpError> {
    let report = crate::diagnose::run().await.map_err(|e| tool_err(e.to_string()))?;
    let json = serde_json::to_string_pretty(&report).map_err(|e| tool_err(e.to_string()))?;
    tool_ok(json)
}
```

## Enriched tool responses (existing tools)

### `reload_routes`

Capture Caddyfile bytes before and after, return `changed` / `unchanged`:

```json
{
  "status": "unchanged",
  "reason": "state file unchanged since last reload",
  "diagnose_hint": "run `diagnose` to detect state vs Docker mismatches"
}
```

### `get_routes`

For each route entry, add:

```json
{"healthy": true}
```

Based on one shared `ss -tlnp` lookup at the top of the handler.

### `get_status`

Add `state_file_color` alongside the existing `active_color` (which reflects
Docker reality):

```json
{
  "service": "sandbox",
  "active_color": "blue",
  "state_file_color": "green",
  "container": "codery-sandbox-blue",
  "running": true
}
```

Mismatches between the two fields are the canonical "stale routing" signal.

## Per-subcommand `--help`

Add a `--help` / `-h` short-circuit before the subcommand match in `main.rs`:

```rust
if let Some(sub) = args.get(1) {
    if args.get(2).map(|s| s.as_str()) == Some("--help")
        || args.get(2).map(|s| s.as_str()) == Some("-h") {
        print_help(sub);
        return Ok(());
    }
}
```

Each subcommand's `print_help` arm prints: usage line, flags, example,
AGENTS.md section link.

## AGENTS.md quick-reference card

Inserted after the "What This Repo Is" section, before "Container Roles":

```markdown
## Common Operations — Quick Reference

| Situation | Command / Tool |
|-----------|----------------|
| Something broken, unknown cause | `codery-ci diagnose` (or `diagnose` MCP tool) |
| Routing points to dead container | `codery-ci cutover <service>` |
| Promote a verified preview | `codery-ci cutover <service>` |
| Abort a preview deploy | `codery-ci cancel-preview <service>` |
| Build + try new image locally | `build` → `deploy-preview` → `cutover` |
| Reload routing after YAML edit | `codery-ci reload-routes` (or `reload_routes` MCP) |
| Restart stuck container | `restart_service` MCP (no blue/green swap) |
| Roll back to previous image | `rollback` MCP |
| State vs reality disagree | `codery-ci diagnose` shows mismatch + fix |

Each subcommand has focused help: `codery-ci <command> --help`.
```

## Build & deploy

Sandbox container has no compiler (per AGENTS.md). Implementation flow:

1. Edit files in sandbox (bind-mounted to host's `/opt/codery/projects/Codery`).
2. `github-push master`.
3. Trigger `Build Orchestrator` workflow manually.
4. Workflow compiles musl binary, uploads to `/opt/codery/codery-ci`,
   restarts `codery-mcp`.
5. Verify with `codery-ci diagnose` (CLI on host) and `diagnose` MCP tool.

## Verification checklist

- [ ] `codery-ci diagnose` detects color mismatch + dead route + stale preview
- [ ] `codery-ci diagnose --json` returns valid JSON matching the documented shape
- [ ] Exit code 1 when unhealthy, 0 when healthy
- [ ] `diagnose` MCP tool returns same data as CLI
- [ ] `reload_routes` MCP returns `changed`/`unchanged` status
- [ ] `get_routes` MCP includes `healthy: bool` per route
- [ ] `get_status` MCP includes `state_file_color` field
- [ ] `codery-ci <subcommand> --help` prints focused usage for each subcommand
- [ ] AGENTS.md quick-ref card visible at top
- [ ] Unit tests pass via `Build Orchestrator` workflow

## Risk / rollback

- All changes additive — no existing tool signatures change.
- `diagnose` is read-only, cannot affect deploys or routing.
- Enriched responses only add new fields; existing consumers unaffected.
- Rollback: revert commit, redeploy orchestrator.
