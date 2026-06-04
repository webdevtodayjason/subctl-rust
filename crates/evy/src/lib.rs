//! Evy v4 daemon library entry point.
//!
//! Three entry points, all driven from the binary in `main.rs`:
//!
//! - [`run_smoke_test`] (`--smoke` flag) — the Phase 1 regression check.
//!   Boots scheduler + policy + providers, registers a heartbeat cron,
//!   waits for one Succeeded run, then exits. Preserves Slice E
//!   behaviour byte-for-byte so the smoke integration test in
//!   `tests/smoke.rs` keeps passing.
//! - [`run_daemon`] (default) — the Phase 3 long-lived process. Composes
//!   the FULL Phase 2+3 stack (HTTP/SSE server, observation log, playbook
//!   store, scoring ledger, preference model, feedback ingest,
//!   shared ask registry, naive retriever, optional Telegram + Discord
//!   bridges) and blocks on `tokio::signal::ctrl_c` / SIGTERM.
//! - [`run_daemon_with_shutdown`] — the testable seam. Takes a
//!   [`tokio_util::sync::CancellationToken`] the caller controls and an
//!   optional ready-signal channel; integration tests use this to learn
//!   the ephemeral HTTP port and drive shutdown without sending signals.
//!
//! # Phase 3 Slice E wiring delta vs Phase 2
//!
//! Slice E closes the v0.3.0/v0.4.0 cutover caveat (REPORT.md §4–§6):
//! the daemon binary now composes the same library stack the cutover
//! tests exercise. Concretely, [`run_daemon_with_shutdown`] does, in order:
//!
//! 1. Open the scheduler + load policy + build provider trait-objects
//!    with HMAC-stamped configs (unchanged from Phase 2).
//! 2. Open the memory substrate: [`evy_memory::ObservationLog`],
//!    [`evy_memory::PlaybookStore`], [`evy_memory::ScoreLedger`],
//!    [`evy_memory::OperatorPreferenceModel`],
//!    [`evy_memory::FeedbackIngest`]. Create the playbook dir if
//!    missing so first-boot doesn't crash on an absent `playbooks/`.
//! 3. Open the comms substrate: [`evy_comms::EventBroadcaster`],
//!    `Arc<AskRegistry>` (shared across every bridge), and the
//!    [`evy_comms::HttpServer`] bound to a `DaemonAppState` that reads
//!    through to the live scheduler + policy + obs_log.
//! 4. Optionally construct + spawn [`evy_comms::TelegramBridge`] and/or
//!    [`evy_comms::DiscordBridge`] when their config sections are
//!    present.
//! 5. Build [`evy_memory::NaiveRetriever`] wired to every memory layer
//!    (the daemon hands an `Arc<NaiveRetriever>` to future surfaces;
//!    see `daemon_state()` for the accessor).
//! 6. Emit a `DaemonBooted` event on SSE and append a `DaemonBooted`
//!    observation row.
//! 7. `scheduler.start()`, spawn HTTP / Telegram / Discord under the
//!    shared shutdown token.
//! 8. Await shutdown (either operator-provided token cancellation, or
//!    in `run_daemon` the first SIGTERM / Ctrl-C).
//! 9. Drain everything cleanly: `scheduler.stop()`, cancel the shared
//!    token (which lets HTTP/Telegram/Discord exit on the next select),
//!    `await` every spawned handle, append `DaemonShutdown` observation.
//!
//! # What's deliberately NOT here
//!
//! - **Worker registry.** `AppState::workers()` returns `Vec::new()`
//!   until a dedicated worker pool ships. The dashboard already serves
//!   the empty list correctly; populating is additive.
//! - **DeepSeek dispatch.** The stub returns `NotImplemented` at
//!   dispatch — `deepseek-builder` is replacing it in a parallel slice.
//! - **Thinking-partner surface.** `thinking-partner-builder` ships a
//!   new crate in parallel; a follow-up slice wires it.
//! - **`recent_events` on `AppState`.** The trait has
//!   `workers / jobs / policy`. Adding a recent-events method would
//!   need an evy-comms extension; this slice keeps the trait intact and
//!   relies on the live SSE stream for recent events instead.

