//! `evy-providers` — concrete `Provider` trait impls for the v4
//! launch set: Claude Code, OpenAI Codex, and a DeepSeek V4 Pro stub.
//!
//! Each adapter owns the provider-specific envelope shape (tmux
//! window + pasted text for Claude / Codex; nothing yet for DeepSeek)
//! and exposes the shared [`evy_core::Provider`] surface so callers can
//! hold a `Box<dyn Provider>` interchangeably.
//!
//! # Phase scope
//!
//! - **Claude Code** — fully wired against the v3 spawn pattern:
//!   `tmux new-window` + `command claude` launch + paste-via-buffer of
//!   a Markdown-formatted mandate directive. HMAC trust-marker wrapper
//!   from v3 is omitted and slated for Phase 2 (see
//!   [`claude_code::compose_directive`] rustdoc).
//! - **Codex** — same shape as Claude, with `CODEX_HOME` for
//!   per-account isolation and a trust-level override embedded in the
//!   launch line.
//! - **DeepSeek** — API-direct adapter (Phase 3 Slice F). Dispatch is
//!   a plain HTTPS `POST /v1/chat/completions` against DeepSeek's
//!   OpenAI-compatible endpoint; the model's response text is the
//!   worker's deliverable and lands in `<output_dir>/<worker_id>.md`.
//!   No tmux pane, no CLI launch. First exercise of the `Provider`
//!   trait's generality across worker shapes.
//!
//! # Lifetime
//!
//! Workers are tmux windows inside an operator-owned long-running
//! session. The session must already exist when `dispatch` is called —
//! Slice E's smoke test bears responsibility for spawning it. Worker
//! handles record `cancel_requested` so `status()` can disambiguate
//! between operator-cancelled and naturally-finished windows.

#![warn(missing_docs)]

pub mod claude_code;
pub mod codex;
pub mod config;
pub mod deepseek;
pub mod hmac;
pub mod oauth;
mod tmux;

pub use claude_code::{ClaudeCodeProvider, ClaudeCodeWorker};
pub use codex::{CodexProvider, CodexWorker};
pub use config::{ClaudeCodeConfig, CodexConfig};
pub use deepseek::{DeepSeekConfig, DeepSeekProvider, DeepSeekWorker};
pub use hmac::{HmacKey, TrustMarker};
// Cutover Phase 2 (2l) — Orch panel liveness + kill; (captures) — pane snapshot.
pub use oauth::{
    AccessToken, AccountRecord, AccountRow, AccountsStore, CodexOauth, DeviceCodeResponse,
    OauthError, OauthFlow, RefreshDedup, TokenRecord, XaiOauth,
};
// EA1 — WindowGuard promoted for the register-or-cleanup integration test.
pub use tmux::{tmux_capture, tmux_kill_session, tmux_session_alive, WindowGuard};

#[cfg(test)]
mod object_safety {
    //! Compile-time + runtime check that every Phase-1 provider fits
    //! behind `Box<dyn Provider>` so the worker pool in `evy-scheduler`
    //! can hold them interchangeably.

    use super::*;
    use evy_core::{PolicyMode, Provider};
    use std::path::PathBuf;

    #[test]
    fn all_phase1_providers_fit_in_a_trait_object_vec() {
        let claude_cfg = ClaudeCodeConfig {
            claude_config_dir: PathBuf::from("/tmp/cfg"),
            claude_bin: PathBuf::from("/tmp/.local/bin/claude"),
            tmux_session: "claude-test".to_string(),
            working_dir: PathBuf::from("/tmp"),
            policy_mode: PolicyMode::Gated,
            hmac_key: None,
        };
        let codex_cfg = CodexConfig {
            codex_home: PathBuf::from("/tmp/codex"),
            codex_bin: PathBuf::from("/tmp/codex-bin/codex"),
            tmux_session: "codex-test".to_string(),
            working_dir: PathBuf::from("/tmp"),
            model: None,
            policy_mode: PolicyMode::Gated,
            hmac_key: None,
        };
        let providers: Vec<Box<dyn Provider>> = vec![
            Box::new(ClaudeCodeProvider::new(claude_cfg)),
            Box::new(CodexProvider::new(codex_cfg)),
            Box::new(DeepSeekProvider::new()),
        ];
        let kinds: Vec<_> = providers.iter().map(|p| p.kind()).collect();
        assert_eq!(
            kinds,
            vec![
                evy_core::ProviderKind::ClaudeCode,
                evy_core::ProviderKind::Codex,
                evy_core::ProviderKind::DeepSeek,
            ]
        );
    }
}
