//! Build-only MCP exec bridge.
//!
//! Allows the MCP-served agent to invoke a strict allowlist of `codery-ci`
//! subcommands (build / validate / deploy-preview / cancel-preview) as
//! background jobs, with full stdout+stderr captured to a per-job log file.
//!
//! Safety model:
//! - Gated by a host-side toggle file (`codery-ci mcp-exec enable`).
//! - Allowlist excludes cutover / deploy / serve / daemon — those can touch
//!   the active sandbox session and stay human-only via the host shell.
//! - Jobs are spawned from the daemon process (root under supervisord).

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::config;

/// Subcommands the MCP exec tool is allowed to invoke.
/// Anything else returns an error. Cutover/deploy/serve/daemon are NEVER allowed.
const ALLOWLIST: &[&str] = &["build", "validate", "deploy-preview", "cancel-preview"];

/// Path to the codery-ci binary. Override-able for tests.
fn codery_ci_bin() -> String {
    std::env::var("CODERY_CI_BIN").unwrap_or_else(|_| "/opt/codery/codery-ci".to_string())
}

#[derive(Debug, Clone, Serialize, PartialEq, Copy)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Running,
    Done,
    Failed,
    Timeout,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecJob {
    pub id: String,
    pub args: Vec<String>,
    pub pid: Option<u32>,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    pub status: JobStatus,
    pub exit_code: Option<i32>,
    pub log_path: PathBuf,
}

pub type JobRegistry = Arc<Mutex<HashMap<String, ExecJob>>>;

/// Process-wide job registry. Lives as long as the daemon.
fn registry() -> &'static JobRegistry {
    static REGISTRY: OnceLock<JobRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
}

/// Is the MCP exec toggle enabled? Toggle = file presence at config::MCP_EXEC_TOGGLE.
pub fn toggle_enabled() -> bool {
    toggle_enabled_at(Path::new(config::MCP_EXEC_TOGGLE))
}

fn toggle_enabled_at(path: &Path) -> bool {
    path.exists()
}

/// Enable/disable the toggle (called from CLI subcommands).
pub fn set_toggle(enabled: bool) -> std::io::Result<()> {
    set_toggle_at(Path::new(config::MCP_EXEC_TOGGLE), enabled)
}

fn set_toggle_at(path: &Path, enabled: bool) -> std::io::Result<()> {
    if enabled {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, "1\n")?;
    } else if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Is the given subcommand allowed via MCP?
pub fn is_allowed(subcmd: &str) -> bool {
    ALLOWLIST.contains(&subcmd)
}

pub fn allowlist() -> &'static [&'static str] {
    ALLOWLIST
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn gen_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Lower 48 bits as hex — enough uniqueness within a daemon lifetime.
    format!("{:0>12x}", nanos & 0xFFFF_FFFF_FFFF)
}

#[derive(Debug, Serialize)]
pub struct SpawnResponse {
    pub job_id: String,
    pub log_path: String,
    pub pid: Option<u32>,
    pub started_at: u64,
    pub allowed_subcommands: &'static [&'static str],
}