#![warn(missing_docs)]

pub mod config;
pub mod state;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use evy_comms::{
    AskRegistry, DaemonEvent, DiscordBridge, EventBroadcaster, HttpServer, TelegramBridge,
};
use evy_core::Provider;
use evy_memory::{
    observation::ObservationKind, FeedbackIngest, NaiveRetriever, Observation, ObservationLog,
    OperatorPreferenceModel, PlaybookStore, ScoreLedger,
};
use evy_policy::{check_command_simple, load_policy, Mode};
use evy_providers::{ClaudeCodeProvider, CodexProvider, DeepSeekProvider, HmacKey};
use evy_scheduler::{Job, JobAction, JobId, RunOutcome, Scheduler};
use evy_skills::SkillRegistry;
use evy_thinking::{
    AnthropicBackend, AnthropicConfig, CodexOauthBackend, CodexOauthConfig, LlmBackend,
    LmStudioBackend, LmStudioConfig, ThinkingPartner,
};
use tokio::signal;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

pub use config::Config;
pub use state::DaemonAppState;

/// Workspace version reported on `/health`, `/api/version`, and the
/// `DaemonBooted` SSE event.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Initialise the global tracing subscriber.
///
/// Honours `RUST_LOG`; defaults to `evy=info,evy_scheduler=info,evy_policy=info`
/// when the env var is absent. Idempotent: calling twice is a no-op (the
/// global subscriber `try_init` swallows the error).
pub fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "evy=info,evy_scheduler=info,evy_policy=info,evy_providers=info,evy_comms=info",
        )
    });
    // try_init lets repeated calls (binary + tests in-process) coexist.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .with_test_writer()
        .try_init();
}

/// Outcome of one Phase 1 smoke run.
///
/// Returned to the caller (binary `main` or integration test) so the
/// wall-clock figure can be reported without re-deriving it.
#[derive(Debug, Clone)]
pub struct SmokeReport {
    /// Job id that fired.
    pub job_id: JobId,
    /// Wall-clock time from `Scheduler::start` to the first run row appearing.
    pub fire_latency: Duration,
    /// Total time spent inside [`run_smoke_test`].
    pub total_wall_clock: Duration,
}

// ─── shared boot ────────────────────────────────────────────────────────

/// Open the scheduler, load policy, build provider trait-objects. Used
/// by both `run_smoke_test` and the daemon entry points.
///
/// Mints a fresh [`HmacKey`] and stamps it into every provider config so
/// downstream dispatch wraps every directive in the ADR-0011 trust
/// marker. The key is borrowed by the configs (each `Provider` keeps its
/// own clone via `ClaudeCodeConfig::hmac_key`); the live key sits in the
/// returned tuple so the caller can also pass it to future surfaces
/// (e.g. the verifier endpoint).
async fn boot_components(config: &Config) -> Result<(Scheduler, Vec<Box<dyn Provider>>, HmacKey)> {
    let scheduler = Scheduler::open(&config.scheduler.db_path)
        .await
        .with_context(|| {
            format!(
                "opening scheduler db at {}",
                config.scheduler.db_path.display()
            )
        })?;
    tracing::info!(
        db = %config.scheduler.db_path.display(),
        "scheduler opened (sqlite migrations applied)",
    );

    let policy = load_policy(&config.policy.path)
        .with_context(|| format!("loading policy from {}", config.policy.path.display()))?;
    tracing::info!(
        path = %config.policy.path.display(),
        default_mode = ?policy.default_mode,
        "policy loaded",
    );
    let policy_check = check_command_simple("ls -la", &policy, Mode::Trusted);
    tracing::info!(?policy_check, "policy gate exercised (ls -la / Trusted)");

    let session_key = HmacKey::generate();
    tracing::info!("session HMAC key minted (32 bytes, in-memory only — never persisted)");

    let providers =
        load_providers(config, &session_key).context("constructing provider trait-objects")?;
    for p in &providers {
        match p.healthcheck().await {
            Ok(()) => tracing::info!(provider = ?p.kind(), "healthcheck ok"),
            Err(e) => tracing::info!(
                provider = ?p.kind(),
                reason = %e,
                "healthcheck failed (expected without a real tmux session / live CLI)",
            ),
        }
    }
    Ok((scheduler, providers, session_key))
}

