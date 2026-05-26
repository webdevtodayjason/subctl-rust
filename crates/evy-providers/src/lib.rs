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
//! - **DeepSeek** — stub per ADR 0020's Phase-deferred items. Every
//!   fallible method returns `Error::Provider { kind: DeepSeek, … }`
//!   with the expected reason; no tmux, no network.
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
mod tmux;

pub use claude_code::{ClaudeCodeProvider, ClaudeCodeWorker};
pub use codex::{CodexProvider, CodexWorker};
pub use config::{ClaudeCodeConfig, CodexConfig};
pub use deepseek::DeepSeekProvider;
pub use hmac::{HmacKey, TrustMarker};

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
            tmux_session: "claude-test".to_string(),
            working_dir: PathBuf::from("/tmp"),
            policy_mode: PolicyMode::Gated,
            hmac_key: None,
        };
        let codex_cfg = CodexConfig {
            codex_home: PathBuf::from("/tmp/codex"),
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
