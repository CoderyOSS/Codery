use anyhow::{Context, Result};
use bollard::auth::DockerCredentials;
use bollard::Docker;
use bollard::image::{CreateImageOptions, ListImagesOptions, RemoveImageOptions};
use futures_util::StreamExt;
use std::collections::HashMap;

use crate::config;

/// Pull an image by service name and git sha. Streams progress to stdout.
/// Reads GHCR credentials from /opt/codery/.env (GHCR_USERNAME + GHCR_TOKEN).
pub async fn pull(service: &str, sha: &str) -> Result<()> {
    let image = config::image_ref(service, sha);
    println!("[images] Pulling {}...", image);

    let docker = Docker::connect_with_socket_defaults()
        .context("failed to connect to Docker socket")?;

    // Build registry credentials from .env (GHCR_USERNAME + GHCR_TOKEN).
    // Without explicit credentials, the Docker daemon API call is anonymous and
    // cannot access private GHCR images.
    let credentials = ghcr_credentials();

    let mut stream = docker.create_image(
        Some(CreateImageOptions {
            from_image: image.clone(),
            ..Default::default()
        }),
        None,
        credentials,
    );

    while let Some(result) = stream.next().await {
        match result {
            Ok(info) => {
                if let (Some(status), Some(progress)) = (info.status, info.progress) {
                    print!("\r[images] {} {}", status, progress);
                }
            }
            Err(e) => anyhow::bail!("pull failed: {}", e),
        }
    }
    println!("\n[images] Pull complete: {}", image);
    Ok(())
}

/// Prune images for a service, keeping the two most recently created.
pub async fn prune(service: &str) -> Result<()> {
    let docker = Docker::connect_with_socket_defaults()
        .context("failed to connect to Docker socket")?;

    let mut filters = HashMap::new();
    filters.insert(
        "reference".to_string(),
        vec![format!("{}:{}-*", config::REGISTRY, service)],
    );

    let mut images = docker
        .list_images(Some(ListImagesOptions {
            all: false,
            filters,
            ..Default::default()
        }))
        .await
        .context("failed to list images")?;

    // Sort by Created descending (newest first)
    images.sort_by(|a, b| b.created.cmp(&a.created));

    // Keep the first 2, remove the rest
    let mut removed = 0;
    for image in images.into_iter().skip(2) {
        println!("[images] Removing old image: {}", image.id);
        if let Err(e) = docker
            .remove_image(
                &image.id,
                Some(RemoveImageOptions {
                    force: false,
                    noprune: false,
                }),
                None,
            )
            .await
        {
            println!("[images] Warning: failed to remove {}: {}", image.id, e);
        } else {
            removed += 1;
        }
    }

    println!("[images] Pruned {} old image(s) for {}", removed, service);
    Ok(())
}

/// A locally available image for a service.
#[derive(Debug, serde::Serialize)]
pub struct LocalImage {
    pub sha: String,
    pub tag: String,
    pub created: i64,
}

/// List images available locally for a service, newest first.
/// Extracts the git SHA from tags like `ghcr.io/coderyoss/codery:sandbox-abc123`.
pub async fn list_local(service: &str) -> anyhow::Result<Vec<LocalImage>> {
    let docker = Docker::connect_with_socket_defaults()
        .context("failed to connect to Docker socket")?;

    let mut filters = HashMap::new();
    filters.insert(
        "reference".to_string(),
        vec![format!("{}:{}-*", config::REGISTRY, service)],
    );

    let images = docker
        .list_images(Some(ListImagesOptions {
            all: false,
            filters,
            ..Default::default()
        }))
        .await
        .context("failed to list images")?;

    let prefix = format!("{}:{}-", config::REGISTRY, service);
    let mut result: Vec<LocalImage> = images
        .into_iter()
        .flat_map(|img| {
            img.repo_tags
                .iter()
                .filter_map(|tag| tag.strip_prefix(&prefix).map(|sha| (sha.to_string(), tag.clone())))
                .map(|(sha, tag)| LocalImage { sha, tag, created: img.created })
                .collect::<Vec<_>>()
        })
        .collect();

    result.sort_by(|a, b| b.created.cmp(&a.created));
    Ok(result)
}

/// Read GHCR credentials from /opt/codery/.env.
/// Returns None if credentials are not configured (anonymous pull).
fn ghcr_credentials() -> Option<DockerCredentials> {
    let content = std::fs::read_to_string(config::ENV_FILE).ok()?;
    let mut username = None;
    let mut password = None;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("GHCR_USERNAME=") {
            username = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("GHCR_TOKEN=") {
            password = Some(v.to_string());
        }
    }
    match (username, password) {
        (Some(u), Some(p)) => Some(DockerCredentials {
            username: Some(u),
            password: Some(p),
            serveraddress: Some(config::GHCR_HOST.to_string()),
            ..Default::default()
        }),
        _ => {
            println!("[images] Warning: GHCR_USERNAME/GHCR_TOKEN not in .env — pulling anonymously");
            None
        }
    }
}