/// Build the provider trait-object vec, stamping `session_key` into each
/// config so dispatch wraps directives in the ADR-0011 trust marker.
fn load_providers(config: &Config, session_key: &HmacKey) -> Result<Vec<Box<dyn Provider>>> {
    let mut out: Vec<Box<dyn Provider>> = Vec::new();
    if let Some(cc) = config.providers.claude_code.clone() {
        let mut adapter_cfg: evy_providers::ClaudeCodeConfig = cc.into();
        adapter_cfg.hmac_key = Some(session_key.clone());
        out.push(Box::new(ClaudeCodeProvider::new(adapter_cfg)));
    }
    if let Some(cx) = config.providers.codex.clone() {
        let mut adapter_cfg: evy_providers::CodexConfig = cx.into();
        adapter_cfg.hmac_key = Some(session_key.clone());
        out.push(Box::new(CodexProvider::new(adapter_cfg)));
    }
    // DeepSeek is always in the vec so the trait-object pool is
    // exercised against three providers from day one. Its dispatch +
    // healthcheck return Error::Provider per ADR 0020 Phase-deferred items.
    out.push(Box::new(DeepSeekProvider::new()));
    if out.len() < 2 {
        // 2 = DeepSeek + at least one real provider.
        anyhow::bail!("no real providers configured (need at least one of claude_code / codex)");
    }
    Ok(out)
}

// ─── Phase 1 smoke (preserved behind --smoke) ──────────────────────────

/// Boot the daemon, run the Phase 1 smoke sequence, and exit cleanly.
///
/// Replaces what was [`run_daemon`] in Slice E. The behaviour is
/// unchanged byte-for-byte — `tests/smoke.rs` drives this directly to
/// guard against Phase 2+3 regressions.
///
/// # Errors
/// Returns `Err` if the scheduler db cannot be opened, the policy file
/// cannot be parsed, the smoke job fails to fire within the 75-second
/// budget, or graceful shutdown fails.
pub async fn run_smoke_test(config: Config) -> Result<SmokeReport> {
    let start = Instant::now();
    tracing::info!(?config, "evy v4 booting (Phase 1 smoke test)");

    let (scheduler, _providers, _key) = boot_components(&config).await?;

    let job_unique_id = JobId::new();
    let job = Job {
        id: job_unique_id,
        name: format!("phase1-smoke-heartbeat-{job_unique_id}"),
        cron_expr: "* * * * *".to_owned(),
        action: JobAction::LogHeartbeat,
        enabled: true,
        created_at: Utc::now(),
        last_run: None,
    };
    let job_id = job.id;
    scheduler
        .register(job)
        .await
        .context("registering smoke heartbeat job")?;
    tracing::info!(%job_id, "smoke job registered");

    scheduler
        .start()
        .await
        .context("starting scheduler fire loop")?;
    let fire_started = Instant::now();
    let report = wait_for_first_run(&scheduler, job_id)
        .await
        .context("waiting for the smoke job to fire")?;
    let fire_latency = fire_started.elapsed();
    tracing::info!(
        ?fire_latency,
        runs = ?report,
        "smoke job fired; runs table contains expected row",
    );

    scheduler.stop().await.context("stopping scheduler")?;
    tracing::info!("evy v4 smoke test complete; exiting cleanly");

    Ok(SmokeReport {
        job_id,
        fire_latency,
        total_wall_clock: start.elapsed(),
    })
}

// ─── Phase 3 long-lived daemon ─────────────────────────────────────────

/// Optional in-process hooks the testable [`run_daemon_with_shutdown`]
/// surfaces back to the caller. Production callers (`run_daemon`) pass
/// [`DaemonHooks::default`]; integration tests pass a oneshot to learn
/// the ephemeral HTTP socket addr after `bind` succeeds.
#[derive(Default)]
pub struct DaemonHooks {
    /// Sent the actual bound HTTP `SocketAddr` once `HttpServer::bind`
    /// succeeds (which happens before `serve` is awaited). Tests that
    /// configure `port = 0` need this to learn the ephemeral port the
    /// kernel assigned. Sender is `take`n on use; subsequent test asserts
    /// see `None` if they peek.
    pub http_ready: Option<oneshot::Sender<SocketAddr>>,
}

