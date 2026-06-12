//! Backend-neutral tool registry — the EA1 agency surface.
//!
//! [`ToolRegistry`] holds the set of tools Evy may call during a chat
//! turn. The registry is *backend-neutral*: it owns names, schemas, and
//! executors, while each [`crate::LlmBackend`] impl owns the wire
//! rendering (Anthropic `tools[]` + `tool_use`/`tool_result` blocks;
//! OpenAI-compat `tools[]` + `tool_calls`/`role:"tool"` messages).
//!
//! # Audit
//!
//! Every invocation — read-only or action — emits exactly one
//! `tracing::info!(target: "evy_tool_audit", ...)` line carrying the
//! tool name, the model-supplied arguments, and the outcome (`ok` /
//! `error`). [`ToolRegistry::execute`] is the single funnel; backends
//! MUST dispatch through it rather than calling
//! [`EvyTool::execute`] directly so no invocation can skip the audit.
//!
//! # Why executors live outside this crate
//!
//! The executors need daemon-side handles (worker registry, tmux,
//! accounts.conf, telegram) that evy-thinking deliberately does not
//! depend on. The daemon (`crates/evy`) constructs the concrete tools
//! and hands the registry down at backend construction time — the same
//! direction skills already flow.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tracing::{info, warn};

/// Upper bound on consecutive tool round-trips per `respond()` call,
/// shared by every backend that drives a tool loop. A well-behaved
/// model calls 1-3 tools then produces text; anything past this cap is
/// a pathological loop and surfaces as
/// [`crate::ThinkingError::BackendRefused`].
pub const MAX_TOOL_ROUNDTRIPS: usize = 5;

/// Static description of one tool — what gets advertised on the wire.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    /// Wire name the model invokes (e.g. `evy_usage`).
    pub name: String,
    /// One-paragraph description shown to the model. Say *when* to call
    /// it, not just what it does.
    pub description: String,
    /// JSON Schema for the tool input (`{"type": "object", ...}`).
    pub input_schema: Value,
}

/// One executable tool. Implementations live in the daemon
/// (`crates/evy/src/agency.rs`) where the data-source handles are.
///
/// `execute` returns `Ok(text)` rendered into the tool result, or
/// `Err(message)` rendered as an error result the model can recover
/// from (try another tool, or answer without one). Implementations
/// must never panic on malformed input — the input comes from a model.
#[async_trait]
pub trait EvyTool: Send + Sync {
    /// The spec advertised to the model.
    fn spec(&self) -> ToolSpec;

    /// Run the tool against the supplied model-provided input.
    async fn execute(&self, input: &Value) -> std::result::Result<String, String>;
}

/// The set of tools advertised for a chat turn. Cheap to clone behind
/// an `Arc`; construction happens once at daemon boot.
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn EvyTool>>,
}

impl ToolRegistry {
    /// An empty registry — backends treat it the same as no registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tool. Builder-style so daemon boot reads top-down.
    #[must_use]
    pub fn with_tool(mut self, tool: Arc<dyn EvyTool>) -> Self {
        self.tools.push(tool);
        self
    }

    /// Number of registered tools.
    #[must_use]
    pub fn count(&self) -> usize {
        self.tools.len()
    }

    /// Specs for every registered tool, in registration order.
    #[must_use]
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    /// Tool names, for the system-prompt capability brief.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.spec().name).collect()
    }

    /// Whether `name` is a registered tool.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.spec().name == name)
    }

    /// Execute `name` with `input`, emitting the audit line. This is
    /// the ONLY dispatch path backends may use — it guarantees one
    /// `evy_tool_audit` record per invocation, success or failure.
    ///
    /// # Errors
    /// `Err(message)` when the tool is unknown or its executor failed;
    /// the caller renders it as an error tool-result so the model can
    /// recover.
    pub async fn execute(&self, name: &str, input: &Value) -> std::result::Result<String, String> {
        let Some(tool) = self.tools.iter().find(|t| t.spec().name == name) else {
            warn!(
                target: "evy_tool_audit",
                tool = %name,
                args = %compact_args(input),
                outcome = "error",
                detail = "unknown tool",
                "evy tool invocation",
            );
            return Err(format!("unknown tool `{name}`"));
        };
        let result = tool.execute(input).await;
        match &result {
            Ok(out) => info!(
                target: "evy_tool_audit",
                tool = %name,
                args = %compact_args(input),
                outcome = "ok",
                result_chars = out.len(),
                "evy tool invocation",
            ),
            Err(e) => warn!(
                target: "evy_tool_audit",
                tool = %name,
                args = %compact_args(input),
                outcome = "error",
                detail = %e,
                "evy tool invocation",
            ),
        }
        result
    }
}

