// Crate scaffold: the policy engine and config surface are wired into the
// HTTP server in later tasks (GATE-005/GATE-007); unused until then.
#![allow(dead_code)]

mod audit;
mod config;
mod github;
mod pktline;
mod policy;
mod state;
mod types;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = config::Config::from_env()?;
    eprintln!(
        "gate v{} starting, listen addr {}",
        env!("CARGO_PKG_VERSION"),
        cfg.listen_addr
    );
    Ok(())
}