/// Boot the daemon and block until SIGTERM or Ctrl-C, then drain cleanly.
///
/// Thin wrapper around [`run_daemon_with_shutdown`] that mints a
/// [`CancellationToken`], arms `tokio::signal::ctrl_c` + (on unix)
/// SIGTERM watchers on it, and passes empty [`DaemonHooks`].
///
/// # Errors
/// Returns `Err` on boot failure or if scheduler shutdown fails.
pub async fn run_daemon(config: Config) -> Result<()> {
    let shutdown = CancellationToken::new();
    let signal_token = shutdown.clone();
    // Spawn the signal watcher in the background. It races SIGTERM and
    // Ctrl-C; whichever fires first cancels the shared token, which the
    // daemon's main select arm picks up and drains on.
    tokio::spawn(async move {
        let kind = wait_for_shutdown_signal().await;
        tracing::info!(?kind, "shutdown signal received; cancelling shared token");
        signal_token.cancel();
    });
    run_daemon_with_shutdown(config, shutdown, DaemonHooks::default()).await
}

/// Boot the daemon, compose every Phase 2+3 component, and run until
/// `shutdown` is cancelled.
///
/// This is the testable seam. [`run_daemon`] is a wrapper that arms a
/// signal-driven cancel; integration tests cancel the token directly.
///
/// # Errors
/// Returns `Err` on boot failure (db open, policy parse, http bind,
/// playbook dir create, memory open) or if scheduler shutdown fails.
#[allow(clippy::too_many_lines)] // composition function — extracting helpers
                                 // hurts readability more than it helps.
