//! Provider configuration types.
//!
//! Each adapter has its own `*Config` struct carrying the bits the
//! adapter needs at construction time (config dir, tmux session, cwd,
//! policy mode). The runtime mandate (goal/context/etc.) arrives later
//! via `dispatch(&Mandate)`.
//!
//! Configs are plain data — `#[derive(Debug, Clone)]` — so a single
//! daemon can hold multiple `ClaudeCodeProvider` instances (one per
//! pinned account) without trait-object juggling.
//!
//! # HMAC trust marker
//!
//! Each provider config carries an optional [`HmacKey`]. When `Some`,
//! `dispatch` wraps the directive body in the ADR-0011 trust marker
//! envelope before pasting via tmux. When `None`, dispatch falls back to
//! [`crate::hmac::default_key`] — a process-global key minted on first
//! access. The daemon binary supplies a fresh per-session key at boot;
//! the fallback exists so unit tests don't have to construct a key.

use std::path::PathBuf;

use evy_core::PolicyMode;

use crate::hmac::HmacKey;

/// Construction-time configuration for [`crate::ClaudeCodeProvider`].
///
/// Mirrors the v3 `providers/claude/teams.sh` per-account inputs. The
/// `claude_config_dir` is the per-account isolation dir Claude Code
/// reads via `CLAUDE_CONFIG_DIR`; `tmux_session` is the long-running
/// detached session every worker window lives inside.
#[derive(Debug, Clone)]
pub struct ClaudeCodeConfig {
    /// Per-account `CLAUDE_CONFIG_DIR` — picks the auth + settings the
    /// spawned `claude` CLI sees.
    pub claude_config_dir: PathBuf,
    /// Absolute path to the `claude` binary the worker launches. Using an
    /// absolute path instead of `command claude` keeps spawning
    /// independent of the tmux/launchd PATH (which differs from the
    /// operator's interactive shell — the root of the v3 install/PATH
    /// split-brain). Built from the daemon config; defaults to
    /// `~/.local/bin/claude` (the native install).
    pub claude_bin: PathBuf,
    /// Detached tmux session name that owns the worker windows.
    /// Convention: `claude-<basename(cwd)>`. Must already exist when
    /// `dispatch` is called.
    pub tmux_session: String,
    /// Working directory the `claude` CLI launches in.
    pub working_dir: PathBuf,
    /// Policy mode propagated into the directive header so the worker
    /// knows its autonomy ceiling. (The hard policy gate lives in
    /// `evy-policy`; this is just informational metadata for Phase 1.)
    pub policy_mode: PolicyMode,
    /// Per-session HMAC key for ADR-0011 trust-marker authentication.
    /// `None` falls back to [`crate::hmac::default_key`] — see module
    /// rustdoc for the rationale.
    pub hmac_key: Option<HmacKey>,
}

/// Construction-time configuration for [`crate::CodexProvider`].
///
/// Mirrors the v3 `providers/openai-codex/teams.sh` per-account inputs.
/// `codex_home` is the `CODEX_HOME` env var — Codex's analog of
/// `CLAUDE_CONFIG_DIR` for per-account isolation. `model` is an
/// optional override; when `None`, Codex picks per `config.toml`.
#[derive(Debug, Clone)]
pub struct CodexConfig {
    /// Per-account `CODEX_HOME` — Codex CLI reads `auth.json` +
    /// `config.toml` from here.
    pub codex_home: PathBuf,
    /// Detached tmux session that owns the worker windows. Convention:
    /// `codex-<basename(cwd)>`. Must already exist when `dispatch` is
    /// called.
    pub tmux_session: String,
    /// Working directory the `codex` CLI launches in.
    pub working_dir: PathBuf,
    /// Optional model override (e.g. `"gpt-5.5"`). When `None`, Codex
    /// picks per `~/.codex/config.toml`.
    pub model: Option<String>,
    /// Policy mode propagated into the directive header so the worker
    /// knows its autonomy ceiling. Codex's hard sandbox lives in the
    /// CLI flags; this is the additive learning-loop signal.
    pub policy_mode: PolicyMode,
    /// Per-session HMAC key for ADR-0011 trust-marker authentication.
    /// `None` falls back to [`crate::hmac::default_key`] — see module
    /// rustdoc for the rationale.
    pub hmac_key: Option<HmacKey>,
}
