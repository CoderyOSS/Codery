use anyhow::{bail, Result};
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use bollard::Docker;
use bollard::system::EventsOptions;
use futures_util::StreamExt;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

use bollard::container::ListContainersOptions;
use crate::service_def::ServiceDef;
use crate::{caddy, config, deploy, state};

fn ts() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{:02}:{:02}:{:02}", (secs / 3600) % 24, (secs / 60) % 60, secs % 60)
}

// ── Types ─────────────────────────────────────────────────────────────────────

pub type RollbackLock = Arc<Mutex<HashSet<String>>>;
pub type Ops = Arc<Mutex<HashMap<String, &'static str>>>;

#[derive(Clone)]
pub struct AppState {
    pub rollback_lock: RollbackLock,
    pub events_tx:    Arc<broadcast::Sender<String>>,
    pub ops:          Ops,
}

#[derive(Serialize)]
pub struct ServiceStatus {
    pub name:               String,
    pub image:              String,
    pub status:             String,
    pub state:              String,
    pub service:            Option<String>,
    pub rollback_available: bool,
    pub prev_container:     Option<String>,
    pub operation:          Option<String>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn make_router(events_tx: Arc<broadcast::Sender<String>>, ops: Ops) -> Router {
    let state = AppState {
        rollback_lock: Arc::new(Mutex::new(HashSet::new())),
        events_tx,
        ops,
    };
    Router::new()
        .route("/", get(serve_index))
        .route("/api/status", get(get_status))
        .route("/api/events", get(get_events))
        .route("/api/stop/{container}",    post(post_stop))
        .route("/api/start/{container}",  post(post_start))
        .route("/api/kill/{container}",   post(post_kill))
        .route("/api/restart/{container}", post(post_restart))
        .route("/api/rollback/{service}", post(post_rollback))
        .with_state(state)
}

pub async fn serve(port: u16, events_tx: Arc<broadcast::Sender<String>>, ops: Ops) -> Result<()> {
    let addr = format!("127.0.0.1:{}", port);
    println!("[ui {}] Listening on http://{}", ts(), addr);
    let listener = TcpListener::bind(&addr).await?;
    axum::serve(listener, make_router(events_tx, ops)).await?;
    Ok(())
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn serve_index() -> impl IntoResponse {
    let html = include_str!("../ui/dist/index.html");
    (
        [
            (header::CONTENT_TYPE,  HeaderValue::from_static("text/html; charset=utf-8")),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        html,
    )
}

async fn get_events(
    State(state): State<AppState>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events_tx.subscribe();
    let ops_snap = state.ops.lock().unwrap().clone();
    let initial = build_status_json(&ops_snap).await.unwrap_or_else(|_| "[]".to_string());

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

async fn get_status(State(state): State<AppState>) -> impl IntoResponse {
    let ops_snap = state.ops.lock().unwrap().clone();
    match build_status(&ops_snap).await {
        Ok(statuses) => {
            let json = serde_json::to_string(&statuses).unwrap_or_else(|_| "[]".to_string());
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))],
                json,
            ).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

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

    let result = run_rollback(&service, &state.ops, &state.events_tx).await;

    {
        let mut set = state.rollback_lock.lock().unwrap();
        set.remove(&service);
    }

    match result {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── Status builder ────────────────────────────────────────────────────────────

pub async fn build_status_json(ops: &HashMap<String, &'static str>) -> Result<String> {
    let statuses = build_status(ops).await?;
    Ok(serde_json::to_string(&statuses)?)
}

async fn build_status(ops: &HashMap<String, &'static str>) -> Result<Vec<ServiceStatus>> {
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

        let operation = ops.get(&name).map(|s| s.to_string());

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
            operation,
        });
    }
    Ok(out)
}

/// Parse `codery-{service}-{color}` container names.
fn parse_service_container(name: &str) -> Option<(String, &'static str)> {
    if let Some(rest) = name.strip_prefix("codery-") {
        if let Some(svc) = rest.strip_suffix("-blue") {
            return Some((svc.to_string(), "blue"));
        }
        if let Some(svc) = rest.strip_suffix("-green") {
            return Some((svc.to_string(), "green"));
        }
    }
    None
}

fn peer_container_name(_name: &str, service: &str, peer_color: &str) -> String {
    config::container_name(service, peer_color)
}

async fn is_container_stopped(docker: &Docker, container: &str) -> bool {
    match docker.inspect_container(container, None).await {
        Ok(info) => !info.state.and_then(|s| s.running).unwrap_or(false),
        Err(_) => false,
    }
}

/// Extract short SHA from image tag `ghcr.io/coderyoss/codery:{service}-{sha}`.
fn sha_from_image_tag(service: &str, image: &str) -> Option<String> {
    let prefix = format!("{}:{}-", config::REGISTRY, service);
    image.strip_prefix(&prefix).map(|s| s.to_string())
}

// ── broadcast helper ──────────────────────────────────────────────────────────

async fn broadcast_status(tx: &broadcast::Sender<String>, ops: &Ops) {
    let ops_snap = ops.lock().unwrap().clone();
    let json = build_status_json(&ops_snap).await.unwrap_or_else(|_| "[]".to_string());
    let _ = tx.send(json);
}

// ── Event watcher ─────────────────────────────────────────────────────────────

/// Spawned once at daemon startup. Reconnects automatically if the Docker
/// event stream ends (e.g. daemon restart).
pub async fn event_watcher(tx: Arc<broadcast::Sender<String>>, ops: Ops) {
    loop {
        if let Err(e) = run_event_watcher(&tx, &ops).await {
            println!("[ui {}] Docker event stream error: {}", ts(), e);
        } else {
            println!("[ui {}] Docker event stream ended", ts());
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        println!("[ui {}] Reconnecting to Docker event stream…", ts());
    }
}

async fn run_event_watcher(tx: &broadcast::Sender<String>, ops: &Ops) -> Result<()> {
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
        let event = event?; // propagate Docker errors → outer loop reconnects

        let action = event.action.as_deref().unwrap_or("?");
        let cname  = event.actor.as_ref()
            .and_then(|a| a.attributes.as_ref())
            .and_then(|attrs| attrs.get("name"))
            .map(String::as_str)
            .unwrap_or("?");
        println!("[ui {}] event action={} name={}", ts(), action, cname);

        {
            let mut ops = ops.lock().unwrap();
            match action {
                // Container came up — always clear op.
                "start" => { ops.remove(cname); }
                // Container process exited — if we were stopping it, clear op.
                // If we sent kill, clear op. If stop is in flight, clear op too.
                "die" | "stop" => {
                    if matches!(ops.get(cname).map(|s| *s), Some("stopping") | Some("killing")) {
                        ops.remove(cname);
                    }
                }
                _ => {}
            }
        }

        let ops_snap = ops.lock().unwrap().clone();
        let tx2 = tx.clone();
        tokio::spawn(async move {
            let t0 = std::time::Instant::now();
            match tokio::time::timeout(
                tokio::time::Duration::from_secs(5),
                build_status_json(&ops_snap),
            ).await {
                Ok(Ok(json)) => {
                    println!("[ui {}] build_status ok ({:.1}s)", ts(), t0.elapsed().as_secs_f32());
                    let _ = tx2.send(json);
                }
                Ok(Err(e)) => eprintln!("[ui {}] build_status err ({:.1}s): {}", ts(), t0.elapsed().as_secs_f32(), e),
                Err(_)     => eprintln!("[ui {}] build_status TIMEOUT ({:.1}s)", ts(), t0.elapsed().as_secs_f32()),
            }
        });
    }

    Ok(())
}

// ── Stop / Start / Kill flows ─────────────────────────────────────────────────

async fn post_stop(
    State(state): State<AppState>,
    Path(container): Path<String>,
) -> impl IntoResponse {
    println!("[ui {}] POST /api/stop/{}", ts(), container);
    dispatch_container_op(container, "stopping", &["stop", "-t", "10"], state).await
}

async fn post_start(
    State(state): State<AppState>,
    Path(container): Path<String>,
) -> impl IntoResponse {
    println!("[ui {}] POST /api/start/{}", ts(), container);
    dispatch_container_op(container, "starting", &["start"], state).await
}

async fn post_kill(
    State(state): State<AppState>,
    Path(container): Path<String>,
) -> impl IntoResponse {
    println!("[ui {}] POST /api/kill/{}", ts(), container);
    dispatch_container_op(container, "killing", &["kill"], state).await
}

async fn post_restart(
    State(state): State<AppState>,
    Path(container): Path<String>,
) -> impl IntoResponse {
    println!("[ui {}] POST /api/restart/{}", ts(), container);
    dispatch_container_op(container, "restarting", &["restart", "-t", "10"], state).await
}

async fn dispatch_container_op(
    container: String,
    op: &'static str,
    docker_args: &'static [&'static str],
    state: AppState,
) -> impl IntoResponse {
    state.ops.lock().unwrap().insert(container.clone(), op);
    broadcast_status(&state.events_tx, &state.ops).await;

    let ops = Arc::clone(&state.ops);
    let tx  = Arc::clone(&state.events_tx);
    tokio::spawn(async move {
        let mut cmd = tokio::process::Command::new("docker");
        cmd.args(docker_args).arg(&container);
        match cmd.spawn() {
            Ok(_)  => println!("[ui {}] {} dispatched: {}", ts(), op, container),
            Err(e) => {
                eprintln!("[ui {}] {} failed to spawn: {}", ts(), op, e);
                ops.lock().unwrap().remove(&container);
                broadcast_status(&tx, &ops).await;
                return;
            }
        }
        // Safety net: clear op after 5 minutes if Docker events never arrive.
        tokio::time::sleep(tokio::time::Duration::from_secs(300)).await;
        ops.lock().unwrap().remove(&container);
        broadcast_status(&tx, &ops).await;
    });

    StatusCode::NO_CONTENT.into_response()
}