pub async fn run_daemon_with_shutdown(
    config: Config,
    shutdown: CancellationToken,
    mut hooks: DaemonHooks,
) -> Result<()> {
    tracing::info!(?config, "evy v4 booting (Phase 3 daemon)");

    // ── 1. Existing pieces: scheduler + providers + policy + HMAC ────
    let (scheduler, providers, _session_key) = boot_components(&config).await?;
    let scheduler = Arc::new(scheduler);
    // Reload the policy for sharing — `boot_components` consumes it via
    // the inner `load_policy` call but doesn't return it. Reading the
    // file again is cheap (it's the operator's policy.toml) and keeps
    // the boot-time validation path exercised once.
    let policy = Arc::new(
        load_policy(&config.policy.path)
            .with_context(|| format!("re-loading policy from {}", config.policy.path.display()))?,
    );

    // ── 2. Memory substrate ──────────────────────────────────────────
    // Create the playbook dir if missing so first-boot on a clean
    // operator install doesn't crash. The store still rejects file-
    // shaped paths (the operator gets a clear error).
    if !config.memory.playbook_dir.exists() {
        std::fs::create_dir_all(&config.memory.playbook_dir).with_context(|| {
            format!(
                "creating playbook dir at {}",
                config.memory.playbook_dir.display()
            )
        })?;
        tracing::info!(
            dir = %config.memory.playbook_dir.display(),
            "playbook dir did not exist; created"
        );
    }

    let obs_log = Arc::new(
        ObservationLog::open(&config.memory.observation_db)
            .await
            .with_context(|| {
                format!(
                    "opening observation log at {}",
                    config.memory.observation_db.display()
                )
            })?,
    );
    let playbooks = Arc::new(
        PlaybookStore::load(&config.memory.playbook_dir).with_context(|| {
            format!(
                "loading playbook store from {}",
                config.memory.playbook_dir.display()
            )
        })?,
    );
    let scoring = Arc::new(
        ScoreLedger::open(&config.memory.score_db)
            .await
            .with_context(|| {
                format!(
                    "opening score ledger at {}",
                    config.memory.score_db.display()
                )
            })?,
    );
    let preferences = Arc::new(
        OperatorPreferenceModel::open(&config.memory.preferences_db)
            .await
            .with_context(|| {
                format!(
                    "opening preference model at {}",
                    config.memory.preferences_db.display()
                )
            })?,
    );
    // Feedback ingest shares the scoring db (migrator is idempotent —
    // both `feedback` and `worker_effectiveness_scores` live under the
    // same evy-memory migrations set). Folding into one file keeps the
    // operator's `[memory]` table short.
    let feedback = Arc::new(
        FeedbackIngest::open(
            &config.memory.score_db,
            obs_log.clone(),
            scoring.clone(),
            preferences.clone(),
        )
        .await
        .with_context(|| {
            format!(
                "opening feedback ingest at {}",
                config.memory.score_db.display()
            )
        })?,
    );

    // Naive retriever wired to every layer. Held in an Arc so future
    // surfaces (TUI / dashboard /api/evy/retrieve / Telegram /retrieve)
    // can share without an additional rebuild.
    let _retriever = Arc::new(
        NaiveRetriever::new(obs_log.clone(), None, playbooks.clone())
            .with_scoring(scoring.clone())
            .with_preferences(preferences.clone())
            .with_feedback(feedback.clone()),
    );
    // TODO: Phase 3+ — once a thinking-partner crate lands, an
    // Option<Arc<...>> here lets the daemon hand the retriever to it.

    // ── 3. Comms substrate ───────────────────────────────────────────
    let event_broadcaster = EventBroadcaster::default();
    let ask_registry = Arc::new(AskRegistry::new());

    // Phase 5 + 6 — optional skill registry. Loaded if `[skills]` is
    // enabled; passed to the thinking-partner so the chat surface can
    // see which skills the model has access to per turn. Failure to
    // load is logged (not fatal) so a misconfigured catalog doesn't
    // take down the daemon.
    let skills_registry = if config.skills.enabled {
        match SkillRegistry::load(&config.skills.directory) {
            Ok(reg) => {
                tracing::info!(
                    dir = %config.skills.directory.display(),
                    count = reg.count(),
                    "skill registry loaded",
                );
                Some(Arc::new(reg))
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    dir = %config.skills.directory.display(),
                    "skill registry load failed; continuing without skills",
                );
                None
            }
        }
    } else {
        tracing::info!("skill catalog disabled in config; not loading");
        None
    };

    // Phase 6 — optional thinking-partner. Constructed only when the
    // operator authored `[thinking_partner]` in their TOML. The
    // anthropic backend is the only production wiring today; "stub"
    // is intended for smoke tests that need a partner without
    // touching the live API.
    let thinking_partner = build_thinking_partner(&config, skills_registry.clone()).await?;
    if let Some(partner) = thinking_partner.as_ref() {
        // P3 — rehydrate persisted sessions before serving so a restart
        // doesn't silently lose the operator's open conversation.
        let restored = partner.restore().await;
        tracing::info!(restored, "thinking-partner constructed; chat endpoint is live");
    } else {
        tracing::info!("no [thinking_partner] in config; chat endpoint will return 503");
    }

    let mut app_state = DaemonAppState::new(scheduler.clone(), policy.clone(), obs_log.clone());
    if let Some(partner) = thinking_partner.clone() {
        app_state = app_state.with_thinking_partner(partner);
    }
    if let Some(skills) = skills_registry.clone() {
        app_state = app_state.with_skills(skills);
    }
    // P2 — supervisor label for the /api/evy/context meter: "backend/model".
    if let Some(tp) = config.thinking_partner.as_ref() {
        let label = match &tp.model {
            Some(m) => format!("{}/{}", tp.backend, m),
            None => tp.backend.clone(),
        };
        app_state = app_state.with_supervisor_label(label);
    }
    let app_state = Arc::new(app_state);
    let http_server = HttpServer::new(
        config.comms.http.clone().into(),
        event_broadcaster.clone(),
        app_state.clone(),
    );
    let bound = http_server
        .bind()
        .await
        .context("binding evy-comms HTTP server")?;
    let http_addr = bound.local_addr();
    tracing::info!(addr = %http_addr, "HTTP server bound");
    if let Some(tx) = hooks.http_ready.take() {
        // Test hook — surface the bound port. Caller-dropped receiver
        // is fine; the daemon proceeds either way.
        let _ = tx.send(http_addr);
    }

    // Optional Telegram bridge. `bridge.clone().run(token)` is the
    // long-lived loop; the original handle stays for `notify` / `ask`.
    let telegram_bridge = config.comms.telegram.clone().map(|tg| {
        let bridge = TelegramBridge::new(tg.into(), ask_registry.clone());
        tracing::info!("telegram bridge configured");
        bridge
    });

    // Optional Discord bridge. Same shape as telegram — `run(token)`
    // long-lived, `notify`/`ask` on the original handle.
    let discord_bridge = config.comms.discord.clone().map(|dc| {
        let bridge = DiscordBridge::new(dc.into(), ask_registry.clone());
        tracing::info!("discord bridge configured");
        bridge
    });

    // ── 4. Boot-time event + observation ─────────────────────────────
    let provider_kinds: Vec<_> = providers.iter().map(|p| p.kind()).collect();
    let booted_event = DaemonEvent::DaemonBooted {
        version: VERSION.to_owned(),
        providers: provider_kinds.clone(),
    };
    event_broadcaster.emit(booted_event);
    obs_log
        .append(Observation::new(ObservationKind::DaemonBooted {
            version: VERSION.to_owned(),
        }))
        .await
        .context("appending DaemonBooted observation")?;

    // ── 5. Start scheduler + spawn long-lived tasks ──────────────────
    scheduler
        .start()
        .await
        .context("starting scheduler fire loop")?;

    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // HTTP server runs under the shared shutdown token.
    {
        let shutdown_for_http = shutdown.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = bound.serve(shutdown_for_http).await {
                tracing::error!(error = %e, "HTTP server exited with error");
            }
        });
        handles.push(handle);
    }

    if let Some(bridge) = telegram_bridge {
        let shutdown_for_tg = shutdown.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = bridge.run(shutdown_for_tg).await {
                tracing::error!(error = %e, "telegram bridge exited with error");
            }
        });
        handles.push(handle);
    }

    if let Some(bridge) = discord_bridge {
        let shutdown_for_dc = shutdown.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = bridge.run(shutdown_for_dc).await {
                tracing::error!(error = %e, "discord bridge exited with error");
            }
        });
        handles.push(handle);
    }

    tracing::info!(
        version = VERSION,
        providers = ?provider_kinds,
        http_addr = %http_addr,
        playbooks = playbooks.count(),
        "evy v4 daemon ready"
    );

    // ── 6. Await shutdown ────────────────────────────────────────────
    shutdown.cancelled().await;
    tracing::info!("shutdown token fired; draining");

    // ── 7. Graceful drain ────────────────────────────────────────────
    if let Err(e) = scheduler.stop().await {
        tracing::error!(error = %e, "scheduler shutdown failed; continuing exit");
    }
    for handle in handles {
        if let Err(e) = handle.await {
            tracing::warn!(error = %e, "spawned task join error");
        }
    }
    if let Err(e) = obs_log
        .append(Observation::new(ObservationKind::DaemonShutdown {
            reason: "shutdown_token_cancelled".to_owned(),
        }))
        .await
    {
        tracing::warn!(error = %e, "appending DaemonShutdown observation failed");
    }
    tracing::info!("evy v4 daemon stopped cleanly");
    Ok(())
}

