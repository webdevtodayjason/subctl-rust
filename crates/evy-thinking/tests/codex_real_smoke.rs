//! Real-Codex smoke test. Stays `#[ignore]`'d so CI never hits the
//! live `chatgpt.com/backend-api/codex` endpoint or consumes the
//! operator's ChatGPT Pro quota; run manually:
//!
//! ```bash
//! SUBCTL_CODEX_SMOKE_ALIAS=openai-jason \
//!     cargo test -p evy-thinking --test codex_real_smoke \
//!     -- --ignored --nocapture
//! ```
//!
//! The alias must already exist in `~/.config/subctl/accounts.conf`
//! with a freshly-minted `auth.json` (run `subctl auth codex` or copy
//! from a working v3 install). Asserts only that a single-turn
//! round-trip produces non-empty text.

use std::time::Duration;

use evy_thinking::{CodexOauthBackend, CodexOauthConfig, LlmBackend, Message, Role, SessionId};

fn live_backend() -> Option<CodexOauthBackend> {
    let alias = std::env::var("SUBCTL_CODEX_SMOKE_ALIAS").ok()?;
    if alias.trim().is_empty() {
        return None;
    }
    let mut cfg = CodexOauthConfig::new(alias);
    cfg.timeout = Duration::from_secs(120);
    // Smallish ceiling — don't burn quota proving the wire works.
    cfg.max_tokens = 512;
    CodexOauthBackend::new(cfg).ok()
}

#[tokio::test]
#[ignore = "real Codex API — run manually with SUBCTL_CODEX_SMOKE_ALIAS set to an alias from accounts.conf"]
async fn real_backend_single_turn_round_trip() {
    let backend =
        live_backend().expect("SUBCTL_CODEX_SMOKE_ALIAS must be set to a real accounts.conf alias");
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