/// Errors prevent the job from starting; Ok means the job is registered and running.
pub async fn spawn(args: Vec<String>, timeout_secs: u64) -> Result<SpawnResponse, String> {
    spawn_with(
        args,
        timeout_secs,
        Path::new(config::MCP_EXEC_TOGGLE),
        PathBuf::from(config::MCP_EXEC_LOG_DIR),
        &codery_ci_bin(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn spawn_with(
    args: Vec<String>,
    timeout_secs: u64,
    toggle_path: &Path,
    log_dir: PathBuf,
    bin: &str,
) -> Result<SpawnResponse, String> {
    if !toggle_enabled_at(toggle_path) {
        return Err(format!(
            "MCP exec disabled. Run on host: codery-ci mcp-exec enable"
        ));
    }
    let subcmd = args
        .first()
        .ok_or_else(|| "no subcommand provided".to_string())?;
    if !is_allowed(subcmd) {
        return Err(format!(
            "subcommand '{}' not in allowlist (allowed: {:?}). \
             Cutover and deploy are never exposed via MCP — run on host shell.",
            subcmd, ALLOWLIST
        ));
    }

    std::fs::create_dir_all(&log_dir).map_err(|e| format!("failed to create log dir: {}", e))?;

    let id = gen_id();
    let started_at = now_secs();
    let log_path = log_dir.join(format!("exec-{}-{}-{}.log", started_at, subcmd, id));

    let log_file = std::fs::File::create(&log_path)
        .map_err(|e| format!("failed to create log file {:?}: {}", log_path, e))?;
    let log_file_stderr = log_file
        .try_clone()
        .map_err(|e| format!("failed to clone log file handle: {}", e))?;

    let mut cmd = Command::new(bin);
    cmd.args(&args)
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_file_stderr));
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn '{}': {}", bin, e))?;

    let pid = child.id();

    let job = ExecJob {
        id: id.clone(),
        args,
        pid,
        started_at,
        finished_at: None,
        status: JobStatus::Running,
        exit_code: None,
        log_path: log_path.clone(),
    };
    registry().lock().await.insert(id.clone(), job);

    // Background task: wait for exit (or timeout), update registry, append marker to log.
    let id_for_task = id.clone();
    let log_path_for_task = log_path.clone();
    tokio::spawn(async move {
        let wait_result = if timeout_secs == 0 {
            child.wait().await
        } else {
            match timeout(Duration::from_secs(timeout_secs), child.wait()).await {
                Ok(r) => r,
                Err(_) => {
                    let _ = child.start_kill();
                    {
                        let mut jobs = registry().lock().await;
                        if let Some(job) = jobs.get_mut(&id_for_task) {
                            job.status = JobStatus::Timeout;
                            job.finished_at = Some(now_secs());
                        }
                    }
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .append(true)
                        .open(&log_path_for_task)
                    {
                        let _ = writeln!(f, "\n[mcp-exec] TIMEOUT after {}s", timeout_secs);
                    }
                    return;
                }
            }
        };

        let (new_status, new_exit_code) = match wait_result {
            Ok(status) => {
                let code = status.code();
                (
                    if status.success() {
                        JobStatus::Done
                    } else {
                        JobStatus::Failed
                    },
                    code,
                )
            }
            Err(e) => {
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&log_path_for_task)
                {
                    let _ = writeln!(f, "\n[mcp-exec] process wait error: {}", e);
                }
                (JobStatus::Failed, None)
            }
        };

        let mut jobs = registry().lock().await;
        if let Some(job) = jobs.get_mut(&id_for_task) {
            job.finished_at = Some(now_secs());
            job.status = new_status;
            job.exit_code = new_exit_code;
        }
    });

    Ok(SpawnResponse {
        job_id: id,
        log_path: log_path.to_string_lossy().to_string(),
        pid,
        started_at,
        allowed_subcommands: ALLOWLIST,
    })
}

#[derive(Debug, Serialize)]
pub struct JobStatusResponse {
    pub job_id: String,
    pub status: JobStatus,
    pub exit_code: Option<i32>,
    pub elapsed_secs: u64,
    pub log_path: String,
    pub tail: String,
}

pub async fn status(job_id: &str, tail_bytes: usize) -> Result<JobStatusResponse, String> {
    let snapshot = {
        let jobs = registry().lock().await;
        jobs.get(job_id)
            .cloned()
            .ok_or_else(|| format!("unknown job_id: {}", job_id))?
    };

    let elapsed = now_secs().saturating_sub(snapshot.started_at);
    let tail = read_tail(&snapshot.log_path, tail_bytes);

    Ok(JobStatusResponse {
        job_id: job_id.to_string(),
        status: snapshot.status,
        exit_code: snapshot.exit_code,
        elapsed_secs: elapsed,
        log_path: snapshot.log_path.to_string_lossy().to_string(),
        tail,
    })
}

