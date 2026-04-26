use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

use crate::config;

/// Read the active color for a service. Returns "blue" if state file missing or invalid.
pub fn read_active(service: &str) -> Result<String> {
    let path = state_path(service);
    if !path.exists() {
        return Ok("blue".to_string());
    }
    let color = fs::read_to_string(&path)
        .with_context(|| format!("failed to read state file {:?}", path))?
        .trim()
        .to_string();
    if color != "blue" && color != "green" {
        return Ok("blue".to_string());
    }
    Ok(color)
}

/// Write the active color for a service.
pub fn write_active(service: &str, color: &str) -> Result<()> {
    let path = state_path(service);
    fs::create_dir_all(path.parent().unwrap())
        .context("failed to create state directory")?;
    fs::write(&path, format!("{}\n", color))
        .with_context(|| format!("failed to write state file {:?}", path))?;
    Ok(())
}

/// Read the active SHA for a service. Returns None if not yet recorded.
pub fn read_active_sha(service: &str) -> Option<String> {
    let path = sha_path(service);
    fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Write the active SHA for a service (called alongside write_active after cutover).
pub fn write_active_sha(service: &str, sha: &str) -> Result<()> {
    let path = sha_path(service);
    fs::create_dir_all(path.parent().unwrap())
        .context("failed to create state directory")?;
    fs::write(&path, format!("{}\n", sha))
        .with_context(|| format!("failed to write SHA state file {:?}", path))?;
    Ok(())
}

fn state_path(service: &str) -> PathBuf {
    PathBuf::from(config::STATE_DIR).join(format!("{}.color", service))
}

fn sha_path(service: &str) -> PathBuf {
    PathBuf::from(config::STATE_DIR).join(format!("{}.sha", service))
}

/// Read the active color from a specific path (for testing).
#[cfg(test)]
fn read_from(path: &std::path::Path) -> Result<String> {
    if !path.exists() {
        return Ok("blue".to_string());
    }
    let color = std::fs::read_to_string(path)?.trim().to_string();
    if color != "blue" && color != "green" {
        return Ok("blue".to_string());
    }
    Ok(color)
}

/// Write the active color to a specific path (for testing).
#[cfg(test)]
fn write_to(path: &std::path::Path, color: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", color))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_paths_are_correct() {
        let sandbox_path = state_path("sandbox");
        assert!(sandbox_path.to_string_lossy().ends_with("sandbox.color"));

        let apps_path = state_path("apps");
        assert!(apps_path.to_string_lossy().ends_with("apps.color"));
    }

    #[test]
    fn missing_file_returns_blue() {
        let dir = std::env::temp_dir().join("orch-test-state-missing");
        let path = dir.join("nonexistent.color");
        assert_eq!(read_from(&path).unwrap(), "blue");
    }

    #[test]
    fn round_trip_blue_and_green() {
        let dir = std::env::temp_dir().join("orch-test-state-rt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sandbox.color");

        write_to(&path, "green").unwrap();
        assert_eq!(read_from(&path).unwrap(), "green");

        write_to(&path, "blue").unwrap();
        assert_eq!(read_from(&path).unwrap(), "blue");

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn corrupt_content_returns_blue() {
        let dir = std::env::temp_dir().join("orch-test-state-corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("corrupt.color");

        std::fs::write(&path, "not-a-color\n").unwrap();
        assert_eq!(read_from(&path).unwrap(), "blue");

        std::fs::remove_file(&path).unwrap();
    }
}
