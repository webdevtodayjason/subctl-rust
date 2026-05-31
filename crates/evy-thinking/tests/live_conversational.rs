//! Live smoke test against a running LM Studio — confirms the
//! *conversational* path produces an Evy-ish reply to a bare greeting
//! instead of the old planning-robot behaviour ("give me a topic / I
//! can't start Phase 1").
//!
//! Ignored by default; it needs LM Studio at `:1234` with a model loaded
//! **with a ≥8k context window** — the Evy persona prompt is ~7.2k tokens,
//! so a model JIT-loaded at the 4096 default overflows ("n_keep > n_ctx").
//! Load the model with context length 16384+ in LM Studio first.
//!
//! Run it with:
//!
//! ```text
//! cargo test -p evy-thinking --test live_conversational -- --ignored --nocapture
//! ```
//!
//! Uses `model: None` so LM Studio answers with whatever model is already
//! resident — it never forces an extra model load.

use std::sync::Arc;

use evy_thinking::lm_studio::{LmStudioBackend, LmStudioConfig};
use evy_thinking::{Role, ThinkingPartner};

#[tokio::test]
#[ignore = "requires a live LM Studio at :1234 with a model loaded"]
async fn hello_gets_evy_not_a_planning_robot() {
    let backend = Arc::new(LmStudioBackend::new(LmStudioConfig {
        // Pin the sanctioned cognee model; LM Studio JIT-loads it if it's
        // not already resident. Single endpoint, one small 9b — within
        // the local-AI consolidation rules.
        model: Some("qwen/qwen3.5-9b".to_string()),
        ..Default::default()
    }));

    assert!(
        backend.health().await.unwrap_or(false),
        "LM Studio not reachable at :1234 — enable Local Server + load a model"
    );

    let partner = ThinkingPartner::new(backend);
    let id = partner
        .start_conversation("hello".into())
        .await
        .expect("conversational open should succeed");

    let session = partner.session(id).await.unwrap().expect("session exists");
    let reply = session
        .last_of(Role::Partner)
        .expect("Evy replied")
        .content
        .clone();

    eprintln!("\n=== Evy replied to \"hello\" ===\n{reply}\n=== end ===\n");

    assert!(!reply.trim().is_empty(), "Evy must actually say something");

    // The robotic failure mode we're fixing: a bare greeting being met
    // with a demand for a topic / a PHASE 1 planning interrogation.
    let lower = reply.to_lowercase();
    for robotic in ["phase 1", "provide a topic", "give me a topic"] {
        assert!(
            !lower.contains(robotic),
            "reply still sounds like the planning robot (contains {robotic:?}):\n{reply}"
        );
    }
}
