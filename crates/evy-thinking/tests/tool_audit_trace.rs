//! EA1 — audit-line assertion for the agency tool registry, isolated
//! in its own test binary on purpose.
//!
//! Tracing caches per-callsite interest process-wide; when several
//! parallel tests hit the registry's audit callsite while only ONE of
//! them holds a thread-local `set_default` subscriber, the capture
//! races and intermittently sees nothing (observed deterministic under
//! `--test-threads` default, green under `--test-threads=1`). One
//! capture test per binary — the same shape `skill_autoload.rs` uses —
//! keeps the callsite exclusively owned and the capture deterministic.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use evy_thinking::{EvyTool, ToolRegistry, ToolSpec};
use serde_json::{json, Value};
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::fmt::MakeWriter;

struct FakeUsage;

#[async_trait]
impl EvyTool for FakeUsage {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "evy_usage".into(),
            description: "per-account usage summary".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    async fn execute(&self, _input: &Value) -> Result<String, String> {
        Ok("claude-a: 7d 46%".into())
    }
}

struct FakeBroken;

#[async_trait]
impl EvyTool for FakeBroken {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "evy_broken".into(),
            description: "always fails".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    async fn execute(&self, _input: &Value) -> Result<String, String> {
        Err("data source offline".into())
    }
}

#[derive(Clone)]
struct BufferedWriter(Arc<Mutex<Vec<u8>>>);

struct LockedBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LockedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for BufferedWriter {
    type Writer = LockedBuf;
    fn make_writer(&'a self) -> Self::Writer {
        LockedBuf(self.0.clone())
    }
}

fn install_capturing_subscriber() -> (Arc<Mutex<Vec<u8>>>, DefaultGuard) {
    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(BufferedWriter(buf.clone()))
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_target(true)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (buf, guard)
}

#[tokio::test]
async fn every_invocation_emits_one_audit_line_with_args_and_outcome() {
    let reg = ToolRegistry::new()
        .with_tool(Arc::new(FakeUsage))
        .with_tool(Arc::new(FakeBroken));

    let (buf, _guard) = install_capturing_subscriber();

    // Success, failure, and unknown-tool — each must audit.
    reg.execute("evy_usage", &json!({"window": "7d"}))
        .await
        .expect("ok");
    reg.execute("evy_broken", &json!({}))
        .await
        .expect_err("err");
    reg.execute("evy_ghost", &json!({}))
        .await
        .expect_err("unknown");

    let log = String::from_utf8(buf.lock().unwrap().clone()).expect("utf-8");
    assert_eq!(
        log.matches("evy_tool_audit").count(),
        3,
        "exactly one audit line per invocation; log:\n{log}"
    );
    assert!(
        log.contains("tool=evy_usage") && log.contains("outcome=\"ok\""),
        "success audit must carry tool + outcome=ok; log:\n{log}"
    );
    assert!(
        log.contains(r#"args={"window":"7d"}"#),
        "audit must carry the model-supplied args; log:\n{log}"
    );
    assert!(
        log.contains("tool=evy_broken")
            && log.contains("outcome=\"error\"")
            && log.contains("data source offline"),
        "failure audit must carry the error detail; log:\n{log}"
    );
    assert!(
        log.contains("tool=evy_ghost") && log.contains("unknown tool"),
        "unknown-tool audit must fire too; log:\n{log}"
    );
}
