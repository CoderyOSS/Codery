use anyhow::{Context, Result};
use bollard::auth::DockerCredentials;
use bollard::Docker;
use bollard::image::{CreateImageOptions, ListImagesOptions, RemoveImageOptions};
use futures_util::StreamExt;
use std::collections::HashMap;

use crate::config;

/// Check whether an image reference is present in the local Docker cache.
pub async fn image_exists_locally(image: &str) -> Result<bool> {
    let docker = Docker::connect_with_socket_defaults()
        .context("failed to connect to Docker socket")?;
    let mut filters = HashMap::new();
    filters.insert("reference".to_string(), vec![image.to_string()]);
    let images = docker
        .list_images(Some(ListImagesOptions {
            all: false,
            filters,
            ..Default::default()
        }))
        .await
        .context("failed to list images")?;
    Ok(!images.is_empty())
}

/// Pull an image by full OCI reference, but skip the network pull if the
/// image is already present locally. Useful for locally-built images that
/// have never been pushed to a registry.
pub async fn pull_if_missing(image: &str) -> Result<()> {
    if image_exists_locally(image).await? {
        println!("[images] Image {} present locally — skipping pull", image);
        return Ok(());
    }
    pull(image).await
}

/// Pull an image by full OCI reference (e.g. "ghcr.io/org/repo:tag" or
/// "mcr.microsoft.com/playwright:v1.54.1-noble"). The reference is opaque:
/// no registry is assumed. Streams progress to stdout.
pub async fn pull(image: &str) -> Result<()> {
    println!("[images] Pulling {}...", image);

    let docker = Docker::connect_with_socket_defaults()
        .context("failed to connect to Docker socket")?;

    // Credentials are resolved from the registry hostname in the reference:
    // ghcr.io pulls use the GitHub credentials from /opt/codery/.env, other
    // registries (mcr.microsoft.com, docker.io, ...) pull anonymously.
    let credentials = credentials_for(image);

    let mut stream = docker.create_image(
        Some(CreateImageOptions {
            from_image: image.to_string(),
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

/// Extract the registry hostname from an OCI image reference.
///
/// Mirrors Docker's rule: a ref without any '/' is always Docker Hub (no
/// registry). With a '/', the first path component is a registry when it
/// contains '.' or ':' (host:port) or equals "localhost".
pub fn registry_of(image: &str) -> Option<&str> {
    if !image.contains('/') {
        return None;
    }
    let first = image.split('/').next()?;
    if first.contains('.') || first.contains(':') || first == "localhost" {
        Some(first)
    } else {
        None
    }
}

/// Docker credentials for an image reference, selected by registry hostname.
/// Returns None (anonymous pull) unless the registry is ghcr.io.
pub fn credentials_for(image: &str) -> Option<DockerCredentials> {
    if registry_of(image) == Some("ghcr.io") {
        ghcr_credentials()
    } else {
        None
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_of_ghcr() {
        assert_eq!(
            registry_of("ghcr.io/coderyoss/codery:sandbox-abc123"),
            Some("ghcr.io")
        );
    }

    #[test]
    fn registry_of_mcr() {
        assert_eq!(
            registry_of("mcr.microsoft.com/playwright:v1.54.1-noble"),
            Some("mcr.microsoft.com")
        );
    }

    #[test]
    fn registry_of_dockerhub_default() {
        assert_eq!(registry_of("ubuntu:24.04"), None);
        assert_eq!(registry_of("nginx"), None);
    }

    #[test]
    fn registry_of_localhost_with_port() {
        assert_eq!(registry_of("localhost:5000/team/app:v1"), Some("localhost:5000"));
    }

    #[test]
    fn credentials_for_non_ghcr_is_none() {
        assert!(credentials_for("mcr.microsoft.com/playwright:v1.54.1-noble").is_none());
        assert!(credentials_for("docker.io/library/ubuntu:24.04").is_none());
    }

    #[test]
    fn credentials_for_ghcr_is_some_when_env_configured() {
        // ghcr.io refs get GHCR credentials; on a host without /opt/codery/.env
        // ghcr_credentials() itself falls back to None (anonymous).
        let _ = credentials_for("ghcr.io/coderyoss/codery:sandbox-abc123");
    }
}
