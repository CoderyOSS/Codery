use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{Pid, User};
use serde::Deserialize;
use signal_hook::consts::signal::SIGTERM;

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case")]
enum RestartPolicy {
    Always,
    Never,
    OnFailure,
}

impl Default for RestartPolicy {
    fn default() -> Self { RestartPolicy::Always }
}

#[derive(Deserialize, Clone, Debug)]
struct ServiceConfig {
    name: String,
    command: Vec<String>,
    user: Option<String>,
    directory: Option<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    restart: RestartPolicy,
}

#[derive(Deserialize, Debug)]
struct Config {
    services: Vec<ServiceConfig>,
}

struct RunningService {
    config: ServiceConfig,
    pid: u32,
}

fn ts() -> String {
    let s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{:02}:{:02}:{:02}", (s / 3600) % 24, (s % 3600) / 60, s % 60)
}

fn spawn_service(cfg: &ServiceConfig) -> std::io::Result<Child> {
    let mut cmd = Command::new(&cfg.command[0]);
    cmd.args(&cfg.command[1..]);
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());

    if let Some(dir) = &cfg.directory {
        cmd.current_dir(dir);
    }

    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }

    if let Some(username) = &cfg.user {
        let user = User::from_name(username)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("getpwnam failed: {}", e)))?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, format!("user '{}' not found", username)))?;
        let gid = user.gid;
        let uid = user.uid;
        unsafe {
            cmd.pre_exec(move || {
                nix::unistd::setgroups(&[gid]).map_err(std::io::Error::from)?;
                nix::unistd::setgid(gid).map_err(std::io::Error::from)?;
                nix::unistd::setuid(uid).map_err(std::io::Error::from)?;
                Ok(())
            });
        }
    }

    let child = cmd.spawn()?;
    println!("[launchy {}] started '{}' pid={}", ts(), cfg.name, child.id());
    Ok(child)
}

fn reap_zombies() -> Vec<(u32, i32)> {
    let mut exited = Vec::new();
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, code)) => {
                let pid = pid.as_raw() as u32;
                println!("[launchy {}] pid={} exited code={}", ts(), pid, code);
                exited.push((pid, code));
            }
            Ok(WaitStatus::Signaled(pid, sig, _)) => {
                let pid = pid.as_raw() as u32;
                println!("[launchy {}] pid={} killed by {:?}", ts(), pid, sig);
                exited.push((pid, 1));
            }
            Ok(WaitStatus::StillAlive) => break,
            Err(nix::errno::Errno::ECHILD) => break,
            Err(_) => break,
            _ => {}
        }
    }
    exited
}

fn shutdown_all(running: &mut HashMap<u32, RunningService>) {
    println!("[launchy {}] SIGTERM received — stopping all services", ts());

    for svc in running.values() {
        println!("[launchy {}] sending SIGTERM to '{}' pid={}", ts(), svc.config.name, svc.pid);
        let _ = nix::sys::signal::kill(
            Pid::from_raw(svc.pid as i32),
            nix::sys::signal::Signal::SIGTERM,
        );
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    while !running.is_empty() && Instant::now() < deadline {
        let exited = reap_zombies();
        for (pid, _) in exited {
            running.remove(&pid);
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    for svc in running.values() {
        println!("[launchy {}] sending SIGKILL to '{}' pid={}", ts(), svc.config.name, svc.pid);
        let _ = nix::sys::signal::kill(
            Pid::from_raw(svc.pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }

    std::thread::sleep(Duration::from_millis(200));
    reap_zombies();
}

fn main() {
    let config_path = std::env::args().nth(1)
        .unwrap_or_else(|| "/etc/launchy.json".to_string());

    let raw = std::fs::read_to_string(&config_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {}", config_path, e));
    let config: Config = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("invalid {}: {}", config_path, e));

    println!("[launchy {}] starting with {} service(s)", ts(), config.services.len());

    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGTERM, Arc::clone(&shutdown))
        .expect("failed to register SIGTERM handler");

    let mut running: HashMap<u32, RunningService> = HashMap::new();
    let configs: Vec<ServiceConfig> = config.services.clone();

    for cfg in &configs {
        match spawn_service(cfg) {
            Ok(child) => { running.insert(child.id(), RunningService { config: cfg.clone(), pid: child.id() }); }
            Err(e) => eprintln!("[launchy {}] failed to start '{}': {}", ts(), cfg.name, e),
        }
    }

    loop {
        if shutdown.load(Ordering::Acquire) {
            shutdown_all(&mut running);
            break;
        }

        let exited = reap_zombies();
        for (pid, code) in exited {
            if let Some(svc) = running.remove(&pid) {
                let should_restart = match svc.config.restart {
                    RestartPolicy::Always => true,
                    RestartPolicy::Never => false,
                    RestartPolicy::OnFailure => code != 0,
                };
                if should_restart && !shutdown.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_secs(1));
                    match spawn_service(&svc.config) {
                        Ok(child) => { running.insert(child.id(), RunningService { config: svc.config, pid: child.id() }); }
                        Err(e) => eprintln!("[launchy {}] restart failed for '{}': {}", ts(), svc.config.name, e),
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_millis(100));
    }

    println!("[launchy {}] exiting", ts());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_parse_minimal() {
        let json = r#"{
            "services": [
                {"name": "test", "command": ["/bin/true"]}
            ]
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.services.len(), 1);
        assert_eq!(cfg.services[0].name, "test");
        assert!(matches!(cfg.services[0].restart, RestartPolicy::Always));
    }

    #[test]
    fn test_config_parse_full() {
        let json = r#"{
            "services": [
                {
                    "name": "web",
                    "command": ["/usr/bin/env", "bash", "-c", "echo hi"],
                    "user": "root",
                    "directory": "/tmp",
                    "env": {"FOO": "bar"},
                    "restart": "never"
                }
            ]
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        let svc = &cfg.services[0];
        assert_eq!(svc.command, vec!["/usr/bin/env", "bash", "-c", "echo hi"]);
        assert_eq!(svc.env.get("FOO").map(|s| s.as_str()), Some("bar"));
        assert!(matches!(svc.restart, RestartPolicy::Never));
    }

    #[test]
    fn test_config_parse_on_failure() {
        let json = r#"{"services": [{"name": "x", "command": ["/bin/true"], "restart": "on_failure"}]}"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert!(matches!(cfg.services[0].restart, RestartPolicy::OnFailure));
    }

    #[test]
    fn test_spawn_and_reap() {
        let cfg = ServiceConfig {
            name: "sleep".into(),
            command: vec!["sleep".into(), "0.1".into()],
            user: None,
            directory: None,
            env: HashMap::new(),
            restart: RestartPolicy::Never,
        };

        let child = spawn_service(&cfg).expect("spawn failed");
        let pid = child.id();
        std::mem::forget(child);

        std::thread::sleep(Duration::from_millis(200));

        let exited = reap_zombies();
        assert!(!exited.is_empty(), "expected pid {} to be reaped", pid);
        assert!(exited.iter().any(|(p, _)| *p == pid));
    }
}
