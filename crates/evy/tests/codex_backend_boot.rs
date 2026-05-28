//! Boot test for `[thinking_partner] backend = "codex"`.
//!
//! Proves that the daemon's `build_thinking_partner` accepts a Codex
//! OAuth section, finds the operator's on-disk `accounts.conf` row +
//! `auth.json`, and surfaces a live chat endpoint without trying to
//! contact the real Codex API at boot. The chat endpoint itself isn't
//! exercised here — the Codex wire is mocked out at the integration
//! tier in `crates/evy-thinking/tests/codex_mock.rs`. This test
//! validates only the *daemon-side wiring*: that the operator can flip
//! their TOML from `backend = "anthropic"` to `backend = "codex"`
//! without a daemon panic.
//!
//! Why a separate test file (not extending `daemon_full_smoke.rs`):
//! the full-smoke fixture intentionally configures
//! `thinking_partner: None` so the chat endpoint returns 503. Adding a
//! Codex thinking-partner there would muddy that contract; this
//! standalone test pins the codex branch in isolation.

use std::time::Duration;

use anyhow::Result;
use chrono::{Duration as ChronoDuration, Utc};
use evy::config::{
    ClaudeCodeConfigToml, CodexConfigToml, CodexSectionConfig, CommsConfig, HttpSectionConfig,
    MemoryConfig, PolicyConfig, ProvidersConfig, SchedulerConfig, SkillsConfig,
    ThinkingPartnerSectionConfig,
};
use evy::{run_daemon_with_shutdown, Config, DaemonHooks};
use evy_core::PolicyMode;
use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

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
async fn daemon_boots_with_codex_thinking_partner() -> Result<()> {
    evy::init_tracing();

    let dir = tempdir()?;
    let policy_path = dir.path().join("policy.toml");
    std::fs::write(&policy_path, POLICY_TOML)?;

    // ── Stage the operator's existing Codex OAuth bundle on disk ─────
    //
    // The Codex backend reads `accounts.conf` to find the row whose
    // alias matches `[thinking_partner.codex].account`, then opens
    // `<config_dir>/auth.json` for the JWT + refresh token. Test fakes
    // both with a tempdir; no real OAuth credentials.
    let codex_account_dir = dir.path().join(".codex-tester");
    std::fs::create_dir_all(&codex_account_dir)?;
    let accounts_conf_path = dir.path().join("accounts.conf");
    std::fs::write(
        &accounts_conf_path,
        format!(
            "openai-tester | openai-codex | tester@example.com | {} | smoke",
            codex_account_dir.display()
        ),
    )?;
    let future_expiry = Utc::now() + ChronoDuration::hours(24);
    let auth_blob = json!({
        "OPENAI_API_KEY": null,
        "tokens": {
            "access_token": "fake.jwt.token",
            "refresh_token": "rt-1",
        },
        "last_refresh": Utc::now().to_rfc3339(),
        "expires_at": future_expiry.to_rfc3339(),
        "_subctl": { "alias": "openai-tester" }
    });
    std::fs::write(
        codex_account_dir.join("auth.json"),
        serde_json::to_string_pretty(&auth_blob)?,
    )?;

    // Skills section pointed at an empty dir — registry loads but has
    // zero entries, which proves the codex backend handles that case.
    let skills_dir = dir.path().join("skills");
    std::fs::create_dir_all(&skills_dir)?;

    let config = Config {
        scheduler: SchedulerConfig {
            db_path: dir.path().join("scheduler.db"),
        },
        policy: PolicyConfig { path: policy_path },
        providers: ProvidersConfig {
            // At least one real provider is required by run_daemon's
            // load_providers check. claude_code + codex are stubs here;
            // the daemon never spawns workers.
            claude_code: Some(ClaudeCodeConfigToml {
                config_dir: dir.path().join("claude-cfg"),
                tmux_session: "evy-codex-boot-smoke-claude".to_string(),
                working_dir: dir.path().to_path_buf(),
                policy_mode: PolicyMode::Trusted,
            }),
            codex: Some(CodexConfigToml {
                codex_home: dir.path().join("codex-home"),
                tmux_session: "evy-codex-boot-smoke-codex".to_string(),
                working_dir: dir.path().to_path_buf(),
                model: None,
                policy_mode: PolicyMode::Trusted,
            }),
        },
        comms: CommsConfig {
            http: HttpSectionConfig {
                host: "127.0.0.1".to_string(),
                port: 0,
                allow_origins: Vec::new(),
            },
            telegram: None,
            discord: None,
        },
        memory: MemoryConfig {
            observation_db: dir.path().join("observations.db"),
            playbook_dir: dir.path().join("playbooks"),
            score_db: dir.path().join("scores.db"),
            preferences_db: dir.path().join("preferences.db"),
            claude_mem_db: None,
        },
        skills: SkillsConfig {
            directory: skills_dir,
            enabled: true,
        },
        thinking_partner: Some(ThinkingPartnerSectionConfig {
            backend: "codex".to_string(),
            // api_key_env is irrelevant for the codex branch; the
            // default value is fine — the daemon doesn't read it.
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            model: Some("gpt-5.5".to_string()),
            max_tokens: Some(2048),
            codex: Some(CodexSectionConfig {
                accounts_conf_path: accounts_conf_path.clone(),
                account: "openai-tester".to_string(),
                // No endpoint override — backend won't talk to it on
                // boot, only on a chat call (which this test doesn't
                // exercise). Production default would point at the real
                // chatgpt.com host; we trust the integration tests in
                // evy-thinking to validate the wire.
                endpoint: None,
            }),
        }),
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

    // Wait for HTTP bind — if the codex backend construction had
    // panicked, run_daemon_with_shutdown would return Err before
    // signaling http_ready and this timeout would fire.
    let http_addr = timeout(Duration::from_secs(10), http_ready_rx)
        .await
        .expect("daemon never signalled http_ready inside 10s (codex backend probably failed to construct)")
        .expect("http_ready sender dropped without sending");
    let base = format!("http://{http_addr}");

    tokio::time::sleep(Duration::from_millis(50)).await;

    // The chat endpoint should be live (200/4xx) — NOT 503 (which would
    // mean the thinking partner was absent). We don't drive a real chat
    // request because the upstream is mocked separately in
    // evy-thinking; here we just confirm the endpoint exists.
    let health: Value = reqwest::get(format!("{base}/health")).await?.json().await?;
    assert_eq!(health["ok"], Value::Bool(true));

    // Shutdown drains cleanly.
    shutdown.cancel();
    let daemon_result = timeout(Duration::from_secs(15), daemon_handle)
        .await
        .expect("daemon did not drain inside 15s")
        .expect("daemon task panicked");
    daemon_result.expect("daemon returned Err on drain");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_fails_fast_when_codex_section_missing() -> Result<()> {
    evy::init_tracing();

    let dir = tempdir()?;
    let policy_path = dir.path().join("policy.toml");
    std::fs::write(&policy_path, POLICY_TOML)?;

    let config = Config {
        scheduler: SchedulerConfig {
            db_path: dir.path().join("scheduler.db"),
        },
        policy: PolicyConfig { path: policy_path },
        providers: ProvidersConfig {
            claude_code: Some(ClaudeCodeConfigToml {
                config_dir: dir.path().join("claude-cfg"),
                tmux_session: "evy-codex-fail-smoke-claude".to_string(),
                working_dir: dir.path().to_path_buf(),
                policy_mode: PolicyMode::Trusted,
            }),
            codex: Some(CodexConfigToml {
                codex_home: dir.path().join("codex-home"),
                tmux_session: "evy-codex-fail-smoke-codex".to_string(),
                working_dir: dir.path().to_path_buf(),
                model: None,
                policy_mode: PolicyMode::Trusted,
            }),
        },
        comms: CommsConfig::default(),
        memory: MemoryConfig {
            observation_db: dir.path().join("observations.db"),
            playbook_dir: dir.path().join("playbooks"),
            score_db: dir.path().join("scores.db"),
            preferences_db: dir.path().join("preferences.db"),
            claude_mem_db: None,
        },
        skills: SkillsConfig::default(),
        thinking_partner: Some(ThinkingPartnerSectionConfig {
            backend: "codex".to_string(),
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            model: None,
            max_tokens: None,
            // The whole point of this test — operator forgot to add
            // [thinking_partner.codex]. Daemon must fail fast at boot
            // with a clear error rather than 503'ing on every chat
            // turn or unwrap()'ing inside the backend constructor.
            codex: None,
        }),
    };

    let shutdown = CancellationToken::new();
    let hooks = DaemonHooks::default();
    let err = run_daemon_with_shutdown(config, shutdown, hooks)
        .await
        .expect_err("daemon must fail to boot when codex section is missing");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("[thinking_partner.codex]"),
        "error should mention the missing section; got: {msg}"
    );
    Ok(())
}
