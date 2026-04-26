use anyhow::{bail, Context, Result};
use std::net::TcpStream;
use std::process::Command;
use std::time::Duration;

use crate::config;

/// Run all pre-flight checks. Returns Ok(()) only if all pass.
pub fn run() -> Result<()> {
    check_supervisord()?;
    check_tailscale()?;
    check_caddy()?;
    Ok(())
}

pub fn check_supervisord() -> Result<()> {
    let out = Command::new("supervisorctl")
        .args(["-c", config::SUPERVISORD_CONF, "status"])
        .output()
        .context("failed to run supervisorctl")?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.lines().any(|l| l.contains("RUNNING")) {
        println!("[preflight] supervisord: OK");
        Ok(())
    } else {
        bail!("[preflight] supervisord: no programs RUNNING\n{}", stdout);
    }
}

pub fn check_tailscale() -> Result<()> {
    let out = Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .context("failed to run tailscale")?;

    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("tailscale status output not JSON")?;

    let state = json["BackendState"].as_str().unwrap_or("unknown");

    if state == "Running" {
        println!("[preflight] tailscale: OK ({})", state);
        Ok(())
    } else {
        bail!("[preflight] tailscale: BackendState = {}", state);
    }
}

pub fn check_caddy() -> Result<()> {
    TcpStream::connect_timeout(
        &format!("127.0.0.1:{}", config::CADDY_ADMIN_PORT)
            .parse()
            .unwrap(),
        Duration::from_secs(3),
    )
    .context(format!(
        "[preflight] caddy admin API not reachable on port {}",
        config::CADDY_ADMIN_PORT
    ))?;

    println!("[preflight] caddy: OK (port {} reachable)", config::CADDY_ADMIN_PORT);
    Ok(())
}
