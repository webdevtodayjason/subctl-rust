//! Evy v4 daemon library entry point.
//!
//! Two entry points, both driven from the binary in `main.rs`:
//!
//! - [`run_smoke_test`] (`--smoke` flag) — the Phase 1 regression check.
//!   Boots scheduler + policy + providers, registers a heartbeat cron,
//!   waits for one Succeeded run, then exits. Preserves Slice E
//!   behaviour byte-for-byte so the smoke integration test in
//!   `tests/smoke.rs` keeps passing.
//! - [`run_daemon`] (default) — the Phase 2 long-lived process. Boots
//!   the same wiring, mints a per-session HMAC key, attaches it to the
//!   provider configs, and blocks on `tokio::signal::ctrl_c` / SIGTERM.
//!   On signal: drains the scheduler, returns Ok. Exit code 0.
//!
//! # Phase 2 wiring delta vs Phase 1
//!
//! - Per-session [`HmacKey`] minted at boot. Threaded into every
//!   provider config so every dispatched directive carries an ADR-0011
//!   HMAC trust marker.
//! - Graceful shutdown: `tokio::signal::ctrl_c` + (unix-only) SIGTERM
//!   watchers race a never-completing future; first to fire triggers a
//!   `scheduler.stop().await` and a clean return.
//! - HTTP server bind port is logged in the "ready" line as a
//!   placeholder. Slice 2B lands the real axum router.
//!
//! # TODO markers carried forward to later slices
//!
//! - Slice 2B: wire `evy-comms` HTTP router (axum) + `/health` + `/metrics`.
//! - Slice 2B: replace the smoke-test cron with a real operator schedule
//!   sourced from a `jobs.toml` file.
//! - Slice 2C: snapshot dispatched mandates into `evy-memory`.
//! - Phase 3: persist the per-session `HmacKey` under
//!   `~/.config/subctl/`, with rotation + worker re-key support.

#![warn(missing_docs)]

pub mod config;

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use evy_core::Provider;
use evy_policy::{check_command_simple, load_policy, Mode};
use evy_providers::{ClaudeCodeProvider, CodexProvider, DeepSeekProvider, HmacKey};
use evy_scheduler::{Job, JobAction, JobId, RunOutcome, Scheduler};
use tokio::signal;
use tracing_subscriber::EnvFilter;

pub use config::Config;

/// Placeholder port for the future HTTP control surface. Slice 2B will
/// replace this with a real configurable bind address sourced from
/// [`Config`].
const PLACEHOLDER_HTTP_PORT: u16 = 7654;

/// Initialise the global tracing subscriber.
///
/// Honours `RUST_LOG`; defaults to `evy=info,evy_scheduler=info,evy_policy=info`
/// when the env var is absent. Idempotent: calling twice is a no-op (the
/// global subscriber `try_init` swallows the error).
pub fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("evy=info,evy_scheduler=info,evy_policy=info,evy_providers=info")
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
/// by both `run_smoke_test` and `run_daemon`.
///
/// Mints a fresh [`HmacKey`] and stamps it into every provider config so
/// downstream dispatch wraps every directive in the ADR-0011 trust
/// marker. The key is borrowed by the configs (each `Provider` keeps its
/// own clone via `ClaudeCodeConfig::hmac_key`); the live key sits in the
/// returned tuple so the caller can also pass it to future surfaces
/// (e.g. the verifier endpoint in 2B).
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
/// guard against Phase 2 regressions.
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

// ─── Phase 2 long-lived daemon ─────────────────────────────────────────

/// Boot the daemon and block until SIGTERM or Ctrl-C, then drain cleanly.
///
/// # Lifecycle
///
/// 1. [`boot_components`] opens the scheduler, loads policy, builds
///    provider trait-objects with HMAC-stamped configs.
/// 2. `scheduler.start()` boots the fire loop. (Slice 2A registers no
///    operator-supplied cron jobs; Slice 2B introduces `jobs.toml`.)
/// 3. A "ready" log line announces which providers are configured and
///    the placeholder HTTP port future surfaces will bind.
/// 4. The function awaits the first of: `tokio::signal::ctrl_c()`,
///    SIGTERM (unix only). When either fires, the function calls
///    `scheduler.stop().await` and returns.
///
/// # Errors
/// Returns `Err` on boot failure or if scheduler shutdown fails.
/// Signal-watching itself is infallible (modulo `tokio::signal::unix`
/// errors, which only happen on broken kernels).
pub async fn run_daemon(config: Config) -> Result<()> {
    tracing::info!(?config, "evy v4 booting (Phase 2 daemon)");
    let (scheduler, providers, _session_key) = boot_components(&config).await?;
    scheduler
        .start()
        .await
        .context("starting scheduler fire loop")?;

    let provider_kinds: Vec<_> = providers.iter().map(|p| p.kind()).collect();
    tracing::info!(
        providers = ?provider_kinds,
        http_port = PLACEHOLDER_HTTP_PORT,
        "evy v4 daemon ready — awaiting SIGTERM / Ctrl-C",
    );

    let signal_kind = wait_for_shutdown_signal().await;
    tracing::info!(?signal_kind, "shutdown signal received; draining");

    if let Err(e) = scheduler.stop().await {
        tracing::error!(error = %e, "scheduler shutdown failed; continuing exit");
        // Surface the error to the caller so the process exits non-zero
        // on a dirty shutdown — operator can investigate the failure.
        return Err(anyhow::Error::from(e).context("scheduler.stop during graceful shutdown"));
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
}
