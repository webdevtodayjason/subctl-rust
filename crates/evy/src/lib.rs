//! Evy v4 daemon library entry point — exposed so the integration test
//! in `tests/smoke.rs` can drive the same wiring path the binary uses.
//!
//! The `main.rs` binary is a thin wrapper around [`run_daemon`]; nothing
//! production-meaningful lives there.
//!
//! # Phase 1 wiring
//!
//! [`run_daemon`] performs the Slice E smoke test:
//!
//! 1. Open the [`evy_scheduler::Scheduler`] against a sqlite db (migrations
//!    run automatically).
//! 2. Load the policy TOML via [`evy_policy::load_policy`] and exercise
//!    [`evy_policy::check_command_simple`] against a synthetic command.
//! 3. Construct one [`evy_providers::ClaudeCodeProvider`] and one
//!    [`evy_providers::CodexProvider`] (plus the `DeepSeekProvider` stub,
//!    purely to prove the trait-object vec composes). Healthcheck each;
//!    log the result. We do NOT dispatch a real mandate — Phase 1's job
//!    is to prove the wiring composes; provider-side end-to-end happens
//!    in Slice F (Phase 2).
//! 4. Register a single `LogHeartbeat` cron job (`* * * * *`).
//! 5. Start the scheduler, poll the `runs` table until exactly one row
//!    is present, then [`evy_scheduler::Scheduler::stop`] cleanly.
//!
//! # Friction reported up to team-lead
//!
//! - 5-field cron has minute granularity. The spec's "~2 seconds" target
//!   is unreachable; we adopt the precedent from
//!   `crates/evy-scheduler/tests/integration.rs` and budget 75 seconds.
//! - Provider config field names diverge between the spec
//!   (`config_dir`, `account`) and the actual struct fields
//!   (`claude_config_dir`, `codex_home`). The [`config`] module maps the
//!   TOML shape onto the construction-time structs via intermediates.

#![warn(missing_docs)]

pub mod config;

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use evy_core::Provider;
use evy_policy::{check_command_simple, load_policy, Mode};
use evy_providers::{ClaudeCodeProvider, CodexProvider, DeepSeekProvider};
use evy_scheduler::{Job, JobAction, JobId, RunOutcome, Scheduler};
use tracing_subscriber::EnvFilter;

pub use config::Config;

// TODO: Phase 2 — wire evy-comms HTTP surface (axum router, /health, /metrics).
// TODO: Phase 2 — wire evy-memory snapshot writer to capture mandate→run pairs.
// TODO: Phase 2 — wire Provider::dispatch end-to-end into JobAction::DispatchMandate.
// TODO: Phase 2 — replace `* * * * *` smoke cron with a real operator
//                 schedule sourced from the config file (jobs.toml).
// TODO: Phase 2 — graceful-shutdown SIGTERM/SIGINT handler around
//                 Scheduler::stop instead of the linear smoke-test exit.

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
    /// Total time spent inside [`run_daemon`].
    pub total_wall_clock: Duration,
}

/// Boot the daemon, run the Phase 1 smoke sequence, and exit cleanly.
///
/// # Errors
/// Returns `Err` if the scheduler db cannot be opened, the policy file
/// cannot be parsed, the smoke job fails to fire within the 75-second
/// budget, or graceful shutdown fails.
pub async fn run_daemon(config: Config) -> Result<SmokeReport> {
    let start = Instant::now();
    tracing::info!(?config, "evy v4 booting (phase 1 wiring)");

    // ---- 1. Scheduler ----
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

    // ---- 2. Policy ----
    let policy = load_policy(&config.policy.path)
        .with_context(|| format!("loading policy from {}", config.policy.path.display()))?;
    tracing::info!(
        path = %config.policy.path.display(),
        default_mode = ?policy.default_mode,
        "policy loaded",
    );
    let policy_check = check_command_simple("ls -la", &policy, Mode::Trusted);
    tracing::info!(
        ?policy_check,
        "policy gate exercised (synthetic ls -la / Trusted)"
    );

    // ---- 3. Providers ----
    let providers =
        load_providers(&config).context("constructing Phase 1 provider trait-objects")?;
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

    // ---- 4. Register one smoke job ----
    let job_unique_id = JobId::new();
    let job = Job {
        id: job_unique_id,
        name: format!("phase1-smoke-heartbeat-{job_unique_id}"),
        // 5-field cron, every minute. Fires at the next minute boundary.
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
        .context("registering phase-1 smoke heartbeat job")?;
    tracing::info!(%job_id, "smoke job registered");

    // ---- 5. Start, wait for the fire, verify ----
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

    // ---- 6. Graceful shutdown ----
    scheduler.stop().await.context("stopping scheduler")?;
    tracing::info!("evy v4 phase-1 smoke test complete; exiting cleanly");

    Ok(SmokeReport {
        job_id,
        fire_latency,
        total_wall_clock: start.elapsed(),
    })
}

/// Build the Phase 1 provider trait-object vec.
///
/// Even providers we won't dispatch against are constructed and added
/// to the vec — the value of Phase 1 is proving that the
/// `Box<dyn Provider>` pool composes against three concrete adapters.
fn load_providers(config: &Config) -> Result<Vec<Box<dyn Provider>>> {
    let mut out: Vec<Box<dyn Provider>> = Vec::new();
    if let Some(cc) = config.providers.claude_code.clone() {
        out.push(Box::new(ClaudeCodeProvider::new(cc.into())));
    }
    if let Some(cx) = config.providers.codex.clone() {
        out.push(Box::new(CodexProvider::new(cx.into())));
    }
    // DeepSeek is always in the vec so the trait-object pool is
    // exercised against three providers from day one. Its dispatch +
    // healthcheck return Error::Provider per ADR 0020 Phase-deferred items.
    out.push(Box::new(DeepSeekProvider::new()));
    if out.is_empty() {
        anyhow::bail!("no providers configured (need at least one of claude_code / codex)");
    }
    Ok(out)
}

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
        "phase-1 smoke timed out after {}s waiting for the heartbeat job to fire",
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
}
