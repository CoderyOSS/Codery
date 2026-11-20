use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gate::apiproxy::ApiDeps;
use gate::approval::{ApprovalBackend, ApprovalOutcome, StubBackend, APPROVAL_TIMEOUT_SECS};
use gate::audit::Audit;
use gate::config::Config;
use gate::custody::MachineTokenStore;
use gate::github::GitHubPlatform;
use gate::gitproxy::GitDeps;
use gate::onepage::OnePageDeps;
use gate::policy::{PolicyEngine, PolicyFile};
use gate::registry::{run_registry_sweep, RegistryDeps};
use gate::routes::{self, GateState};
use gate::state::State as DbState;
use gate::tasks::{run_daily_loop, startup_check, DAILY_INTERVAL, LifecycleDeps};
use gate::control::approvals::{register_approvals, ControlApprovalBackend};
use gate::control::commands::{register_commands, CommandDeps};
use gate::control::elevation::{
    nonce_lookup as elevation_nonce_lookup, register_elevation, ElevationDeps, ElevationHandler,
};
use gate::control::grant::{
    register_revoke, revoke_nonce_lookup, GrantOfferDeps, RealGrantFlow, RealReductionFlow,
    ReductionDeps,
};
use gate::control::lifecycle::{register_lifecycle, nonce_lookup as rotate_nonce_lookup};
use gate::control::setup::{
    nonce_lookup as setup_nonce_lookup, register_setup, resume as setup_resume, SetupDeps,
};
use gate::control::{run_poll_loop, ControlRouter, POLL_BACKOFF_BASE};

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

    // Control-channel selection (GATE-013): ntfy when configured, else
    // none — the StubBackend keeps dangerous holds denied. The triple
    // `(transport, chat, allowlist)` preserves every downstream seam's
    // call shape. The broker token IS the operator identity, so the
    // allowlist is exactly the transport-assigned id `[1]`.
    let channel: Option<(
        Arc<dyn gate::control::ControlApi>,
        Option<i64>,
        Vec<i64>,
        &'static str,
    )> = {
        // ntfy transport wires in here (config + transport tasks, GATE-013)
        None
    };
    let chat_id = channel.as_ref().and_then(|(_, chat, _, _)| *chat);

    // Approval backend selection (GATE-005/006/012): with either channel
    // configured, dangerous holds land on the operator's chat/room; without
    // one the StubBackend keeps every hold fail closed — the auto decision
    // is Denied, so dangerous operations are rejected, never silently
    // forwarded.
    let approval_backend: Option<Arc<ControlApprovalBackend>> = match &channel {
        Some((api, chat, _, _)) => Some(Arc::new(ControlApprovalBackend::new(
            state.clone(),
            audit.clone(),
            api.clone(),
            *chat,
        )?)),
        None => None,
    };
    let backend: Arc<dyn ApprovalBackend> = match &approval_backend {
        Some(b) => b.clone(),
        None => Arc::new(StubBackend::new(
            state.clone(),
            audit.clone(),
            ApprovalOutcome::Denied,
        )?),
    };
    // Router-side handle: `backend` below moves into the proxy deps.
    let control_backend = approval_backend;

    // Where lifecycle notices go: the bot AND a chat must both exist. With
    // either missing, lifecycle checks still run and flags still flip —
    // only the notices are skipped.
    let control_notify: Option<(Arc<dyn gate::control::ControlApi>, i64)> =
        match (&channel, chat_id) {
            (Some((api, ..)), Some(chat)) => Some((api.clone(), chat)),
            _ => None,
        };

    let mut git = GitDeps::new(
        state.clone(),
        platform.clone(),
        policy.clone(),
        backend.clone(),
        cfg.machine_login.clone(),
        machine_token.clone(),
        Duration::from_secs(APPROVAL_TIMEOUT_SECS),
    )?;
    // Personal-repo ceiling channel (GATE-010 §9.3): Maintain/Admin needs
    // on personal repos get DIY instructions instead of an elevation flow.
    git.elevation_notify = control_notify.clone();

    // The API proxy shares every dependency with the git proxy: one
    // platform (single place holding api_base + token custody), one
    // policy engine, one approval backend, one registry.
    let mut api = ApiDeps::new(
        state.clone(),
        platform.clone(),
        policy,
        backend,
        machine_token.clone(),
        Duration::from_secs(APPROVAL_TIMEOUT_SECS),
    )?;
    // Grant-offer hook (GATE-010): a repo-scoped mutation that upstream
    // answers 404 offers the grant flow, debounced once per repo
    // per 24 h. Without the bot the hook stays off (offer-silent).
    api.offer = Some(GrantOfferDeps {
        state: state.clone(),
        notify: control_notify.clone(),
        machine_login: machine_login.clone(),
    });
    api.elevation_notify = control_notify.clone();

    // Machine-token lifecycle (GATE-004 §6.3): the master forwarding
    // switch is created here and shared by the gate state and the
    // lifecycle tasks; it starts OFF (fail closed) and the startup check
    // below delivers the verdict before the server binds.
    let forwarding = Arc::new(AtomicBool::new(false));
    let mut gate_state = GateState::with_git(state.clone(), audit.clone(), git);
    gate_state.api = Some(Arc::new(api));
    gate_state.max_body_bytes = cfg.max_body_bytes;
    gate_state.forwarding_enabled = forwarding.clone();
    gate_state.onepage = Some(Arc::new(OnePageDeps {
        gate_host: gate_host.clone(),
        machine_login: machine_login.clone(),
        platform: platform.clone(),
        enc_key: cfg.token_enc_key.clone(),
        notify: control_notify.clone(),
    }));

    let lifecycle_deps = Arc::new(LifecycleDeps {
        state: state.clone(),
        platform: platform.clone(),
        machine_login: cfg.machine_login.clone(),
        enc_key: cfg.token_enc_key.clone(),
        forwarding: forwarding.clone(),
        notify: control_notify.clone(),
        gate_host: gate_host.clone(),
        audit: audit.clone(),
    });

    // Reduction machinery (GATE-010 §10, task 9): the real reducer behind
    // the elevation seam — temporary grants reduce automatically at DONE,
    // /revoke drives the same conversation, the sweep expires stale
    // reduction rows and un-pressed menus (24 h no-progress, the
    // elevation expiry value). The sweep is always spawned (expiry must
    // progress); the conversation only exists with the bot.
    let reduction_deps = Arc::new(ReductionDeps {
        state: state.clone(),
        platform: platform.clone(),
        machine_login: machine_login.clone(),
        notify: control_notify.clone(),
        poll: Duration::from_secs(cfg.flow_poll_secs),
        expire_after: Duration::from_secs(cfg.elevation_expire_secs),
        audit: audit.clone(),
    });
    let (_reduction_shutdown_tx, reduction_shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(gate::control::grant::run_reduction_sweep(
        (*reduction_deps).clone(),
        reduction_shutdown_rx,
        Duration::from_secs(cfg.flow_poll_secs),
    ));

    // Elevation sweep (GATE-010 §8–9, task 8): expires stale elevation
    // flows after 24 h and takes over DETECTED rows (proxy pre-flight AND
    // the 403 error mapping feed the same rows) — consent asks go to the
    // operator chat when the bot is configured. Temporary grants reduce
    // through the real reducer (task 9).
    let elevation_deps = Arc::new(ElevationDeps {
        state: state.clone(),
        platform: platform.clone(),
        machine_login: machine_login.clone(),
        notify: control_notify.clone(),
        batch_window: Duration::from_secs(cfg.elevation_batch_secs),
        poll: Duration::from_secs(cfg.flow_poll_secs),
        reminder_after: Duration::from_secs(cfg.elevation_reminder_secs),
        expire_after: Duration::from_secs(cfg.elevation_expire_secs),
        reduction: Arc::new(RealReductionFlow::new((*reduction_deps).clone())),
        audit: audit.clone(),
    });
    let (_elevation_shutdown_tx, elevation_shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(gate::control::elevation::run_elevation_sweep(
        (*elevation_deps).clone(),
        elevation_shutdown_rx,
        Duration::from_secs(cfg.flow_poll_secs),
    ));

    // Control plane (GATE-006/013): spawn the long-poll loop ONLY
    // when a bot token is configured — core git/API serving must work
    // without the bot. An empty allowlist keeps the loop running but drops
    // every update (fail closed). The router carries the approval button
    // handler, the command handlers (/pending /block /unblock /status
    // /token /rotate /disconnect), the lifecycle rotate-start button
    // handler and the guided setup walkthrough (/setup, GATE-011);
    // callbacks are nonce-validated against the pending rows, the
    // lifecycle rotate nonce and the setup flow nonce.
    if let Some((api, _chat, allowlist, channel_name)) = channel {
        // Grant-flow machinery (GATE-010): the real implementation behind
        // setup step 8 AND the /grant command. Durations come from env
        // (GATE_FLOW_* overrides shrink them for tests).
        let grant_flow = Arc::new(RealGrantFlow::new(gate::control::grant::GrantDeps {
            state: state.clone(),
            platform: platform.clone(),
            machine_login: machine_login.clone(),
            poll: Duration::from_secs(cfg.flow_poll_secs),
            reminder_after: Duration::from_secs(cfg.flow_reminder_secs),
            give_up_after: Duration::from_secs(cfg.flow_give_up_secs),
        }));
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let mut router = ControlRouter::new();
        register_approvals(
            &mut router,
            control_backend.expect("a live channel implies a real approval backend"),
        );
        // Composed callback validation: approval rows first, then the
        // lifecycle rotate flow, then the setup walkthrough (the approval
        // layer set its own lookup; the composed one keeps all three).
        let approvals = ControlApprovalBackend::nonce_lookup(state.clone());
        let rotate = rotate_nonce_lookup(state.clone());
        let setup = setup_nonce_lookup(state.clone());
        let elevation = elevation_nonce_lookup(state.clone());
        let revoke = revoke_nonce_lookup(state.clone());
        router.set_nonce_lookup(Arc::new(move |flow| {
            approvals(flow)
                .or_else(|| rotate(flow))
                .or_else(|| setup(flow))
                .or_else(|| elevation(flow))
                .or_else(|| revoke(flow))
        }));
        register_elevation(
            &mut router,
            Arc::new(ElevationHandler::new((*elevation_deps).clone())),
        );
        register_revoke(&mut router, (*reduction_deps).clone());
        register_commands(
            &mut router,
            Arc::new(CommandDeps {
                state: state.clone(),
                audit: audit.clone(),
                machine_token: machine_token.clone(),
                lifecycle: Some(lifecycle_deps.clone()),
                grant: chat_id.map(|chat| (grant_flow.clone(), chat)),
                reduction: Some(reduction_deps.clone()),
            }),
        );
        register_lifecycle(&mut router, lifecycle_deps.clone());
        // Guided setup walkthrough (GATE-011): the grant-flow seam is fed
        // by the real machinery (control::grant::RealGrantFlow) — step 8
        // walks the invitation flow end-to-end.
        let setup_deps = Arc::new(SetupDeps {
            state: state.clone(),
            platform: platform.clone(),
            machine_login: machine_login.clone(),
            enc_key: cfg.token_enc_key.clone(),
            gate_public_url: cfg.public_url.clone(),
            chat_id: chat_id.unwrap_or(0),
            forwarding: forwarding.clone(),
            grant: grant_flow.clone(),
            audit: audit.clone(),
        });
        register_setup(&mut router, setup_deps.clone());
        let approver_count = allowlist.len();
        tokio::spawn(run_poll_loop(
            api.clone(),
            allowlist,
            tx,
            shutdown_rx,
            POLL_BACKOFF_BASE,
        ));
        tokio::spawn(router.run(rx, api.clone()));
        gate_state.control_live.store(true, Ordering::Relaxed);
        eprintln!(
            "{channel_name} control plane live ({approver_count} allowlisted approver(s))"
        );
        // Restart resume (GATE-011): resends the current setup step only
        // when its progress row is older than 1 h; fresh flows stay quiet.
        setup_resume(&setup_deps, api).await;
    }

    // Startup credential check (GATE-004 §6.3): runs BEFORE the server
    // binds — no request is ever served while the credential state is
    // unknown. Any failure lands on forwarding-off + a control-channel notice;
    // only an explicit verify pass enables forwarding.
    startup_check(&lifecycle_deps).await;

    // Daily verify + expiry-reminder cadence (GATE-004 §6.5); the first
    // tick is one full interval out (startup covered t=0).
    let (_lifecycle_shutdown_tx, lifecycle_shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(run_daily_loop(
        (*lifecycle_deps).clone(),
        lifecycle_shutdown_rx,
        DAILY_INTERVAL,
    ));

    // Registry re-check cadence (GATE-010): every registry row is
    // re-verified every 30 min (`GATE_REGISTRY_SWEEP_SECS` shrinks it for
    // tests); role drops land in the registry + one control-channel notice each.
    let registry_deps = Arc::new(RegistryDeps {
        state: state.clone(),
        platform: platform.clone(),
        machine_login: machine_login.clone(),
        notify: control_notify.clone(),
    });
    let (_registry_shutdown_tx, registry_shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(run_registry_sweep(
        (*registry_deps).clone(),
        registry_shutdown_rx,
        Duration::from_secs(cfg.registry_sweep_secs),
    ));

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
