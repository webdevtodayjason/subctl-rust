//! [`LlmBackend`] trait — the abstract surface the partner consumes.
//!
//! Concrete implementations live in sibling modules ([`crate::anthropic`]
//! today, future `LocalDgxBackend` planned for Phase 4). Callers should
//! hold an `Arc<dyn LlmBackend>` so the backend can be swapped in tests
//! without touching call sites.

use async_trait::async_trait;

use crate::error::Result;
use crate::session::Message;

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
