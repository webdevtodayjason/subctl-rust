//! Daemon configuration. Layered TOML + env override via figment.
//!
//! Source priority (highest wins):
//!
//! 1. Process environment, double-underscore-nested, `EVY_` prefix.
//!    (`EVY_SCHEDULER__DB_PATH=/tmp/foo.db` overrides `[scheduler].db_path`.)
//! 2. TOML file at the path passed to [`Config::load_from`] (the binary
//!    resolves this from `$SUBCTL_EVY_CONFIG` falling back to
//!    `config/evy.toml`).
//!
//! # Provider config translation
//!
//! The on-disk TOML keys are intentionally operator-friendly
//! (`config_dir`, `account`); the construction-time structs in
//! `evy-providers` use the underlying env-var names
//! (`claude_config_dir`, `codex_home`). We bridge with intermediate
//! structs that own a `From` impl into the real config. This was flagged
//! as friction point #2 in the Slice E report — Phase 2 could reconcile
//! by changing the provider crate's field names, but that's a public-API
//! break we're not ready to make at Slice E close.

use std::path::PathBuf;

use anyhow::{Context, Result};
use evy_core::PolicyMode;
use evy_providers::{ClaudeCodeConfig, CodexConfig};
use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};

/// Root daemon config.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Scheduler-side configuration (db path, etc.).
    pub scheduler: SchedulerConfig,
    /// Policy gate configuration (path to the resolved policy.toml).
    pub policy: PolicyConfig,
    /// Per-provider configuration. Both providers are optional; at
    /// least one must be set or [`crate::run_daemon`] will refuse to boot.
    pub providers: ProvidersConfig,
}

/// `[scheduler]` table.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchedulerConfig {
    /// sqlite db path the scheduler opens; migrations run automatically.
    pub db_path: PathBuf,
}

/// `[policy]` table.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PolicyConfig {
    /// Path to a single policy TOML file consumed by
    /// [`evy_policy::load_policy`].
    pub path: PathBuf,
}

/// `[providers]` table.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProvidersConfig {
    /// `[providers.claude_code]` — optional.
    #[serde(default)]
    pub claude_code: Option<ClaudeCodeConfigToml>,
    /// `[providers.codex]` — optional.
    #[serde(default)]
    pub codex: Option<CodexConfigToml>,
}

/// On-disk shape for `[providers.claude_code]`. Maps to
/// [`evy_providers::ClaudeCodeConfig`] via `From`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ClaudeCodeConfigToml {
    /// `CLAUDE_CONFIG_DIR` — picks the per-account auth + settings dir.
    pub config_dir: PathBuf,
    /// Detached tmux session that owns the worker windows.
    pub tmux_session: String,
    /// Working directory the `claude` CLI launches in.
    pub working_dir: PathBuf,
    /// Policy mode (`"Trusted" | "Gated" | "Sealed"`).
    pub policy_mode: PolicyMode,
}

impl From<ClaudeCodeConfigToml> for ClaudeCodeConfig {
    fn from(toml: ClaudeCodeConfigToml) -> Self {
        // `hmac_key` is intentionally `None` at TOML-deserialize time:
        // the daemon mints a per-session key at boot and attaches it
        // via `with_hmac_key` before the config is handed to the
        // provider constructor. Persisting the key in TOML would
        // contradict ADR 0011's secret-hygiene rule.
        Self {
            claude_config_dir: toml.config_dir,
            tmux_session: toml.tmux_session,
            working_dir: toml.working_dir,
            policy_mode: toml.policy_mode,
            hmac_key: None,
        }
    }
}

/// On-disk shape for `[providers.codex]`. Maps to
/// [`evy_providers::CodexConfig`] via `From`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CodexConfigToml {
    /// `CODEX_HOME` — picks the per-account `auth.json` + `config.toml`.
    pub codex_home: PathBuf,
    /// Detached tmux session that owns the worker windows.
    pub tmux_session: String,
    /// Working directory the `codex` CLI launches in.
    pub working_dir: PathBuf,
    /// Optional model override (e.g. `"gpt-5.5"`).
    #[serde(default)]
    pub model: Option<String>,
    /// Policy mode (`"Trusted" | "Gated" | "Sealed"`).
    pub policy_mode: PolicyMode,
}

impl From<CodexConfigToml> for CodexConfig {
    fn from(toml: CodexConfigToml) -> Self {
        // See ClaudeCodeConfigToml::from — `hmac_key` is attached at
        // daemon boot, never serialized.
        Self {
            codex_home: toml.codex_home,
            tmux_session: toml.tmux_session,
            working_dir: toml.working_dir,
            model: toml.model,
            policy_mode: toml.policy_mode,
            hmac_key: None,
        }
    }
}

impl Config {
    /// Resolve from the binary's default sources: `$SUBCTL_EVY_CONFIG`
    /// falling back to `./config/evy.toml`, plus env override.
    pub fn load() -> Result<Self> {
        let path = std::env::var("SUBCTL_EVY_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("config/evy.toml"));
        Self::load_from(&path)
    }

    /// Resolve from an explicit TOML path plus env override.
    ///
    /// `EVY_<SECTION>__<KEY>` env vars override their TOML siblings.
    pub fn load_from(path: &std::path::Path) -> Result<Self> {
        let fig = Figment::new()
            .merge(Toml::file(path))
            .merge(Env::prefixed("EVY_").split("__"));
        fig.extract()
            .with_context(|| format!("parsing evy config from {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_toml(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn full_config_parses() {
        let f = write_toml(
            r#"
[scheduler]
db_path = "/tmp/evy.db"

[policy]
path = "/tmp/policy.toml"

[providers.claude_code]
config_dir = "/tmp/claude"
tmux_session = "claude-smoke"
working_dir = "."
policy_mode = "Trusted"

[providers.codex]
codex_home = "/tmp/codex"
tmux_session = "codex-smoke"
working_dir = "."
policy_mode = "Trusted"
"#,
        );
        let cfg = Config::load_from(f.path()).expect("parse");
        assert_eq!(cfg.scheduler.db_path, PathBuf::from("/tmp/evy.db"));
        assert!(cfg.providers.claude_code.is_some());
        assert!(cfg.providers.codex.is_some());
    }

    #[test]
    fn missing_providers_section_is_ok() {
        let f = write_toml(
            r#"
[scheduler]
db_path = "/tmp/evy.db"

[policy]
path = "/tmp/policy.toml"

[providers]
"#,
        );
        let cfg = Config::load_from(f.path()).expect("parse");
        assert!(cfg.providers.claude_code.is_none());
        assert!(cfg.providers.codex.is_none());
    }

    #[test]
    fn into_claude_config_maps_fields() {
        let t = ClaudeCodeConfigToml {
            config_dir: PathBuf::from("/cfg"),
            tmux_session: "s".into(),
            working_dir: PathBuf::from("/w"),
            policy_mode: PolicyMode::Gated,
        };
        let c: ClaudeCodeConfig = t.into();
        assert_eq!(c.claude_config_dir, PathBuf::from("/cfg"));
        assert_eq!(c.tmux_session, "s");
    }

    #[test]
    fn into_codex_config_maps_fields() {
        let t = CodexConfigToml {
            codex_home: PathBuf::from("/h"),
            tmux_session: "s".into(),
            working_dir: PathBuf::from("/w"),
            model: Some("gpt-5.5".into()),
            policy_mode: PolicyMode::Sealed,
        };
        let c: CodexConfig = t.into();
        assert_eq!(c.codex_home, PathBuf::from("/h"));
        assert_eq!(c.model.as_deref(), Some("gpt-5.5"));
    }
}
