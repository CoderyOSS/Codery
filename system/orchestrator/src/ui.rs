use anyhow::{bail, Result};
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use bollard::Docker;
use serde::Serialize;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

use bollard::container::ListContainersOptions;
use crate::service_def::ServiceDef;
use crate::{caddy, config, deploy, state};

// ── Types ─────────────────────────────────────────────────────────────────────

pub type RollbackLock = Arc<Mutex<HashSet<String>>>;

#[derive(Serialize)]
pub struct ServiceStatus {
    pub name:   String,
    pub image:  String,
    pub status: String,
    pub state:  String,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn make_router() -> Router {
    let lock: RollbackLock = Arc::new(Mutex::new(HashSet::new()));
    Router::new()
        .route("/", get(serve_index))
        .route("/api/status", get(get_status))
        .route("/api/rollback/{service}", post(post_rollback))
        .with_state(lock)
}

pub async fn serve(port: u16) -> Result<()> {
    let addr = format!("127.0.0.1:{}", port);
    println!("[ui] Listening on http://{}", addr);
    let listener = TcpListener::bind(&addr).await?;
    axum::serve(listener, make_router()).await?;
    Ok(())
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn serve_index() -> impl IntoResponse {
    let html = include_str!("ui.html");
    (
        [(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"))],
        html,
    )
}

async fn get_status() -> impl IntoResponse {
    match build_status().await {
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
    State(lock): State<RollbackLock>,
    Path(service): Path<String>,
) -> impl IntoResponse {
    // 409 if already in flight
    {
        let mut set = lock.lock().unwrap();
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
        let mut set = lock.lock().unwrap();
        set.remove(&service);
    }

    match result {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── Status builder ────────────────────────────────────────────────────────────

async fn build_status() -> Result<Vec<ServiceStatus>> {
    let docker = Docker::connect_with_socket_defaults()?;
    let containers = docker.list_containers(Some(ListContainersOptions::<String> {
        all: false, // running only
        ..Default::default()
    })).await?;

    let out = containers.into_iter().map(|c| {
        let name = c.names
            .unwrap_or_default()
            .into_iter()
            .next()
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_string();
        ServiceStatus {
            name,
            image:  c.image.unwrap_or_default(),
            status: c.status.unwrap_or_default(),
            state:  c.state.unwrap_or_default(),
        }
    }).collect();

    Ok(out)
}

/// Extract short SHA from image tag `ghcr.io/CoderyOSS/codery:{service}-{sha}`.
fn sha_from_image_tag(service: &str, image: &str) -> Option<String> {
    let prefix = format!("{}:{}-", config::REGISTRY, service);
    image.strip_prefix(&prefix).map(|s| s.to_string())
}

// ── Rollback flow ─────────────────────────────────────────────────────────────

async fn run_rollback(service: &str) -> Result<()> {
    let def = ServiceDef::load(service)
        .map_err(|_| anyhow::anyhow!("unknown service: {}", service))?;

    let docker = Docker::connect_with_socket_defaults()?;

    let active_color = state::read_active(service)?;
    let prev_color = config::flip(&active_color);
    let prev_container = config::container_name(service, prev_color);
    let active_container = config::container_name(service, &active_color);

    // Verify the previous container exists and is stopped
    let info = docker
        .inspect_container(&prev_container, None)
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

    println!("[ui] Rolling back {} to {} (sha={})", service, prev_color, prev_sha);

    // Start the previous container
    use bollard::container::StartContainerOptions;
    docker
        .start_container(&prev_container, None::<StartContainerOptions<String>>)
        .await
        .map_err(|e| anyhow::anyhow!("failed to start {}: {}", prev_container, e))?;

    // Health check — on failure, stop the container we just started
    if let Err(e) = deploy::poll_health(&docker, &def, prev_color).await {
        let _ = deploy::stop_container(&docker, &prev_container).await;
        bail!("health check failed after restart: {}", e);
    }

    // Flip state
    state::write_active(service, prev_color)?;
    state::write_active_sha(service, &prev_sha)?;
    println!("[ui] State updated: {} is now {}", service, prev_color);

    // Reload Caddy
    caddy::apply_all()
        .map_err(|e| anyhow::anyhow!("caddy reload failed after state write: {}", e))?;

    // Stop the now-old active container (only reached if Caddy reload succeeded)
    deploy::stop_container(&docker, &active_container).await?;

    println!("[ui] Rollback complete: {} is now {}", service, prev_color);
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
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, make_router()).await.unwrap() });
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
