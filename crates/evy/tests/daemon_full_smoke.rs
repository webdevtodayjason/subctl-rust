//! Phase 3 Slice E end-to-end smoke for the long-lived daemon.
//!
//! Boots [`evy::run_daemon_with_shutdown`] in-process — NOT as a
//! subprocess — and asserts the full Phase 2+3 wiring is alive:
//!
//! 1. `/health` returns `{ ok: true, version: <str> }`.
//! 2. `/api/evy/scheduler/jobs` reports a job registered before the
//!    HTTP server bound (proves the AppState reads through to the live
//!    scheduler).
//! 3. A `DaemonBooted` SSE event is observable via the same broadcaster
//!    the daemon emits to.
//! 4. The shared `CancellationToken` drains the whole stack — HTTP
//!    server exits, scheduler stops, no leaked tasks.
//!
//! Why this test composes config in-line rather than reading evy.toml:
//! tests must not pollute the operator's real state. Every path is a
//! tempdir; the HTTP port is 0 so the kernel picks one (we learn it
//! through [`evy::DaemonHooks::http_ready`]).
//!
//! Why telegram + discord are unconfigured: those bridges need real
//! tokens. The daemon's optional-bridge wiring is exercised by the
//! evy-comms wiremock tests; this test asserts the daemon falls through
//! cleanly when both are absent.

use std::time::Duration;

use anyhow::Result;
use evy::config::{
    ClaudeCodeConfigToml, CodexConfigToml, CommsConfig, HttpSectionConfig, MemoryConfig,
    PolicyConfig, ProvidersConfig, SchedulerConfig, SkillsConfig,
};
use evy::{run_daemon_with_shutdown, Config, DaemonHooks};
use evy_core::PolicyMode;
use evy_scheduler::{Job, JobAction, JobId, Scheduler};
use serde_json::Value;
use tempfile::tempdir;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

