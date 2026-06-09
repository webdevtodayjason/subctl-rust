//! End-to-end smoke test for the Phase 1 daemon wiring.
//!
//! Drives [`evy::run_daemon`] directly (not as a subprocess), since the
//! binary's body is already a thin wrapper around the library entry
//! point. Asserts:
//!
//! 1. `run_daemon` returns Ok within the cron-resolution budget.
//! 2. The reported job id matches the row in the `runs` table.
//! 3. Wall-clock fire latency is below 75 seconds (matches the
//!    scheduler crate's integration-test precedent — 5-field cron has
//!    minute granularity, so worst case is ~60s + scheduler latency).

use std::path::PathBuf;
use std::time::Duration;

use evy::config::{
    ClaudeCodeConfigToml, CodexConfigToml, CommsConfig, MemoryConfig, PolicyConfig,
    ProvidersConfig, SchedulerConfig, SkillsConfig,
};
use evy::{run_smoke_test, Config};
use evy_core::PolicyMode;
use tempfile::tempdir;

/// Minimal policy.toml the test writes into the tempdir. Matches the
/// `crates/evy-policy/tests/fixtures/install/config/policy/defaults.toml`
/// shape (the known-parsing reference).
const POLICY_TOML: &str = r#"
default_mode = "gated"
preset = "generic"

[mode.gated]

[mode.gated.allow]
commands = ["ls", "pwd", "echo"]

[mode.gated.deny_always]
substrings = ["rm -rf /"]
regex = []
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase1_smoke_test_runs_cron_job_and_exits_clean() -> anyhow::Result<()> {
    evy::init_tracing();

    let dir = tempdir()?;
    let db_path = dir.path().join("scheduler.db");
    let policy_path = dir.path().join("policy.toml");
    std::fs::write(&policy_path, POLICY_TOML)?;

    let config = Config {
        scheduler: SchedulerConfig {
            db_path: db_path.clone(),
            jobs_path: None,
        },
        policy: PolicyConfig {
            path: policy_path.clone(),
        },
        providers: ProvidersConfig {
            claude_code: Some(ClaudeCodeConfigToml {
                claude_bin: None,
                config_dir: dir.path().join("claude-cfg"),
                tmux_session: "evy-smoke-claude".to_string(),
                working_dir: dir.path().to_path_buf(),
                policy_mode: PolicyMode::Trusted,
            }),
            codex: Some(CodexConfigToml {
                codex_home: dir.path().join("codex-home"),
                codex_bin: None,
                tmux_session: "evy-smoke-codex".to_string(),
                working_dir: dir.path().to_path_buf(),
                model: None,
                policy_mode: PolicyMode::Trusted,
            }),
        },
        // Phase 3 Slice E added two config sections; the Phase 1 smoke
        // path doesn't exercise either (it never spins HTTP / memory),
        // so passing defaults keeps this test independent of the new
        // wiring.
        comms: CommsConfig::default(),
        memory: MemoryConfig::default(),
        skills: SkillsConfig::default(),
        thinking_partner: None,
    };

    let report = run_smoke_test(config).await?;

    // Friction-flagged contract: 5-field cron has minute granularity, so
    // we budget the same 75s the scheduler's own integration test does.
    assert!(
        report.fire_latency < Duration::from_secs(75),
        "fire latency {:?} should be inside the 75s budget",
        report.fire_latency,
    );
    assert!(
        report.total_wall_clock >= report.fire_latency,
        "total wall clock cannot be less than the fire latency",
    );

    // Re-open the scheduler post-shutdown and confirm the run persisted.
    // This proves restart-survival of the run row, not just the in-memory
    // state we observed during the smoke run.
    let scheduler = evy_scheduler::Scheduler::open(&db_path).await?;
    let runs = scheduler.list_runs(report.job_id).await?;
    assert!(
        !runs.is_empty(),
        "runs table should contain at least one row for the smoke job after the daemon exits",
    );
    let succeeded: Vec<_> = runs
        .iter()
        .filter(|r| matches!(r.outcome, evy_scheduler::RunOutcome::Succeeded))
        .collect();
    // Spec ("Assert exactly one row exists ... with outcome = Succeeded"): we
    // stop the scheduler the instant the first Succeeded run lands, so this
    // is "exactly one" by construction. Asserting it explicitly catches a
    // regression where the fire loop double-runs a job before stop drains.
    assert_eq!(
        succeeded.len(),
        1,
        "expected exactly one Succeeded run; got outcomes {:?}",
        runs.iter().map(|r| &r.outcome).collect::<Vec<_>>(),
    );

    // Drop the scheduler cleanly. (Open above did not start the fire loop.)
    drop(scheduler);
    // dir is dropped here; the tempdir cleanup removes the sqlite file.
    let _ = PathBuf::from(dir.path());
    Ok(())
}
