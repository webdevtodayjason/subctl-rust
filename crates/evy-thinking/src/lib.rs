//! `evy-thinking` — the thinking-partner conversational surface for
//! Evy v4 (primitive #6 in ADR 0020).
//!
//! Where worker-dispatch crates ([`evy-providers`], [`evy-scheduler`])
//! translate a [`Mandate`](evy_core::Mandate) into a running worker, this
//! crate gives Evy a separate LLM relationship dedicated to **thinking
//! about projects**: drafting plans, asking the hard clarifying questions
//! up-front, surfacing unknowns, iterating with the operator until the
//! plan is good enough to hand off to dispatch.
//!
//! # Why a separate LLM relationship
//!
//! ADR 0020 §6 calls this out explicitly: worker LLMs are dispatched
//! per-task with task-scoped context. The thinking-partner needs
//! continuity across many turns of one conversation, with cross-session
//! memory injected from the learning loop. Mixing the two surfaces would
//! either pollute worker context windows or strand planning continuity
//! when a worker finishes.
//!
//! # Crate boundaries
//!
//! * **Owns:** session lifecycle (start / send / conclude), the prompt
//!   templates, an [`LlmBackend`] trait, and an [`AnthropicBackend`]
//!   implementation against the Anthropic Messages API.
//! * **Does NOT own:** persistence. Sessions are held in-memory; the
//!   daemon-side composition layer wires the [`on_message`
//!   hook](ThinkingPartner::with_message_hook) into
//!   `evy-memory::ObservationLog` so each turn is appended to the
//!   learning-loop substrate.
//! * **Does NOT spawn workers.** A thinking session can produce a draft
//!   plan; converting that plan into a [`Mandate`](evy_core::Mandate) is
//!   an explicit operator action — never an implicit side-effect of the
//!   conversation. (ADR 0020 §6, "Critical scope discipline".)
//!
//! # Quick start
//!
//! ```no_run
//! use std::sync::Arc;
//! use evy_thinking::{AnthropicBackend, AnthropicConfig, ThinkingPartner};
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let backend = Arc::new(AnthropicBackend::new(AnthropicConfig::from_env()?));
//! let partner = ThinkingPartner::new(backend);
//!
//! let id = partner
//!     .start_session("brownfield migration for project X".to_string())
//!     .await?;
//! // Pull the partner's opening clarifying questions:
//! let session = partner.session(id).await?.expect("session exists");
//! let opening = session.messages.last().expect("partner asked questions");
//! println!("{}", opening.content);
//!
//! // Operator iterates:
//! let reply = partner
//!     .send(id, "Migration target is Postgres 16, no downtime budget.".into())
//!     .await?;
//! println!("{reply}");
//!
//! partner.conclude(id).await?;
//! # Ok(()) }
//! ```

#![warn(missing_docs)]

pub mod anthropic;
pub mod backend;
pub mod codex;
pub mod error;
pub mod lm_studio;
pub mod partner;
pub mod session;
pub mod templates;
pub mod tools;

// ── Public re-exports — the surface daemon-side code consumes ──────────

pub use anthropic::{
    AnthropicBackend, AnthropicConfig, DEFAULT_ANTHROPIC_API_BASE, DEFAULT_MAX_TOKENS,
    DEFAULT_MODEL, DEFAULT_TIMEOUT,
};
pub use backend::{LlmBackend, StreamChunk};
pub use codex::{
    CodexOauthBackend, CodexOauthConfig, DEFAULT_CODEX_ENDPOINT, DEFAULT_CODEX_MAX_TOKENS,
    DEFAULT_CODEX_MODEL, DEFAULT_CODEX_TIMEOUT,
};
pub use error::{Result, ThinkingError};
pub use lm_studio::{
    LmStudioBackend, LmStudioConfig, DEFAULT_LM_STUDIO_ENDPOINT,
    DEFAULT_MAX_TOKENS as DEFAULT_LM_STUDIO_MAX_TOKENS,
    DEFAULT_TEMPERATURE as DEFAULT_LM_STUDIO_TEMPERATURE,
    DEFAULT_TIMEOUT as DEFAULT_LM_STUDIO_TIMEOUT,
};
pub use partner::{MessageHook, ThinkingPartner};
pub use session::{Message, Role, Session, SessionId, SessionMode, SessionStatus};
pub use templates::{
    conclusion_user_turn, conversational_system_prompt, kickoff_user_turn, no_tools_brief,
    planning_system_prompt, status_header, tool_capability_brief,
};
pub use tools::{EvyTool, LiveStatusSource, ToolRegistry, ToolSpec, MAX_TOOL_ROUNDTRIPS};