// ── Rollback flow ─────────────────────────────────────────────────────────────

async fn run_rollback(service: &str, ops: &Ops, tx: &broadcast::Sender<String>) -> Result<()> {
    let _lock = crate::deploy_lock::DeployLock::try_acquire(service)
        .map_err(|_| anyhow::anyhow!("deploy already in progress for {}", service))?;

    let def = ServiceDef::load(service)
        .map_err(|_| anyhow::anyhow!("unknown service: {}", service))?;

    let docker = Docker::connect_with_socket_defaults()?;

    let active_color = state::read_active(service)?;
    let prev_color = config::flip(&active_color);
    let prev_container = config::container_name(service, prev_color);
    let active_container = config::container_name(service, &active_color);

    ops.lock().unwrap().insert(active_container.clone(), "rolling_back");
    broadcast_status(tx, ops).await;

    let result = do_rollback(&docker, &def, service, prev_color, &prev_container, &active_container).await;

    ops.lock().unwrap().remove(&active_container);
    broadcast_status(tx, ops).await;

    result
}

async fn do_rollback(
    docker: &Docker,
    def: &ServiceDef,
    service: &str,
    prev_color: &str,
    prev_container: &str,
    active_container: &str,
) -> Result<()> {
    // Verify the previous container exists and is stopped
    let info = docker
        .inspect_container(prev_container, None)
        .await
        .map_err(|_| anyhow::anyhow!("no stopped container for {}", service))?;

    let running = info.state.as_ref().and_then(|s| s.running).unwrap_or(false);
    if running {
        bail!("container is already running — no rollback needed");
    }

    // Extract SHA from image tag
    let image = info
        .config
        .as_ref()
        .and_then(|c| c.image.as_deref())
        .unwrap_or("");
    let prev_sha = sha_from_image_tag(service, image)
        .unwrap_or_else(|| "unknown".to_string());

    println!("[ui {}] Rolling back {} to {} (sha={})", ts(), service, prev_color, prev_sha);

    // Start the previous container
    use bollard::container::StartContainerOptions;
    docker
        .start_container(prev_container, None::<StartContainerOptions<String>>)
        .await
        .map_err(|e| anyhow::anyhow!("failed to start {}: {}", prev_container, e))?;

    // Health check — on failure, stop the container we just started
    if let Err(e) = deploy::poll_health(docker, def, prev_color).await {
        let _ = deploy::stop_container(docker, prev_container).await;
        bail!("health check failed after restart: {}", e);
    }

    // Flip state
    state::write_active(service, prev_color)?;
    state::write_active_sha(service, &prev_sha)?;
    println!("[ui {}] State updated: {} is now {}", ts(), service, prev_color);

    // Reload Caddy
    caddy::apply_all()
        .map_err(|e| anyhow::anyhow!("caddy reload failed after state write: {}", e))?;

    // Stop the now-old active container (only reached if Caddy reload succeeded)
    deploy::stop_container(docker, active_container).await?;

    println!("[ui {}] Rollback complete: {} is now {}", ts(), service, prev_color);
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::container::{
        Config, CreateContainerOptions, RemoveContainerOptions,
        StartContainerOptions, StopContainerOptions,
    };

    async fn start_test_server() -> u16 {
        let (tx, _) = tokio::sync::broadcast::channel::<String>(16);
        let ops: Ops = Arc::new(Mutex::new(HashMap::new()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, make_router(Arc::new(tx), ops)).await.unwrap()
        });
        port
    }

    /// E2E: start a real Docker container, hit /api/status, verify it appears
    /// with the correct flat shape {name, image, status, state}.
    #[tokio::test]
    async fn status_lists_running_containers() {
        let docker = match Docker::connect_with_socket_defaults() {
            Ok(d) => d,
            Err(_) => return, // skip if Docker unavailable
        };
        if docker.ping().await.is_err() {
            return;
        }

        let cname = "codery-test-ui-status";

        // Clean up any leftover from a previous run
        let _ = docker.remove_container(
            cname,
            Some(RemoveContainerOptions { force: true, ..Default::default() }),
        ).await;

        docker.create_container(
            Some(CreateContainerOptions { name: cname, platform: None }),
            Config {
                image: Some("rust:latest"),
                cmd: Some(vec!["sleep", "60"]),
                ..Default::default()
            },
        ).await.expect("create test container");

        docker.start_container(cname, None::<StartContainerOptions<String>>)
            .await.expect("start test container");

        let port = start_test_server().await;
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let resp = reqwest::get(format!("http://127.0.0.1:{}/api/status", port))
            .await.expect("GET /api/status");

        assert!(resp.status().is_success(), "expected 200, got {}", resp.status());

        let body: serde_json::Value = resp.json().await.expect("parse JSON");
        let arr = body.as_array().expect("expected JSON array");

        // Every element must carry the four fields the JS renders
        for item in arr {
            assert!(item["name"].as_str().is_some(),   "missing 'name' in {}", item);
            assert!(item["image"].as_str().is_some(),  "missing 'image' in {}", item);
            assert!(item["status"].as_str().is_some(), "missing 'status' in {}", item);
            assert!(item["state"].as_str().is_some(),  "missing 'state' in {}", item);
        }

        // Our container must appear as running
        let found = arr.iter().any(|c| {
            c["name"].as_str() == Some(cname) && c["state"].as_str() == Some("running")
        });

        // Cleanup before asserting so we don't leave orphans on failure
        let _ = docker.stop_container(cname, Some(StopContainerOptions { t: 0 })).await;
        let _ = docker.remove_container(cname, None).await;

        assert!(found, "container '{}' not in /api/status\ngot: {}", cname, body);
    }
}
