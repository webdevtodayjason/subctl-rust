//! Real-Anthropic smoke test. Stays `#[ignore]`'d so CI never hits the
//! live endpoint or burns the operator's API budget; run manually with
//! credentials:
//!
//! ```bash
//! ANTHROPIC_API_KEY=<key> cargo test -p evy-thinking \
//!     --test anthropic_real_smoke -- --ignored --nocapture
//! ```
//!
//! Asserts only that a single-turn round-trip produces non-empty text.
//! Content varies day-to-day so any deeper check would be flaky.

use std::sync::Arc;
use std::time::Duration;

use evy_thinking::{
    AnthropicBackend, AnthropicConfig, LlmBackend, Message, Role, Session, SessionId,
    ThinkingPartner,
};

fn live_backend() -> Option<AnthropicBackend> {
    let key = std::env::var("ANTHROPIC_API_KEY").ok()?;
    if key.trim().is_empty() {
        return None;
    }
    let mut cfg = AnthropicConfig::new(key);
    cfg.timeout = Duration::from_secs(120);
    // Keep `max_tokens` smallish for the smoke test — we don't want to
    // burn budget proving the wire works.
    cfg.max_tokens = 512;
    Some(AnthropicBackend::new(cfg))
}

#[tokio::test]
#[ignore = "real Anthropic API — run manually with ANTHROPIC_API_KEY set"]
async fn real_backend_single_turn_round_trip() {
    let backend = live_backend().expect("ANTHROPIC_API_KEY must be set for this test");
    let sid = SessionId::new();
    let msgs = vec![Message::new(
        sid,
        Role::Operator,
        "Say the word 'pong' and nothing else.",
    )];
    let reply = backend
        .respond("You are a test fixture. Reply tersely.", &msgs)
        .await
        .expect("real call must succeed");
    assert!(!reply.trim().is_empty(), "expected non-empty reply");
}

#[tokio::test]
#[ignore = "real Anthropic API — run manually with ANTHROPIC_API_KEY set"]
async fn real_partner_opens_session_with_clarifying_questions() {
    let backend = Arc::new(live_backend().expect("ANTHROPIC_API_KEY must be set for this test"));
    let partner = ThinkingPartner::new(backend);
    let id = partner
        .start_session("setting up a small homelab backup strategy".into())
        .await
        .expect("start ok");
    let session: Session = partner.session(id).await.unwrap().expect("session");
    let opening = session.last_of(Role::Partner).expect("opening turn");
    assert!(
        !opening.content.trim().is_empty(),
        "partner produced an opening turn"
    );
    // We can't assert the LLM literally numbered the questions — it
    // sometimes uses bullets, sometimes prose. The smoke test only
    // proves the wire round-trips.
}
