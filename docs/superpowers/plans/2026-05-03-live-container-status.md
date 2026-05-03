# Live Container Status (Docker Events → SSE) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the static `/api/status` fetch with a Docker-event-driven SSE stream so the UI updates the moment a container starts, stops, or restarts — with no polling anywhere.

**Architecture:** A persistent `event_watcher` tokio task subscribes to `docker.events()` (bollard), filters for container lifecycle events, and on each event broadcasts a full `list_containers(all: true)` snapshot over a `tokio::sync::broadcast` channel. An `/api/events` SSE endpoint fans that channel out to connected browsers. The frontend replaces `loadStatus()` with `EventSource('/api/events')` and re-renders the full card list on each message, grouping running containers above stopped ones (sorted alpha within each group).

**Tech Stack:** Rust/Axum 0.8 (sse feature), bollard 0.17, tokio broadcast channel, browser EventSource API.

---

## File Map

| File | Change |
|------|--------|
| `system/orchestrator/Cargo.toml` | Add `"sse"` to axum features |
| `system/orchestrator/src/ui.rs` | Add `AppState`, `event_watcher`, `/api/events` handler; update `build_status` to `all: true`; update `make_router` + `serve` signatures |
| `system/orchestrator/src/daemon.rs` | Create broadcast channel, spawn `event_watcher`, pass `events_tx` to `ui::serve` |
| `system/orchestrator/src/ui.html` | EventSource, grouped+sorted `renderCards`, `.card.inactive` CSS, connection dot |

---

## Task 1: Enable axum SSE feature and add AppState

**Files:**
- Modify: `system/orchestrator/Cargo.toml`
- Modify: `system/orchestrator/src/ui.rs` (Types section, lines 17–30)

- [ ] **Step 1: Add `"sse"` to axum features in Cargo.toml**

```toml
axum = { version = "0.8", default-features = false, features = ["http1", "tokio", "sse"] }
```

- [ ] **Step 2: Replace `RollbackLock`-only state with `AppState` in ui.rs**

Replace the Types section (keep `RollbackLock` type alias, add `AppState`):

```rust
use tokio::sync::broadcast;

pub type RollbackLock = Arc<Mutex<HashSet<String>>>;

#[derive(Clone)]
pub struct AppState {
    pub rollback_lock: RollbackLock,
    pub events_tx:    Arc<broadcast::Sender<String>>,
}
```

- [ ] **Step 3: Update `make_router` to accept `events_tx` and use `AppState`**

```rust
pub fn make_router(events_tx: Arc<broadcast::Sender<String>>) -> Router {
    let state = AppState {
        rollback_lock: Arc::new(Mutex::new(HashSet::new())),
        events_tx,
    };
    Router::new()
        .route("/", get(serve_index))
        .route("/api/status", get(get_status))
        .route("/api/events", get(get_events))
        .route("/api/rollback/{service}", post(post_rollback))
        .route("/api/restart/{container}", post(post_restart))
        .with_state(state)
}
```

- [ ] **Step 4: Update `serve` to accept and forward `events_tx`**

```rust
pub async fn serve(port: u16, events_tx: Arc<broadcast::Sender<String>>) -> Result<()> {
    let addr = format!("127.0.0.1:{}", port);
    println!("[ui] Listening on http://{}", addr);
    let listener = TcpListener::bind(&addr).await?;
    axum::serve(listener, make_router(events_tx)).await?;
    Ok(())
}
```

- [ ] **Step 5: Update all handlers to extract `State(state): State<AppState>` instead of `State(lock): State<RollbackLock>`**

`post_rollback` — change the state extraction and lock access:
```rust
async fn post_rollback(
    State(state): State<AppState>,
    Path(service): Path<String>,
) -> impl IntoResponse {
    {
        let mut set = state.rollback_lock.lock().unwrap();
        if set.contains(&service) {
            return (
                StatusCode::CONFLICT,
                format!("rollback already in progress for {}", service),
            ).into_response();
        }
        set.insert(service.clone());
    }

    let result = run_rollback(&service).await;

    {
        let mut set = state.rollback_lock.lock().unwrap();
        set.remove(&service);
    }

    match result {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
```

