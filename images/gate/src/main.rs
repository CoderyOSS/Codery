use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gate::apiproxy::ApiDeps;
use gate::approval::{ApprovalOutcome, StubBackend, APPROVAL_TIMEOUT_SECS};
use gate::audit::Audit;
use gate::config::Config;
use gate::custody::MachineTokenStore;
use gate::github::GitHubPlatform;
use gate::gitproxy::GitDeps;
use gate::onepage::OnePageDeps;
use gate::policy::{PolicyEngine, PolicyFile};
use gate::routes::{self, GateState};
use gate::state::State as DbState;
use gate::telegram::{run_poll_loop, TelegramClient, TelegramRouter, POLL_BACKOFF_BASE};
use std::sync::atomic::Ordering;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::from_env()?;

    let policy_src = std::fs::read_to_string(&cfg.policy_path)
        .map_err(|e| anyhow::anyhow!("policy file {}: {e}", cfg.policy_path))?;
    let policy_file = PolicyFile::parse(&policy_src)
        .map_err(|e| anyhow::anyhow!("policy file {}: {e}", cfg.policy_path))?;
    let policy = PolicyEngine::new(&policy_file);

    let state = Arc::new(Mutex::new(DbState::open(&cfg.state_path)?));
    let audit = Arc::new(Audit::open(&cfg.audit_path));

    // Machine-token custody (GATE-004): the PAT decrypts in memory, in the
    // request path only. A wrong GATE_TOKEN_ENC_KEY fails closed (no
    // credential → upstream calls fail) with a note on stderr; the token
    // itself is never logged.
    let custody_state = state.clone();
    let enc_key = cfg.token_enc_key.clone();
    let machine_token: Arc<dyn Fn() -> Option<String> + Send + Sync> = Arc::new(move || {
        let state = custody_state
            .lock()
            .expect("state lock must be poison-free");
        match MachineTokenStore::new(&state, &enc_key).load() {
            Ok(token) => token,
            Err(e) => {
                eprintln!("custody load failed — forwarding disabled: {e}");
                None
            }
        }
    });
    let platform = Arc::new(GitHubPlatform::new(machine_token.clone()));
    let gate_host = cfg.gate_host();
    let machine_login = cfg.machine_login.clone();

    // Test double until the Telegram control plane swaps in the real
    // approval backend; Denied keeps holds fail closed without a decider.
    let backend = Arc::new(StubBackend::new(
        state.clone(),
        audit.clone(),
        ApprovalOutcome::Denied,
    )?);

    let git = GitDeps::new(
        state.clone(),
        platform.clone(),
        policy.clone(),
        backend.clone(),
        cfg.machine_login.clone(),
        machine_token.clone(),
        Duration::from_secs(APPROVAL_TIMEOUT_SECS),
    )?;

    // The API proxy shares every dependency with the git proxy: one
    // platform (single place holding api_base + token custody), one
    // policy engine, one approval backend, one registry.
    let api = ApiDeps::new(
        state.clone(),
        platform.clone(),
        policy,
        backend,
        machine_token,
        Duration::from_secs(APPROVAL_TIMEOUT_SECS),
    )?;

    let mut gate_state = GateState::with_git(state, audit, git);
    gate_state.api = Some(Arc::new(api));
    gate_state.max_body_bytes = cfg.max_body_bytes;
    gate_state.onepage = Some(Arc::new(OnePageDeps {
        gate_host,
        machine_login,
        platform,
        enc_key: cfg.token_enc_key.clone(),
    }));

    // Telegram control plane (GATE-006): spawn the long-poll loop ONLY
    // when a bot token is configured — core git/API serving must work
    // without the bot. An empty allowlist keeps the loop running but drops
    // every update (fail closed); Tasks 2–5 register the real handlers on
    // the router, so until then validated updates simply find no handler.
    if let Some(token) = cfg.telegram_bot_token.clone() {
        let api: Arc<dyn gate::telegram::TelegramApi> = Arc::new(TelegramClient::new(&token));
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let router = TelegramRouter::new();
        tokio::spawn(run_poll_loop(
            api.clone(),
            cfg.telegram_allowlist.clone(),
            tx,
            shutdown_rx,
            POLL_BACKOFF_BASE,
        ));
        tokio::spawn(router.run(rx, api));
        gate_state.telegram_live.store(true, Ordering::Relaxed);
        eprintln!(
            "telegram control plane live ({} allowlisted approver(s))",
            cfg.telegram_allowlist.len()
        );
    }

    let app = routes::router(gate_state);

    let listener = tokio::net::TcpListener::bind(&cfg.listen_addr).await?;
    eprintln!(
        "gate v{} listening on {} (machine account: {})",
        env!("CARGO_PKG_VERSION"),
        cfg.listen_addr,
        cfg.machine_login
    );
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