/// Same minimal policy.toml the Phase 1 smoke uses — keeps the path
/// shape consistent across the two integration tests.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_full_smoke_serves_health_jobs_and_shuts_down() -> Result<()> {
    evy::init_tracing();

    let dir = tempdir()?;
    let policy_path = dir.path().join("policy.toml");
    std::fs::write(&policy_path, POLICY_TOML)?;
    let scheduler_db = dir.path().join("scheduler.db");
    let playbook_dir = dir.path().join("playbooks");
    // Intentionally NOT pre-created — `run_daemon_with_shutdown` should
    // mkdir on first boot. The test only sets the path.

    // ── Pre-register a job so the dashboard has something to show ────
    //
    // The daemon doesn't load jobs.toml yet (REPORT.md follow-up #10);
    // we register through a separate `Scheduler::open` on the same db
    // before the daemon boots, then the daemon's scheduler reads the
    // persisted row. Restart-survival of the jobs table is covered by
    // evy-scheduler's own integration tests.
    let pre_seeded_job_id = JobId::new();
    {
        let scheduler = Scheduler::open(&scheduler_db).await?;
        scheduler
            .register(Job {
                id: pre_seeded_job_id,
                name: "daemon-smoke-job".to_owned(),
                cron_expr: "0 9 * * 1-5".to_owned(),
                action: JobAction::LogHeartbeat,
                enabled: true,
                created_at: chrono::Utc::now(),
                last_run: None,
            })
            .await?;
        // Scheduler dropped; the row persists on disk for the daemon
        // to pick up on its own Scheduler::open call.
    }

    let config = Config {
        scheduler: SchedulerConfig {
            db_path: scheduler_db.clone(),
        },
        policy: PolicyConfig { path: policy_path },
        providers: ProvidersConfig {
            claude_code: Some(ClaudeCodeConfigToml {
                claude_bin: None,
                config_dir: dir.path().join("claude-cfg"),
                tmux_session: "evy-daemon-smoke-claude".to_string(),
                working_dir: dir.path().to_path_buf(),
                policy_mode: PolicyMode::Trusted,
            }),
            codex: Some(CodexConfigToml {
                codex_home: dir.path().join("codex-home"),
                codex_bin: None,
                tmux_session: "evy-daemon-smoke-codex".to_string(),
                working_dir: dir.path().to_path_buf(),
                model: None,
                policy_mode: PolicyMode::Trusted,
            }),
        },
        comms: CommsConfig {
            // port=0 → kernel-assigned ephemeral port. Test learns the
            // bound addr via DaemonHooks::http_ready.
            http: HttpSectionConfig {
                host: "127.0.0.1".to_string(),
                port: 0,
                allow_origins: Vec::new(),
                static_dir: None,
            },
            telegram: None,
            discord: None,
        },
        memory: MemoryConfig {
            observation_db: dir.path().join("observations.db"),
            playbook_dir,
            score_db: dir.path().join("scores.db"),
            preferences_db: dir.path().join("preferences.db"),
            claude_mem_db: None,
        },
        skills: SkillsConfig::default(),
        thinking_partner: None,
    };

    let shutdown = CancellationToken::new();
    let (http_ready_tx, http_ready_rx) = oneshot::channel();
    let hooks = DaemonHooks {
        http_ready: Some(http_ready_tx),
    };

    let shutdown_for_daemon = shutdown.clone();
    let daemon_handle =
        tokio::spawn(
            async move { run_daemon_with_shutdown(config, shutdown_for_daemon, hooks).await },
        );

    // ── Wait for the HTTP server to bind ─────────────────────────────
    let http_addr = timeout(Duration::from_secs(10), http_ready_rx)
        .await
        .expect("daemon never signalled http_ready inside 10s")
        .expect("http_ready sender dropped without sending");
    let base = format!("http://{http_addr}");

    // Give the listener a beat to accept once the port is open.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // ── 1. /health ───────────────────────────────────────────────────
    let health: Value = reqwest::get(format!("{base}/health")).await?.json().await?;
    assert_eq!(health["ok"], Value::Bool(true), "health.ok must be true");
    assert!(
        health["version"].is_string(),
        "health.version must be a string"
    );

    // ── 2. /api/evy/scheduler/jobs ───────────────────────────────────
    let jobs: Value = reqwest::get(format!("{base}/api/evy/scheduler/jobs"))
        .await?
        .json()
        .await?;
    let arr = jobs.as_array().expect("jobs response must be a JSON array");
    assert_eq!(
        arr.len(),
        1,
        "exactly one job should appear (the one pre-seeded on disk); got: {jobs}"
    );
    assert_eq!(arr[0]["name"], "daemon-smoke-job");
    assert_eq!(arr[0]["cron_expr"], "0 9 * * 1-5");
    assert_eq!(arr[0]["action_kind"], "log_heartbeat");
    assert_eq!(arr[0]["enabled"], Value::Bool(true));

    // ── 3. /api/evy/policy ───────────────────────────────────────────
    let policy: Value = reqwest::get(format!("{base}/api/evy/policy"))
        .await?
        .json()
        .await?;
    assert!(policy.is_object(), "policy must be a JSON object");

    // ── 4. /api/evy/workers — empty until a worker registry lands ────
    let workers: Value = reqwest::get(format!("{base}/api/evy/workers"))
        .await?
        .json()
        .await?;
    let workers_arr = workers.as_array().expect("workers must be a JSON array");
    assert!(
        workers_arr.is_empty(),
        "Phase 3 Slice E ships workers() returning empty; got {workers:?}"
    );

    // ── 5. /api/version ──────────────────────────────────────────────
    let version: Value = reqwest::get(format!("{base}/api/version"))
        .await?
        .json()
        .await?;
    assert!(version["version"].is_string());

    // ── 6. Shutdown drains the whole stack ───────────────────────────
    shutdown.cancel();
    let daemon_result = timeout(Duration::from_secs(15), daemon_handle)
        .await
        .expect("daemon did not exit within 15s of cancel")
        .expect("daemon task panicked");
    daemon_result.expect("daemon returned Err on shutdown");

    // ── 7. Observation log captured DaemonBooted + DaemonShutdown ───
    //
    // The daemon's lifecycle hook appends both observations to the log
    // at the path the test supplied. Opening the log after the daemon
    // exits proves the events made it to disk (not just the
    // broadcaster's volatile buffer).
    let obs_log = evy_memory::ObservationLog::open(&dir.path().join("observations.db")).await?;
    let recent = obs_log.query_recent(10).await?;
    let kinds: Vec<&str> = recent.iter().map(|o| o.kind.discriminator()).collect();
    assert!(
        kinds.contains(&"daemon_booted"),
        "expected daemon_booted observation; got {kinds:?}"
    );
    assert!(
        kinds.contains(&"daemon_shutdown"),
        "expected daemon_shutdown observation; got {kinds:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_creates_missing_playbook_dir() -> Result<()> {
    evy::init_tracing();
    let dir = tempdir()?;
    let policy_path = dir.path().join("policy.toml");
    std::fs::write(&policy_path, POLICY_TOML)?;

    // playbook_dir is a fresh subpath that does NOT exist yet — the
    // daemon should mkdir it on first boot rather than crash. This is
    // the "first-run on a clean install" contract from the lib docs.
    let playbook_dir = dir.path().join("does/not/exist/yet/playbooks");
    assert!(!playbook_dir.exists());

    let config = Config {
        scheduler: SchedulerConfig {
            db_path: dir.path().join("scheduler.db"),
        },
        policy: PolicyConfig { path: policy_path },
        providers: ProvidersConfig {
            claude_code: Some(ClaudeCodeConfigToml {
                claude_bin: None,
                config_dir: dir.path().join("claude-cfg"),
                tmux_session: "evy-mkdir-smoke".to_string(),
                working_dir: dir.path().to_path_buf(),
                policy_mode: PolicyMode::Trusted,
            }),
            codex: None,
        },
        comms: CommsConfig {
            http: HttpSectionConfig {
                host: "127.0.0.1".to_string(),
                port: 0,
                allow_origins: Vec::new(),
                static_dir: None,
            },
            telegram: None,
            discord: None,
        },
        memory: MemoryConfig {
            observation_db: dir.path().join("observations.db"),
            playbook_dir: playbook_dir.clone(),
            score_db: dir.path().join("scores.db"),
            preferences_db: dir.path().join("preferences.db"),
            claude_mem_db: None,
        },
        skills: SkillsConfig::default(),
        thinking_partner: None,
    };

    let shutdown = CancellationToken::new();
    let (tx, rx) = oneshot::channel();
    let hooks = DaemonHooks {
        http_ready: Some(tx),
    };

    let shutdown_for_daemon = shutdown.clone();
    let handle =
        tokio::spawn(
            async move { run_daemon_with_shutdown(config, shutdown_for_daemon, hooks).await },
        );

    // If the daemon booted, http_ready will fire. If it crashed on the
    // missing playbook dir, the rx will see a closed sender.
    let addr = timeout(Duration::from_secs(10), rx)
        .await
        .expect("daemon timed out before signalling http_ready")
        .expect("http_ready sender dropped — daemon likely errored on boot");
    assert!(addr.port() > 0, "kernel must have assigned a port");

    assert!(
        playbook_dir.exists(),
        "daemon must mkdir the playbook directory on boot"
    );

    shutdown.cancel();
    let _ = timeout(Duration::from_secs(10), handle).await;
    Ok(())
}