`post_restart` — state is now `AppState` (unused field is fine):
```rust
async fn post_restart(
    State(_state): State<AppState>,
    Path(container): Path<String>,
) -> impl IntoResponse {
    match do_restart(&container).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
```

- [ ] **Step 6: Verify it compiles**

```bash
cd /tmp/Codery/system/orchestrator
CARGO_TARGET_DIR=/tmp/orchestrator-target cargo check 2>&1 | grep -E "^error"
```

Expected: no output (no errors). The test will fail to compile until Task 4 — that's fine for now.

---

## Task 2: Update `build_status` to include stopped containers

**Files:**
- Modify: `system/orchestrator/src/ui.rs` (build_status function)

- [ ] **Step 1: Change `all: false` to `all: true` and extract `build_status_json` helper**

Replace the Status builder section with:

```rust
// ── Status builder ────────────────────────────────────────────────────────────

pub async fn build_status_json() -> Result<String> {
    let statuses = build_status().await?;
    Ok(serde_json::to_string(&statuses)?)
}

async fn build_status() -> Result<Vec<ServiceStatus>> {
    let docker = Docker::connect_with_socket_defaults()?;
    let containers = docker.list_containers(Some(ListContainersOptions::<String> {
        all: true, // running AND stopped
        ..Default::default()
    })).await?;

    let mut out = Vec::new();
    for c in containers {
        let name = c.names
            .unwrap_or_default()
            .into_iter()
            .next()
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_string();

        let (service, rollback_available, prev_container) =
            if let Some((svc, color)) = parse_service_container(&name) {
                let peer_color = config::flip(color);
                let peer = peer_container_name(&name, &svc, peer_color);
                let stopped = is_container_stopped(&docker, &peer).await;
                (Some(svc), stopped, if stopped { Some(peer) } else { None })
            } else {
                (None, false, None)
            };

        out.push(ServiceStatus {
            name,
            image:              c.image.unwrap_or_default(),
            status:             c.status.unwrap_or_default(),
            state:              c.state.unwrap_or_default(),
            service,
            rollback_available,
            prev_container,
        });
    }
    Ok(out)
}
```

- [ ] **Step 2: Compile check**

```bash
cd /tmp/Codery/system/orchestrator
CARGO_TARGET_DIR=/tmp/orchestrator-target cargo check 2>&1 | grep -E "^error"
```

Expected: no output.

---

## Task 3: Add `event_watcher` and `/api/events` SSE endpoint

**Files:**
- Modify: `system/orchestrator/src/ui.rs`

- [ ] **Step 1: Add SSE imports at the top of ui.rs**

Add to the existing `use` block:
```rust
use axum::response::sse::{Event, KeepAlive, Sse};
use bollard::system::EventsOptions;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::convert::Infallible;
use tokio::sync::broadcast;
```

- [ ] **Step 2: Add `event_watcher` and `run_event_watcher` functions**

Add this section after the Status builder section:

```rust
// ── Event watcher ─────────────────────────────────────────────────────────────

/// Spawned once at daemon startup. Reconnects automatically if the Docker
/// event stream ends (e.g. daemon restart).
pub async fn event_watcher(tx: Arc<broadcast::Sender<String>>) {
    loop {
        if let Err(e) = run_event_watcher(&tx).await {
            println!("[ui] Docker event stream error: {}", e);
        } else {
            println!("[ui] Docker event stream ended");
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        println!("[ui] Reconnecting to Docker event stream…");
    }
}

async fn run_event_watcher(tx: &broadcast::Sender<String>) -> Result<()> {
    let docker = Docker::connect_with_socket_defaults()?;

    let filters: HashMap<String, Vec<String>> = [(
        "type".to_string(),
        vec!["container".to_string()],
    )].into_iter().collect();

    let mut stream = docker.events(Some(EventsOptions::<String> {
        filters,
        ..Default::default()
    }));

    while let Some(event) = stream.next().await {
        let _ = event?; // propagate Docker errors → outer loop reconnects
        let snapshot = build_status_json().await.unwrap_or_else(|_| "[]".to_string());
        let _ = tx.send(snapshot); // ignore Err (no receivers connected)
    }

    Ok(())
}
```

