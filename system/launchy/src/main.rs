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

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
enum RestartPolicy {
    Always,
    Never,
    OnFailure,
}

impl Default for RestartPolicy {
    fn default() -> Self { RestartPolicy::Always }
}

fn default_priority() -> u32 { 100 }

#[derive(Deserialize, Clone, Debug, PartialEq)]
struct ServiceConfig {
    name: String,
    command: Vec<String>,
    user: Option<String>,
    directory: Option<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    restart: RestartPolicy,
    #[serde(default = "default_priority")]
    priority: u32,
}

#[derive(Deserialize, Debug)]
struct Config {
    #[serde(default)]
    include_dirs: Vec<String>,
    status_file: Option<String>,
    services: Vec<ServiceConfig>,
}

#[derive(Deserialize, Debug)]
struct DevContainerFile {
    customizations: DevContainerCustomizations,
}

#[derive(Deserialize, Debug)]
struct DevContainerCustomizations {
    codery: DevContainerCodery,
}

#[derive(Deserialize, Debug)]
struct DevContainerCodery {
    sandbox: Config,
}

fn parse_full_config(raw: &str) -> Result<Config, serde_json::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ConfigFile {
        DevContainer(DevContainerFile),
        Direct(Config),
    }
    match serde_json::from_str::<ConfigFile>(raw)? {
        ConfigFile::DevContainer(dc) => Ok(dc.customizations.codery.sandbox),
        ConfigFile::Direct(c) => Ok(c),
    }
}

fn parse_config(raw: &str) -> Result<Config, serde_json::Error> {
    parse_full_config(raw)
}

fn load_include_dirs(dirs: &[String]) -> Vec<ServiceConfig> {
    let mut services = Vec::new();
    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[launchy {}] failed to read {}: {}", ts(), path.display(), e);
                    continue;
                }
            };
            match serde_json::from_str::<ServiceConfig>(&content) {
                Ok(svc) => services.push(svc),
                Err(e) => {
                    eprintln!("[launchy {}] failed to parse {}: {}", ts(), path.display(), e);
                    continue;
                }
            }
        }
    }
    services.sort_by_key(|s| s.priority);
    services
}

