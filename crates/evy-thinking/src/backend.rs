//! [`LlmBackend`] trait — the abstract surface the partner consumes.
//!
//! Concrete implementations live in sibling modules ([`crate::anthropic`]
//! today, future `LocalDgxBackend` planned for Phase 4). Callers should
//! hold an `Arc<dyn LlmBackend>` so the backend can be swapped in tests
//! without touching call sites.

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::Result;
use crate::session::Message;

/// One in-flight increment from a streaming partner turn.
///
/// Emitted by [`LlmBackend::stream_respond`] (or the partner's
/// `stream_send` / `stream_start_session`) before the final assembled
/// text is returned. The HTTP `text/event-stream` branch of
/// `POST /api/evy/chat` translates each chunk into an SSE `data:` frame
/// for the operator's TUI to render in real time.
///
/// The variant set is intentionally narrow — additions are wire-shape
/// changes and need TUI coordination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamChunk {
    /// Next slice of assistant text, suitable for incremental rendering.
    /// Concatenating every `Token` in order yields the full reply.
    Token(String),
    /// The model autoloaded a skill from the registry. Emitted by
    /// backends that drive Hermes-style `skill_view` tool round-trips
    /// (Anthropic). Other backends never emit this variant.
    SkillLoaded(String),
}

/// One-shot text generation from a structured planning context.
///
/// Implementations translate the supplied `system_prompt` plus
/// `messages` into the backend's native envelope and return the
/// partner's next reply as a plain string. Streaming, tool-use, and
/// multi-modal generation are out of scope for v0.5.0 — see the
/// `// TODO: Phase 4` markers in [`crate::anthropic`].
///
/// # Conventions
///
/// * `system_prompt` is rendered separately from `messages`. Backends
///   that don't support a system slot natively (none of the ones we
///   target) should still accept the argument and prepend it.
/// * `messages` is the chronological message log. Implementations
///   **must** skip [`crate::Role::System`] entries — those are
///   surface-side scaffolding, not LLM input. Operator → user;
///   Partner → assistant.
/// * The trait is `Send + Sync` so implementations can be shared across
///   tokio tasks (`Arc<dyn LlmBackend>` is the typical handle).
#[async_trait]
pub trait LlmBackend: Send + Sync {
    /// Per-backend capability truth, appended to the system prompt by
    /// [`crate::ThinkingPartner`] each turn.
    ///
    /// `Some(brief)` — this backend will advertise tools on the wire
    /// this turn (return [`crate::templates::tool_capability_brief`]).
    /// `None` (the default) — no tools reach the wire; the partner
    /// appends [`crate::templates::no_tools_brief`] instead, so the
    /// model never claims abilities its backend doesn't carry.
    fn capability_brief(&self) -> Option<String> {
        None
    }

    /// Produce the next partner turn given the planning context.
    ///
    /// # Errors
    /// - [`crate::ThinkingError::Transport`] on a network failure.
    /// - [`crate::ThinkingError::HttpStatus`] on a non-2xx response.
    /// - [`crate::ThinkingError::Decode`] when the response body does
    ///   not match the expected schema.
    /// - [`crate::ThinkingError::BackendRefused`] when the wire shape is
    ///   valid but content is unusable (empty `content` array, no
    ///   `text` block).
    /// - [`crate::ThinkingError::Config`] when required credentials are
    ///   absent.
    async fn respond(&self, system_prompt: &str, messages: &[Message]) -> Result<String>;

    /// Produce the next partner turn, emitting [`StreamChunk`]s
    /// incrementally over `sink` and returning the final assembled
    /// text.
    ///
    /// # Default implementation
    ///
    /// The default impl calls [`respond`](Self::respond), emits the
    /// whole reply as a single `Token` chunk, and returns. Backends
    /// whose upstream API natively supports streaming should override
    /// this to forward tokens as they arrive — see
    /// [`crate::lm_studio::LmStudioBackend`] for the OpenAI-compat
    /// streaming pattern.
    ///
    /// Send failures on `sink` (the SSE client disconnected) are
    /// treated as a quiet completion: the backend stops emitting and
    /// still returns the assembled text. The handler then either
    /// records or discards it based on the partner-side bookkeeping.
    ///
    /// # Errors
    /// Same as [`respond`](Self::respond).
    async fn stream_respond(
        &self,
        system_prompt: &str,
        messages: &[Message],
        sink: &mpsc::Sender<StreamChunk>,
    ) -> Result<String> {
        let text = self.respond(system_prompt, messages).await?;
        // Best-effort emit — if the client disconnected we still want
        // the partner to receive the full text so the session stays
        // consistent. The handler-side bookkeeping decides what to do
        // with the orphan.
        let _ = sink.send(StreamChunk::Token(text.clone())).await;
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Compile-time assertion that the trait stays dyn-compatible. The
    // partner stores `Arc<dyn LlmBackend>`; if the trait ever stops
    // being object-safe this file fails to type-check.
    #[allow(dead_code)]
    fn assert_backend_object_safe(b: Box<dyn LlmBackend>) -> Box<dyn LlmBackend> {
        b
    }
}