- [ ] **Step 3: Add `/api/events` SSE handler**

Add this after the Handlers section:

```rust
async fn get_events(
    State(state): State<AppState>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events_tx.subscribe();
    let initial = build_status_json().await.unwrap_or_else(|_| "[]".to_string());

    let stream = futures_util::stream::unfold(
        (rx, Some(initial)),
        |(mut rx, initial)| async move {
            if let Some(json) = initial {
                return Some((Ok(Event::default().data(json)), (rx, None)));
            }
            loop {
                match rx.recv().await {
                    Ok(json) => {
                        return Some((Ok(Event::default().data(json)), (rx, None)));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );

    Sse::new(stream).keep_alive(KeepAlive::default())
}
```

- [ ] **Step 4: Compile check**

```bash
cd /tmp/Codery/system/orchestrator
CARGO_TARGET_DIR=/tmp/orchestrator-target cargo check 2>&1 | grep -E "^error"
```

Expected: no output.

---

## Task 4: Wire event_watcher into daemon startup and fix tests

**Files:**
- Modify: `system/orchestrator/src/daemon.rs`
- Modify: `system/orchestrator/src/ui.rs` (test helper)

- [ ] **Step 1: Update daemon.rs to create the channel and spawn the watcher**

```rust
use anyhow::Result;
use std::sync::Arc;
use crate::{config, mcp, tcp_proxy, ui};

pub async fn serve() -> Result<()> {
    crate::open_port_for_docker_bridges(config::MCP_PORT);

    println!(
        "[daemon] Starting codery-ci daemon: MCP=:{} UI=:{} TCP-proxy",
        config::MCP_PORT,
        config::UI_PORT
    );

    let (events_tx, _) = tokio::sync::broadcast::channel::<String>(32);
    let events_tx = Arc::new(events_tx);

    tokio::spawn(ui::event_watcher(Arc::clone(&events_tx)));

    tokio::select! {
        r = mcp::serve(config::MCP_PORT)                           => r?,
        r = ui::serve(config::UI_PORT, Arc::clone(&events_tx))     => r?,
        r = tcp_proxy::serve()                                     => r?,
        _ = shutdown_signal()                                      => {
            println!("[daemon] Received shutdown signal — stopping");
        }
    }

    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c().await.unwrap_or(())
    };

    #[cfg(unix)]
    let sigterm = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let sigterm = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c  => {}
        _ = sigterm => {}
    }
}
```

- [ ] **Step 2: Update `start_test_server` in ui.rs tests to pass a dummy channel**

```rust
async fn start_test_server() -> u16 {
    let (tx, _) = tokio::sync::broadcast::channel::<String>(16);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, make_router(Arc::new(tx))).await.unwrap()
    });
    port
}
```

- [ ] **Step 3: Run all tests**

```bash
cd /tmp/Codery/system/orchestrator
CARGO_TARGET_DIR=/tmp/orchestrator-target cargo test -- --nocapture 2>&1 | tail -15
```

Expected:
```
test ui::tests::status_lists_running_containers ... ok
test result: ok. 1 passed; 0 failed; ...
```

- [ ] **Step 4: Commit backend**

```bash
cd /tmp/Codery
git add system/orchestrator/Cargo.toml \
        system/orchestrator/Cargo.lock \
        system/orchestrator/src/ui.rs \
        system/orchestrator/src/daemon.rs
git commit -m "feat(ui): add Docker event stream → SSE backend"
```

---

## Task 5: Update frontend

**Files:**
- Modify: `system/orchestrator/src/ui.html`

- [ ] **Step 1: Add `.card.inactive` and connection-dot CSS**

Add after the existing `.card.unavailable` rule (line ~18):
```css
.card.inactive { opacity: 0.5; }
```

Add after the existing `.spin` rule:
```css
.conn-dot { font-size: 0.65rem; vertical-align: middle; margin-left: 6px; }
.conn-live { color: #4caf50; }
.conn-dead { color: #555; }
```

- [ ] **Step 2: Add connection dot to the header HTML**