/// Which signal triggered shutdown. Logged so operators can tell a
/// `kill` from a Ctrl-C from a `launchctl unload`.
#[derive(Debug, Clone, Copy)]
enum ShutdownSignal {
    /// Operator Ctrl-C (or `kill -INT`).
    CtrlC,
    /// `kill -TERM`, `launchctl unload`, etc. Unix-only.
    Terminate,
}

/// Block until the first OS signal we care about arrives.
///
/// On macOS + Linux: races `ctrl_c` and SIGTERM via `tokio::select!`.
/// On Windows (not actively supported, but kept compiling): only
/// `ctrl_c`. The brief targets Mac/Linux for the daemon — a future
/// Windows port would need to handle CTRL_BREAK_EVENT separately.
async fn wait_for_shutdown_signal() -> ShutdownSignal {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        // SIGTERM signal stream is constructed eagerly so any setup
        // error surfaces before we start awaiting. If construction
        // fails (kernel-level brokenness), fall back to ctrl_c only —
        // logging the failure rather than crashing the daemon.
        let term = match signal(SignalKind::terminate()) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "could not install SIGTERM handler; daemon will only respond to Ctrl-C",
                );
                None
            }
        };
        match term {
            Some(mut term_stream) => {
                tokio::select! {
                    _ = signal::ctrl_c() => ShutdownSignal::CtrlC,
                    _ = term_stream.recv() => ShutdownSignal::Terminate,
                }
            }
            None => {
                let _ = signal::ctrl_c().await;
                ShutdownSignal::CtrlC
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = signal::ctrl_c().await;
        ShutdownSignal::CtrlC
    }
}

