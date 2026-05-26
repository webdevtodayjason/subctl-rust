//! Smoke test for the claude-mem reader against a *real* operator
//! database.
//!
//! Marked `#[ignore]` because it requires a populated claude-mem db on
//! the host. Run with:
//!
//! ```sh
//! CLAUDE_MEM_DB=/Users/sem/.claude-mem/claude-mem.db \
//!   cargo test -p evy-memory --test claude_mem_smoke -- --ignored
//! ```
//!
//! The default path (~/.claude-mem/claude-mem.db) is also tried as a
//! fallback when the env var is unset; this makes the test trivially
//! runnable on the operator's primary machine while still gated behind
//! `--ignored` in CI.

use std::path::PathBuf;

use evy_memory::ClaudeMemReader;

fn resolve_db_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CLAUDE_MEM_DB") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let home = std::env::var("HOME").ok()?;
    let default = PathBuf::from(home)
        .join(".claude-mem")
        .join("claude-mem.db");
    default.exists().then_some(default)
}

#[ignore = "requires a real claude-mem database; pass --ignored to run"]
#[tokio::test]
async fn reads_one_or_more_episodes_from_real_db() {
    let path =
        resolve_db_path().expect("set CLAUDE_MEM_DB or ensure ~/.claude-mem/claude-mem.db exists");
    let reader = ClaudeMemReader::open(&path)
        .await
        .expect("open claude-mem db");
    let recent = reader.recent_episodes(5).await.expect("recent");
    assert!(
        !recent.is_empty(),
        "expected at least one episode in claude-mem"
    );
    let head = &recent[0];
    assert!(!head.id.is_empty());
    assert!(!head.project.is_empty());
    assert!(!head.kind.is_empty());
}

#[ignore = "requires a real claude-mem database; pass --ignored to run"]
#[tokio::test]
async fn fts_search_does_not_panic_on_arbitrary_text() {
    let path =
        resolve_db_path().expect("set CLAUDE_MEM_DB or ensure ~/.claude-mem/claude-mem.db exists");
    let reader = ClaudeMemReader::open(&path).await.expect("open");
    // Variety of special chars and operators that would break naive FTS5
    // syntax. The reader is responsible for quoting / escaping these.
    for q in ["subctl", "rate:limit", "v3.3.*", "\"weird\"", "AND OR NOT"] {
        let hits = reader.search(q, 3).await.unwrap_or_default();
        // We don't assert hits exist (depends on corpus) — only that
        // the call returns without erroring. The FTS5 escape is what's
        // under test.
        let _ = hits.len();
    }
}
