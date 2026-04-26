use anyhow::Result;
use crate::{config, mcp, tcp_proxy, ui};

/// Run MCP, UI, and TCP-proxy servers concurrently in a single process.
/// Exits if any server errors or if SIGTERM/SIGINT is received.
pub async fn serve() -> Result<()> {
    crate::open_port_for_docker_bridges(config::MCP_PORT);

    println!(
        "[daemon] Starting codery-ci daemon: MCP=:{} UI=:{} TCP-proxy",
        config::MCP_PORT,
        config::UI_PORT
    );

    tokio::select! {
        r = mcp::serve(config::MCP_PORT)  => r?,
        r = ui::serve(config::UI_PORT)    => r?,
        r = tcp_proxy::serve()            => r?,
        _ = shutdown_signal()             => {
            println!("[daemon] Received shutdown signal — stopping");
        }
    }

    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .unwrap_or(())
    };

    #[cfg(unix)]
    let sigterm = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let sigterm = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c  => {}
        _ = sigterm => {}
    }
}