// ─── Phase 6 — thinking-partner construction ───────────────────────────

/// Construct the thinking-partner from `[thinking_partner]` config.
///
/// Returns `Ok(None)` when the section is absent. Returns `Err` when
/// the section is present but invalid (e.g. requested backend
/// `"anthropic"` but the configured env var is empty).
///
/// The "stub" backend is a fixed-reply implementation useful for
/// integration tests that need a partner without touching Anthropic.
async fn build_thinking_partner(
    config: &Config,
    skills: Option<Arc<SkillRegistry>>,
) -> Result<Option<Arc<ThinkingPartner>>> {
    let Some(cfg) = config.thinking_partner.as_ref() else {
        return Ok(None);
    };
    let backend: Arc<dyn LlmBackend> = match cfg.backend.as_str() {
        "anthropic" => {
            let key = std::env::var(&cfg.api_key_env).with_context(|| {
                format!(
                    "reading thinking-partner API key from env var {}",
                    cfg.api_key_env
                )
            })?;
            if key.trim().is_empty() {
                anyhow::bail!("thinking-partner: env var {} is empty", cfg.api_key_env);
            }
            let mut anth_cfg = AnthropicConfig::new(key);
            if let Some(model) = &cfg.model {
                anth_cfg.model = model.clone();
            }
            if let Some(mt) = cfg.max_tokens {
                anth_cfg.max_tokens = mt;
            }
            let mut backend = AnthropicBackend::new(anth_cfg);
            if let Some(reg) = skills {
                backend = backend.with_skills(reg);
            }
            Arc::new(backend)
        }
        "lm-studio" => {
            // LM Studio binds 127.0.0.1:1234 by default, no auth. The
            // [thinking_partner.lm_studio] block carries endpoint /
            // temperature overrides; model + max_tokens fall through
            // the parent [thinking_partner] keys so operators don't
            // have to repeat them.
            let mut lm_cfg = LmStudioConfig::default();
            if let Some(section) = &cfg.lm_studio {
                if let Some(ep) = &section.endpoint {
                    lm_cfg.endpoint = ep.clone();
                }
                if let Some(temp) = section.temperature {
                    lm_cfg.temperature = temp;
                }
            }
            if let Some(model) = &cfg.model {
                lm_cfg.model = Some(model.clone());
            }
            if let Some(mt) = cfg.max_tokens {
                lm_cfg.max_tokens = mt;
            }
            // Skills autoload is Anthropic-only today (see lm_studio
            // module docs) — `skills` is intentionally not consumed
            // here; future work can add it once local-model tool-use
            // is reliable.
            let _ = skills;
            Arc::new(LmStudioBackend::new(lm_cfg))
        }
        "codex" => {
            // Codex OAuth — reuses tokens minted by `subctl auth codex`
            // (Phase 4 Slice D's device-flow). No env-var lookup; the
            // bundle lives in `<config_dir>/auth.json` as referenced by
            // `accounts.conf`.
            let codex_cfg = cfg.codex.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "thinking-partner: backend = \"codex\" requires a [thinking_partner.codex] section with `account = ...`"
                )
            })?;
            if codex_cfg.account.trim().is_empty() {
                anyhow::bail!(
                    "thinking-partner: [thinking_partner.codex].account is empty"
                );
            }
            let mut backend_cfg = CodexOauthConfig::new(codex_cfg.account.clone());
            backend_cfg.accounts_conf_path = codex_cfg.accounts_conf_path.clone();
            if let Some(model) = &cfg.model {
                backend_cfg.model = model.clone();
            }
            if let Some(mt) = cfg.max_tokens {
                backend_cfg.max_tokens = mt;
            }
            if let Some(ep) = &codex_cfg.endpoint {
                backend_cfg.endpoint = ep.clone();
            }
            let mut backend = CodexOauthBackend::new(backend_cfg)
                .context("constructing CodexOauthBackend")?;
            if let Some(reg) = skills {
                backend = backend.with_skills(reg);
            }
            Arc::new(backend)
        }
        "stub" => Arc::new(StubBackend),
        other => anyhow::bail!(
            "thinking-partner: unknown backend {other:?} (expected \"anthropic\", \"lm-studio\", \"codex\", or \"stub\")"
        ),
    };
    // P3 — persist sessions next to the other v4 state dbs so chats survive
    // a daemon restart (Goal 3). Snapshot lives at <state_dir>/evy-sessions.json.
    let store_path = config
        .memory
        .observation_db
        .parent()
        .map(|d| d.join("evy-sessions.json"))
        .unwrap_or_else(|| std::path::PathBuf::from("evy-sessions.json"));
    let partner = ThinkingPartner::new(backend).with_store_path(store_path);
    Ok(Some(Arc::new(partner)))
}