Change:
```html
<div class="header">
  <h1>Codery Deploy Console</h1>
  <span class="header-sub">Deploy Console</span>
</div>
```

To:
```html
<div class="header">
  <h1>Codery Deploy Console <span id="conn-dot" class="conn-dot conn-dead" title="Connecting…">●</span></h1>
  <span class="header-sub">Deploy Console</span>
</div>
```

- [ ] **Step 3: Replace the entire `<script>` block**

```html
<script>
const rollbackInFlight = new Set();
const restartInFlight  = new Set();

function escHtml(s) {
  return String(s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

function cid(name) { return 'card-' + name; }

// ── Connection status ──────────────────────────────────────────────────────────

function setConnStatus(live) {
  const el = document.getElementById('conn-dot');
  el.className = 'conn-dot ' + (live ? 'conn-live' : 'conn-dead');
  el.title     = live ? 'Live' : 'Reconnecting…';
}

// ── Render ─────────────────────────────────────────────────────────────────────

function renderCards(containers) {
  const el = document.getElementById('cards');
  el.innerHTML = '';
  if (!containers.length) {
    el.innerHTML = '<p style="color:#666">No containers</p>';
    return;
  }
  const alpha = (a, b) => a.name.localeCompare(b.name);
  const running  = containers.filter(c => c.state === 'running').sort(alpha);
  const inactive = containers.filter(c => c.state !== 'running').sort(alpha);
  for (const c of [...running, ...inactive]) el.appendChild(buildCard(c));
}

function buildCard(c) {
  const id      = cid(c.name);
  const card    = document.createElement('div');
  card.id        = id;
  card.className = 'card' + (c.state !== 'running' ? ' inactive' : '');

  const stateClass = c.state === 'running' ? 'badge-green' : 'badge-blue';

  const rollbackBtn = c.rollback_available
    ? `<button class="btn btn-rollback" id="${id}-rbtn"
         onclick="handleRollback(${JSON.stringify(c.service)},${JSON.stringify(c.name)})"
       >↩ Rollback</button>`
    : '';

  const prevRow = c.prev_container
    ? `<div class="meta-row"><span class="lbl">rollback&nbsp;target:</span> <span class="val">${escHtml(c.prev_container)}</span></div>`
    : '';

  card.innerHTML = `
    <div class="card-inner">
      <div class="card-left">
        <div class="service-row">
          <span class="service-name">${escHtml(c.name)}</span>
          <span class="badge ${stateClass}">● ${escHtml(c.state.toUpperCase())}</span>
        </div>
        <div class="meta">
          <div class="meta-row"><span class="lbl">image:</span>  <span class="val">${escHtml(c.image)}</span></div>
          <div class="meta-row"><span class="lbl">status:</span> <span class="val">${escHtml(c.status)}</span></div>
          ${prevRow}
        </div>
      </div>
      <div class="card-right">
        <div class="status-msg" id="${id}-status"></div>
        <div class="btn-row">
          <button class="btn btn-restart" id="${id}-restart"
            onclick="handleRestart(${JSON.stringify(c.name)})"
          >↺ Restart</button>
          ${rollbackBtn}
        </div>
      </div>
    </div>`;
  return card;
}

// ── Shared card state helpers ──────────────────────────────────────────────────

function cardEls(name, suffix) {
  const id = cid(name);
  return {
    card:   document.getElementById(id),
    btn:    document.getElementById(id + suffix),
    status: document.getElementById(id + '-status'),
  };
}

function setProgress(els, label, msg) {
  els.card.classList.add('in-progress');
  els.btn.disabled = true;
  els.btn.className = 'btn btn-in-progress';
  els.btn.textContent = label;
  els.status.className = 'status-msg spinning';
  els.status.innerHTML = '<span class="spin">⟳</span> ' + escHtml(msg);
}

function setSuccess(els, label, msg) {
  els.card.classList.remove('in-progress');
  els.card.classList.add('success');
  els.btn.disabled = true;
  els.btn.className = 'btn btn-done';
  els.btn.textContent = label;
  els.status.className = 'status-msg done';
  els.status.textContent = msg;
}

function setError(els, origClass, origLabel, msg) {
  els.card.classList.remove('in-progress');
  els.card.classList.add('error');
  els.btn.disabled = false;
  els.btn.className = 'btn ' + origClass;
  els.btn.textContent = origLabel;
  els.status.className = 'status-msg err';
  els.status.textContent = '✗ ' + msg;
}

// ── Restart ────────────────────────────────────────────────────────────────────

async function handleRestart(name) {
  if (restartInFlight.has(name)) return;
  restartInFlight.add(name);
  const els = cardEls(name, '-restart');
  els.card.classList.remove('success', 'error');
  setProgress(els, '↺ Restarting…', 'Restarting…');
  try {
    const resp = await fetch('/api/restart/' + encodeURIComponent(name), { method: 'POST' });
    if (resp.ok) {
      setSuccess(els, '✓ Restarted', '✓ Restart complete');
      setTimeout(() => restartInFlight.delete(name), 1500);
    } else {
      const text = await resp.text();
      restartInFlight.delete(name);
      setError(els, 'btn-restart', '↺ Restart', text || 'restart failed');
    }
  } catch (err) {
    restartInFlight.delete(name);
    setError(els, 'btn-restart', '↺ Restart', 'network error');
  }
}

// ── Rollback ───────────────────────────────────────────────────────────────────

async function handleRollback(service, name) {
  if (rollbackInFlight.has(service)) return;
  rollbackInFlight.add(service);
  const els = cardEls(name, '-rbtn');
  els.card.classList.remove('success', 'error');
  setProgress(els, '↩ Rolling back…', 'Rolling back…');
  try {
    const resp = await fetch('/api/rollback/' + encodeURIComponent(service), { method: 'POST' });
    if (resp.ok) {
      setSuccess(els, '✓ Done', '✓ Rollback complete');
      setTimeout(() => rollbackInFlight.delete(service), 500);
    } else {
      const text = await resp.text();
      rollbackInFlight.delete(service);
      setError(els, 'btn-rollback', '↩ Rollback', text || 'rollback failed');
    }
  } catch (err) {
    rollbackInFlight.delete(service);
    setError(els, 'btn-rollback', '↩ Rollback', 'network error');
  }
}

// ── SSE connection ─────────────────────────────────────────────────────────────

function loadStatus() {
  fetch('/api/status')
    .then(r => r.json())
    .then(renderCards)
    .catch(() => {
      document.getElementById('cards').innerHTML =
        '<p style="color:#ef4444">Failed to load status</p>';
    });
}

const es = new EventSource('/api/events');
es.onmessage = (e) => {
  setConnStatus(true);
  try { renderCards(JSON.parse(e.data)); } catch (_) {}
};
es.onerror = () => setConnStatus(false);
</script>
```