/// Read last N bytes of a file as UTF-8 (lossy). Empty string if unreadable.
fn read_tail(path: &Path, max_bytes: usize) -> String {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let size = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
    if size > max_bytes {
        if file
            .seek(SeekFrom::End(-(max_bytes as i64)))
            .is_err()
        {
            return String::new();
        }
    }
    let mut buf = Vec::with_capacity(max_bytes.min(size));
    let _ = file.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_includes_build_only_subset() {
        assert!(is_allowed("build"));
        assert!(is_allowed("validate"));
        assert!(is_allowed("deploy-preview"));
        assert!(is_allowed("cancel-preview"));
    }

    #[test]
    fn allowlist_excludes_destructive_commands() {
        assert!(!is_allowed("cutover"));
        assert!(!is_allowed("deploy"));
        assert!(!is_allowed("serve"));
        assert!(!is_allowed("daemon"));
        assert!(!is_allowed(""));
        assert!(!is_allowed("rm"));
        assert!(!is_allowed("shell"));
    }

    #[test]
    fn toggle_constants_point_at_expected_paths() {
        assert!(config::MCP_EXEC_TOGGLE.ends_with("mcp-exec.enabled"));
        assert!(config::MCP_EXEC_LOG_DIR.starts_with("/var/log/"));
    }

    #[test]
    fn toggle_set_and_check_round_trip_in_tempdir() {
        let dir = std::env::temp_dir().join("mcp-exec-test-toggle");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("enabled");

        assert!(!toggle_enabled_at(&path));
        set_toggle_at(&path, true).unwrap();
        assert!(toggle_enabled_at(&path));
        set_toggle_at(&path, false).unwrap();
        assert!(!toggle_enabled_at(&path));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_toggle_enable_creates_parent_dir() {
        let dir = std::env::temp_dir().join("mcp-exec-test-nested/inner");
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("mcp-exec-test-nested"));
        let path = dir.join("enabled");

        set_toggle_at(&path, true).unwrap();
        assert!(path.exists());

        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("mcp-exec-test-nested"));
    }

    #[test]
    fn set_toggle_disable_when_already_off_is_noop() {
        let dir = std::env::temp_dir().join("mcp-exec-test-noop");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("enabled");
        assert!(set_toggle_at(&path, false).is_ok());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_tail_handles_missing_file() {
        let s = read_tail(Path::new("/nonexistent/should/not/exist"), 1024);
        assert_eq!(s, "");
    }

    #[test]
    fn read_tail_returns_full_content_when_smaller_than_max() {
        let dir = std::env::temp_dir().join("mcp-exec-test-tail-small");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("small.log");
        std::fs::write(&path, "hello world\n").unwrap();
        let s = read_tail(&path, 1024);
        assert_eq!(s, "hello world\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_tail_returns_only_last_n_bytes_when_larger() {
        let dir = std::env::temp_dir().join("mcp-exec-test-tail-large");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("large.log");
        let content = "0123456789".repeat(100); // 1000 bytes
        std::fs::write(&path, &content).unwrap();
        let s = read_tail(&path, 50);
        assert_eq!(s.len(), 50);
        assert!(s.ends_with("0123456789"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn spawn_refuses_when_toggle_off() {
        let dir = std::env::temp_dir().join("mcp-exec-test-off");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let toggle = dir.join("enabled");
        let log_dir = dir.join("logs");

        let result = spawn_with(
            vec!["build".to_string(), "sandbox".to_string(), "t".to_string()],
            30,
            &toggle,
            log_dir,
            "/bin/true",
        )
        .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("disabled"), "err = {}", err);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn spawn_refuses_disallowed_subcommand_even_when_toggle_on() {
        let dir = std::env::temp_dir().join("mcp-exec-test-deny");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let toggle = dir.join("enabled");
        std::fs::write(&toggle, "1\n").unwrap();
        let log_dir = dir.join("logs");

        let result = spawn_with(
            vec!["cutover".to_string(), "sandbox".to_string()],
            30,
            &toggle,
            log_dir,
            "/bin/true",
        )
        .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("not in allowlist"), "err = {}", err);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn spawn_runs_allowed_subcommand_and_completes() {
        let dir = std::env::temp_dir().join("mcp-exec-test-run");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let toggle = dir.join("enabled");
        std::fs::write(&toggle, "1\n").unwrap();
        let log_dir = dir.join("logs");

        // /bin/true simulates codery-ci succeeding.
        let resp = spawn_with(
            vec!["build".to_string(), "sandbox".to_string(), "t".to_string()],
            10,
            &toggle,
            log_dir,
            "/bin/true",
        )
        .await
        .expect("spawn should succeed");
        assert_eq!(resp.allowed_subcommands, ALLOWLIST);

        // Wait for the background task to mark the job done.
        for _ in 0..20 {
            if let Ok(s) = status(&resp.job_id, 4096).await {
                if s.status != JobStatus::Running {
                    assert_eq!(s.status, JobStatus::Done, "expected Done, got {:?}", s);
                    assert_eq!(s.exit_code, Some(0));
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let final_status = status(&resp.job_id, 4096).await.expect("status should resolve");
        assert_eq!(final_status.status, JobStatus::Done);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn status_returns_error_for_unknown_job() {
        let result = status("does-not-exist", 1024).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("unknown job_id"));
    }
}
