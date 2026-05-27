//! Tmux query interface used by the Phase 4 watchdogs.
//!
//! The `evy-providers` crate has a `tmux` helper but it's `mod tmux;` —
//! private. Rather than punching a hole through the providers crate for
//! a sister crate's needs, we define a small read-only trait here and
//! ship a real impl that shells out via `tokio::process::Command`.
//!
//! The trait is read-only on purpose: watchdogs MUST NOT issue
//! `send-keys` / `kill-window` against tmux. Mutation is the daemon's /
//! provider's job. The watchdogs' role is observation only — they
//! detect, they notify, they prune in-memory state. They do not drive
//! tmux.
//!
//! # Testing
//!
//! All Phase 4 watchdogs accept `Arc<dyn TmuxQuery>`. Tests construct
//! a [`MockTmuxQuery`] with scripted responses; the registry smoke
//! test never shells out.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use evy_core::Result;
use tokio::process::Command;

use crate::error::watchdog_io_error;

/// Read-only tmux interrogation surface.
///
/// All methods are async because they shell out to `tmux(1)`. The mock
/// impl resolves immediately — async is purely shape-preserving.
#[async_trait]
pub trait TmuxQuery: Send + Sync {
    /// `tmux has-session -t <name>` exit status.
    async fn session_exists(&self, name: &str) -> Result<bool>;

    /// `tmux list-sessions -F '#{session_name}'`. Returns one name per
    /// element; on error returns an empty list (callers must treat
    /// "no sessions visible" the same as "tmux isn't running" — both
    /// mean nothing to watchdog).
    async fn list_sessions(&self) -> Result<Vec<String>>;

    /// `tmux capture-pane -p -t <target> -S -<lines>`. `target` is
    /// `<session>:<window>` or `<session>:<window>.<pane>`; the
    /// watchdog passes whatever the daemon gave it.
    async fn capture_pane(&self, target: &str, lines: usize) -> Result<String>;
}

/// Real tmux impl — shells out via `tokio::process::Command`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealTmuxQuery;

#[async_trait]
impl TmuxQuery for RealTmuxQuery {
    async fn session_exists(&self, name: &str) -> Result<bool> {
        let output = Command::new("tmux")
            .args(["has-session", "-t", name])
            .output()
            .await
            .map_err(|e| watchdog_io_error("tmux_query", format!("spawn tmux: {e}")))?;
        Ok(output.status.success())
    }

    async fn list_sessions(&self) -> Result<Vec<String>> {
        let output = Command::new("tmux")
            .args(["list-sessions", "-F", "#{session_name}"])
            .output()
            .await
            .map_err(|e| watchdog_io_error("tmux_query", format!("spawn tmux: {e}")))?;
        if !output.status.success() {
            // No tmux server / no sessions — that's a normal steady
            // state for a freshly-booted daemon. Return empty rather
            // than error so watchdogs treat it as "nothing to do".
            return Ok(Vec::new());
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect())
    }

    async fn capture_pane(&self, target: &str, lines: usize) -> Result<String> {
        let history = format!("-{lines}");
        let output = Command::new("tmux")
            .args(["capture-pane", "-p", "-t", target, "-S", history.as_str()])
            .output()
            .await
            .map_err(|e| watchdog_io_error("tmux_query", format!("spawn tmux: {e}")))?;
        if !output.status.success() {
            return Err(watchdog_io_error(
                "tmux_query",
                format!(
                    "tmux capture-pane -t {target} failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// In-process tmux mock for tests. Sessions, pane captures, and the
/// list-sessions result are all configurable up front; the impl is
/// purely interior-mutability-via-`Mutex`, no async I/O.
#[derive(Debug, Default)]
pub struct MockTmuxQuery {
    inner: Mutex<MockState>,
}

#[derive(Debug, Default)]
struct MockState {
    sessions: Vec<String>,
    /// `<target>` → captured pane content.
    panes: HashMap<String, String>,
}

impl MockTmuxQuery {
    /// Build an empty mock — no sessions, no panes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the list of live sessions; replaces any previous list.
    pub fn set_sessions(&self, names: impl IntoIterator<Item = impl Into<String>>) {
        let mut s = self.inner.lock().expect("mock-tmux-query poisoned");
        s.sessions = names.into_iter().map(Into::into).collect();
    }

    /// Set the pane content visible to `capture_pane(target, _)`.
    pub fn set_pane(&self, target: impl Into<String>, content: impl Into<String>) {
        let mut s = self.inner.lock().expect("mock-tmux-query poisoned");
        s.panes.insert(target.into(), content.into());
    }

    /// Remove a session AND any associated pane content. Mirrors the
    /// real-world "operator killed the tmux session" event.
    pub fn drop_session(&self, name: &str) {
        let mut s = self.inner.lock().expect("mock-tmux-query poisoned");
        s.sessions.retain(|n| n != name);
        s.panes.retain(|k, _| !k.starts_with(&format!("{name}:")));
    }
}

#[async_trait]
impl TmuxQuery for MockTmuxQuery {
    async fn session_exists(&self, name: &str) -> Result<bool> {
        let s = self.inner.lock().expect("mock-tmux-query poisoned");
        Ok(s.sessions.iter().any(|n| n == name))
    }

    async fn list_sessions(&self) -> Result<Vec<String>> {
        let s = self.inner.lock().expect("mock-tmux-query poisoned");
        Ok(s.sessions.clone())
    }

    async fn capture_pane(&self, target: &str, _lines: usize) -> Result<String> {
        let s = self.inner.lock().expect("mock-tmux-query poisoned");
        Ok(s.panes.get(target).cloned().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_session_exists_round_trips() {
        let m = MockTmuxQuery::new();
        m.set_sessions(["claude-a", "claude-b"]);
        assert!(m.session_exists("claude-a").await.unwrap());
        assert!(!m.session_exists("ghost").await.unwrap());
        let listed = m.list_sessions().await.unwrap();
        assert_eq!(listed.len(), 2);
    }

    #[tokio::test]
    async fn mock_pane_capture_returns_empty_for_unknown_target() {
        let m = MockTmuxQuery::new();
        let captured = m.capture_pane("nope:0", 10).await.unwrap();
        assert!(captured.is_empty());
    }

    #[tokio::test]
    async fn mock_drop_session_removes_panes() {
        let m = MockTmuxQuery::new();
        m.set_sessions(["claude-a"]);
        m.set_pane("claude-a:0", "hello");
        m.drop_session("claude-a");
        assert!(!m.session_exists("claude-a").await.unwrap());
        assert!(m.capture_pane("claude-a:0", 5).await.unwrap().is_empty());
    }
}