- [ ] **Step 4: Build, SCP, and manually verify**

```bash
cd /tmp/Codery/system/orchestrator
CARGO_TARGET_DIR=/tmp/orchestrator-target cargo build --release 2>&1 | grep -E "^error|Finished"
scp /tmp/orchestrator-target/release/codery-ci vps:/tmp/codery-ci-test
ssh vps "sudo mv /tmp/codery-ci-test /opt/codery/codery-ci && sudo supervisorctl restart codery-ci-daemon && sleep 2 && sudo supervisorctl status codery-ci-daemon"
```

Then open the UI in a browser. Verify:
- Connection dot turns green (live)
- `willow-apps-green` card appears in active section
- SSH to VPS, run `docker stop willow-apps-green` — card should move to inactive section within a second
- Run `docker start willow-apps-green` — card moves back to active

- [ ] **Step 5: Run tests**

```bash
cd /tmp/Codery/system/orchestrator
CARGO_TARGET_DIR=/tmp/orchestrator-target cargo test -- --nocapture 2>&1 | tail -10
```

Expected: `test result: ok. 1 passed; 0 failed`

- [ ] **Step 6: Commit frontend and tag**

```bash
cd /tmp/Codery
git add system/orchestrator/src/ui.html
git commit -m "feat(ui): live container status via EventSource, grouped active/inactive"
git tag codery-ci-v0.1.6
git push origin master codery-ci-v0.1.6
```