/// Render tool args for the audit line, capped so a model that stuffs a
/// novel into an argument can't bloat the log.
fn compact_args(input: &Value) -> String {
    let s = input.to_string();
    if s.len() > 400 {
        let truncated: String = s.chars().take(400).collect();
        format!("{truncated}…")
    } else {
        s
    }
}

/// Live fleet status injected into the system prompt at compose time.
///
/// This is the *no-tools* path to self-knowledge: backends whose models
/// can't reliably drive a tool loop (gemma-class on LM Studio, see the
/// `lm_studio` module docs) still answer fleet questions because the
/// partner injects this block into EVERY backend's system prompt each
/// turn. Implementations live in the daemon and should cache with a
/// short TTL so chat turns don't hammer tmux.
#[async_trait]
pub trait LiveStatusSource: Send + Sync {
    /// A compact, pre-rendered status block (accounts, usage, sessions,
    /// workers, watchdogs), or `None` when nothing is available. Keep
    /// it small — it ships on every turn.
    async fn status_block(&self) -> Option<String>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Echo;
    #[async_trait]
    impl EvyTool for Echo {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".into(),
                description: "echo the input".into(),
                input_schema: json!({"type": "object"}),
            }
        }
        async fn execute(&self, input: &Value) -> std::result::Result<String, String> {
            Ok(input.to_string())
        }
    }

    struct Fails;
    #[async_trait]
    impl EvyTool for Fails {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "fails".into(),
                description: "always errors".into(),
                input_schema: json!({"type": "object"}),
            }
        }
        async fn execute(&self, _input: &Value) -> std::result::Result<String, String> {
            Err("boom".into())
        }
    }

    #[tokio::test]
    async fn registry_dispatches_by_name() {
        let reg = ToolRegistry::new()
            .with_tool(Arc::new(Echo))
            .with_tool(Arc::new(Fails));
        assert_eq!(reg.count(), 2);
        assert!(reg.contains("echo"));
        assert!(!reg.contains("ghost"));
        let out = reg.execute("echo", &json!({"x": 1})).await.unwrap();
        assert_eq!(out, r#"{"x":1}"#);
    }

    #[tokio::test]
    async fn registry_surfaces_executor_error() {
        let reg = ToolRegistry::new().with_tool(Arc::new(Fails));
        let err = reg.execute("fails", &json!({})).await.unwrap_err();
        assert_eq!(err, "boom");
    }

    #[tokio::test]
    async fn registry_rejects_unknown_tool() {
        let reg = ToolRegistry::new();
        let err = reg.execute("nope", &json!({})).await.unwrap_err();
        assert!(err.contains("unknown tool"));
    }

    #[test]
    fn compact_args_caps_long_input() {
        let long = json!({"goal": "x".repeat(1000)});
        let s = compact_args(&long);
        assert!(s.len() <= 405, "got {}", s.len());
        assert!(s.ends_with('…'));
    }

    #[test]
    fn specs_and_names_follow_registration_order() {
        let reg = ToolRegistry::new()
            .with_tool(Arc::new(Echo))
            .with_tool(Arc::new(Fails));
        assert_eq!(reg.names(), vec!["echo", "fails"]);
        assert_eq!(reg.specs().len(), 2);
    }
}