struct RunningService {
    config: ServiceConfig,
    pid: u32,
    restart_count: u32,
    started_at: Instant,
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

fn write_status_file(config: &Config, running: &HashMap<u32, RunningService>) {
    let path = match &config.status_file {
        Some(p) => p,
        None => return,
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let services: Vec<_> = running.values().map(|svc| {
        serde_json::json!({
            "name": svc.config.name,
            "pid": svc.pid,
            "status": "running",
            "uptime_secs": svc.started_at.elapsed().as_secs(),
            "restart_count": svc.restart_count,
        })
    }).collect();
    let json = serde_json::json!({
        "timestamp": timestamp,
        "services": services,
    });
    if let Err(e) = std::fs::write(path, serde_json::to_string_pretty(&json).unwrap()) {
        eprintln!("[launchy {}] failed to write status file {}: {}", ts(), path, e);
    }
}

fn build_desired_services(config: &Config) -> Vec<ServiceConfig> {
    let mut desired = config.services.clone();
    desired.extend(load_include_dirs(&config.include_dirs));
    desired.sort_by_key(|s| s.priority);
    desired.dedup_by(|a, b| a.name == b.name);
    desired.sort_by_key(|s| s.priority);
    desired
}

fn reload_services(config: &Config, running: &mut HashMap<u32, RunningService>) {
    let desired = build_desired_services(config);
    let desired_names: Vec<String> = desired.iter().map(|s| s.name.clone()).collect();

    let to_stop: Vec<u32> = running.iter()
        .filter(|(_, svc)| !desired_names.contains(&svc.config.name))
        .map(|(pid, _)| *pid)
        .collect();
    for pid in to_stop {
        if let Some(svc) = running.remove(&pid) {
            let _ = nix::sys::signal::kill(
                Pid::from_raw(svc.pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
            println!("[launchy {}] stopped '{}' (removed)", ts(), svc.config.name);
        }
    }

    for cfg in &desired {
        let existing = running.values().find(|svc| svc.config.name == cfg.name);
        match existing {
            None => {
                match spawn_service(cfg) {
                    Ok(child) => {
                        running.insert(child.id(), RunningService {
                            config: cfg.clone(),
                            pid: child.id(),
                            restart_count: 0,
                            started_at: Instant::now(),
                        });
                    }
                    Err(e) => eprintln!("[launchy {}] failed to start '{}': {}", ts(), cfg.name, e),
                }
            }
            Some(svc) => {
                if svc.config.command != cfg.command
                    || svc.config.directory != cfg.directory
                    || svc.config.env != cfg.env
                {
                    let old_pid = svc.pid;
                    let count = svc.restart_count;
                    let _ = nix::sys::signal::kill(
                        Pid::from_raw(old_pid as i32),
                        nix::sys::signal::Signal::SIGTERM,
                    );
                    running.remove(&old_pid);
                    match spawn_service(cfg) {
                        Ok(child) => {
                            running.insert(child.id(), RunningService {
                                config: cfg.clone(),
                                pid: child.id(),
                                restart_count: count + 1,
                                started_at: Instant::now(),
                            });
                        }
                        Err(e) => eprintln!("[launchy {}] restart failed for '{}': {}", ts(), cfg.name, e),
                    }
                    println!("[launchy {}] restarted '{}' (config changed)", ts(), cfg.name);
                }
            }
        }
    }

    write_status_file(config, running);
}

fn main() {
    let config_path = std::env::args().nth(1)
        .unwrap_or_else(|| "/etc/launchy.json".to_string());

    let raw = std::fs::read_to_string(&config_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {}", config_path, e));
    let config = parse_config(&raw)
        .unwrap_or_else(|e| panic!("invalid {}: {}", config_path, e));

    let mut all_services = config.services.clone();
    all_services.extend(load_include_dirs(&config.include_dirs));
    all_services.sort_by_key(|s| s.priority);
    all_services.dedup_by(|a, b| a.name == b.name);
    all_services.sort_by_key(|s| s.priority);

    println!("[launchy {}] starting with {} service(s)", ts(), all_services.len());

    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGTERM, Arc::clone(&shutdown))
        .expect("failed to register SIGTERM handler");

    let reload = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::signal::SIGHUP, Arc::clone(&reload))
        .expect("failed to register SIGHUP handler");

    let mut running: HashMap<u32, RunningService> = HashMap::new();

    for cfg in &all_services {
        match spawn_service(cfg) {
            Ok(child) => { running.insert(child.id(), RunningService { config: cfg.clone(), pid: child.id(), restart_count: 0, started_at: Instant::now() }); }
            Err(e) => eprintln!("[launchy {}] failed to start '{}': {}", ts(), cfg.name, e),
        }
    }

    write_status_file(&config, &running);

    loop {
        if shutdown.load(Ordering::Acquire) {
            shutdown_all(&mut running);
            break;
        }

        if reload.swap(false, Ordering::Acquire) {
            reload_services(&config, &mut running);
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
                        Ok(child) => { running.insert(child.id(), RunningService { config: svc.config, pid: child.id(), restart_count: svc.restart_count + 1, started_at: Instant::now() }); }
                        Err(e) => eprintln!("[launchy {}] restart failed for '{}': {}", ts(), svc.config.name, e),
                    }
                    write_status_file(&config, &running);
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
    fn parse_devcontainer_json() {
        let json = r#"{
            "name": "Codery",
            "customizations": {
                "codery": {
                    "sandbox": {
                        "services": [
                            {
                                "name": "opencode",
                                "command": ["opencode", "serve"],
                                "user": "gem",
                                "directory": "/home/gem/projects",
                                "env": {},
                                "restart": "always"
                            }
                        ]
                    },
                    "apps": []
                }
            }
        }"#;
        let config = parse_config(json).expect("should parse devcontainer.json");
        assert_eq!(config.services.len(), 1);
        assert_eq!(config.services[0].name, "opencode");
    }

    #[test]
    fn parse_flat_launchy_json() {
        let json = r#"{"services": [{"name": "svc", "command": ["sleep", "1"]}]}"#;
        let config = parse_config(json).expect("should parse flat launchy.json");
        assert_eq!(config.services.len(), 1);
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
            priority: 100,
        };

        let child = spawn_service(&cfg).expect("spawn failed");
        let pid = child.id();
        std::mem::forget(child);

        std::thread::sleep(Duration::from_millis(200));

        let exited = reap_zombies();
        assert!(!exited.is_empty(), "expected pid {} to be reaped", pid);
        assert!(exited.iter().any(|(p, _)| *p == pid));
    }

    #[test]
    fn test_config_with_include_dirs() {
        let json = r#"{"include_dirs": ["/a", "/b"], "status_file": "/run/status.json", "services": []}"#;
        let cfg = parse_full_config(json).expect("should parse");
        assert_eq!(cfg.include_dirs.len(), 2);
        assert_eq!(cfg.status_file, Some("/run/status.json".to_string()));
    }

    #[test]
    fn test_service_with_priority() {
        let json = r#"{"services": [{"name": "a", "command": ["x"], "priority": 10}, {"name": "b", "command": ["y"], "priority": 100}]}"#;
        let cfg = parse_full_config(json).expect("should parse");
        assert_eq!(cfg.services[0].priority, 10);
        assert_eq!(cfg.services[1].priority, 100);
    }

    #[test]
    fn test_config_defaults() {
        let json = r#"{"services": [{"name": "x", "command": ["/bin/true"]}]}"#;
        let cfg = parse_full_config(json).expect("should parse");
        assert!(cfg.include_dirs.is_empty());
        assert!(cfg.status_file.is_none());
        assert_eq!(cfg.services[0].priority, 100);
    }

    #[test]
    fn test_load_include_dirs() {
        let dir = std::env::temp_dir().join("launchy_test_include");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), r#"{"name": "a", "command": ["x"], "priority": 10}"#).unwrap();
        std::fs::write(dir.join("b.json"), r#"{"name": "b", "command": ["y"], "priority": 30}"#).unwrap();
        let services = load_include_dirs(&[dir.to_string_lossy().to_string()]);
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].name, "a");
        assert_eq!(services[1].name, "b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_include_dirs_ignores_non_json() {
        let dir = std::env::temp_dir().join("launchy_test_ignore");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("readme.txt"), "not json").unwrap();
        std::fs::write(dir.join("app.json"), r#"{"name": "app", "command": ["true"]}"#).unwrap();
        let services = load_include_dirs(&[dir.to_string_lossy().to_string()]);
        assert_eq!(services.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