/// Stub LLM backend for `[thinking_partner].backend = "stub"`. Always
/// returns a fixed conversational reply so smoke tests can exercise
/// the daemon-side wiring without an API key.
struct StubBackend;

#[async_trait::async_trait]
impl LlmBackend for StubBackend {
    async fn respond(
        &self,
        _system_prompt: &str,
        _messages: &[evy_thinking::Message],
    ) -> evy_thinking::Result<String> {
        Ok(
            "stub thinking-partner reply — set [thinking_partner].backend = \"anthropic\" for production"
                .to_string(),
        )
    }
}

// ─── helpers ────────────────────────────────────────────────────────────

/// Poll the runs table until exactly one row is present for `job_id`.
/// Budget mirrors `crates/evy-scheduler/tests/integration.rs` (75s) —
/// 5-field cron has minute granularity, so worst-case wait is ~60s plus
/// scheduler-side latency.
async fn wait_for_first_run(
    scheduler: &Scheduler,
    job_id: JobId,
) -> Result<Vec<evy_scheduler::Run>> {
    let budget = Duration::from_secs(75);
    let started = Instant::now();
    while started.elapsed() < budget {
        let runs = scheduler
            .list_runs(job_id)
            .await
            .with_context(|| format!("list_runs({job_id})"))?;
        if let Some(r) = runs.iter().find(|r| r.outcome == RunOutcome::Succeeded) {
            tracing::info!(run_id = %r.id, "found Succeeded run row");
            return Ok(runs);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!(
        "smoke timed out after {}s waiting for the heartbeat job to fire",
        budget.as_secs(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_report_is_debug() {
        let r = SmokeReport {
            job_id: JobId::new(),
            fire_latency: Duration::from_secs(3),
            total_wall_clock: Duration::from_secs(5),
        };
        let s = format!("{r:?}");
        assert!(s.contains("fire_latency"));
    }

    #[test]
    fn shutdown_signal_is_debug() {
        // Both variants format without panicking — sanity check.
        let s_ctrlc = format!("{:?}", ShutdownSignal::CtrlC);
        let s_term = format!("{:?}", ShutdownSignal::Terminate);
        assert_eq!(s_ctrlc, "CtrlC");
        assert_eq!(s_term, "Terminate");
    }

    #[test]
    fn daemon_hooks_default_carries_no_sender() {
        let h = DaemonHooks::default();
        assert!(h.http_ready.is_none());
    }
}
