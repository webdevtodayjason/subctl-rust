//! [`IdlePaneWatchdog`] — flag tmux panes whose content hasn't changed.
//!
//! # Phase 4 scope vs. v3
//!
//! v3's `idle-pane-watchdog.ts` does **buffered-prompt detection**: it
//! looks at the trailing line of the pane, ignores the rest, and pairs
//! the result with a recently-sent-directive registry to gate
//! auto-retry. That heuristic is high-value but heavy — it needs
//! correlated state with the dispatch path that doesn't exist in v4 yet.
//!
//! Phase 4 ships the **simpler hash-based** check: if the entire
//! captured pane content is byte-identical across `max_idle_secs / tick`
//! ticks, flag `IdlePane { pane, idle_secs }`. False positives are
//! cheap (operator looks, finds nothing, dismisses); false negatives
//! are the v3 bug we're trying to detect.
//!
//! TODO: Phase 5 — port the v3 buffered-prompt detection on top of
//! this hash-based scaffold:
//!   1. add a recently-sent-directives registry shared with the
//!      provider dispatch path
//!   2. switch the "is the pane stuck?" predicate from "full content
//!      hash unchanged" to "trailing-line text exists AND unchanged"
//!   3. wire the auto-retry gate (only press Enter when the trailing
//!      text matches a registered directive)
//!
//! # What this watchdog enumerates
//!
//! v4 lacks `Provider::list_workers()`. Until Phase 5 closes that gap,
//! `IdlePaneWatchdog` enumerates **every tmux session whose name starts
//! with a configurable prefix** (default `"claude-"` to match v3's
//! filter) and captures pane `:0` from each. The session-list filter
//! is the *only* thing today that distinguishes "Evy-managed pane"
//! from "operator's editor pane" — fragile, but consistent with v3.
//!
//! TODO: Phase 5 — drive enumeration from `Provider::list_workers()`
//! once that lands. Each worker carries its tmux target explicitly,
//! eliminating the prefix-sniff.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use evy_core::Result;
use tokio::sync::Mutex;

use crate::report::{Finding, TickReport};
use crate::tmux_query::TmuxQuery;
use crate::trait_def::{Watchdog, WatchdogContext, WatchdogSchedule};

/// Default trigger threshold — v3 used the same 10-minute window.
pub const DEFAULT_MAX_IDLE_SECS: u64 = 600;
/// Default tick cadence (matches the registry's outer 30s loop).
pub const DEFAULT_TICK_INTERVAL_SECS: u64 = 30;
/// Default session-name prefix matched against `tmux list-sessions`.
/// Mirrors v3's `claude-*` filter.
pub const DEFAULT_SESSION_PREFIX: &str = "claude-";

/// Watchdog that flags tmux panes whose captured content has not
/// changed across a sliding window.
pub struct IdlePaneWatchdog {
    /// Tmux query surface. Real daemon uses [`crate::RealTmuxQuery`];
    /// tests use [`crate::MockTmuxQuery`].
    tmux: Arc<dyn TmuxQuery>,
    /// Number of seconds of unchanged content before flagging.
    pub max_idle_secs: u64,
    /// Cadence at which the registry will call `tick`. Recorded here
    /// so the watchdog can report `idle_secs` proportional to wall
    /// time rather than tick count.
    pub tick_interval_secs: u64,
    /// Only inspect sessions whose name starts with this prefix.
    pub session_prefix: String,
    /// Per-session state. `last_changed` is wall-clock `Instant` so
    /// `idle_secs` survives clock skew that would break Utc deltas.
    state: Arc<Mutex<HashMap<String, PaneState>>>,
}

#[derive(Debug)]
struct PaneState {
    last_hash: u64,
    last_changed: Instant,
}

impl IdlePaneWatchdog {
    /// Build with default thresholds and a real tmux query.
    #[must_use]
    pub fn new(tmux: Arc<dyn TmuxQuery>) -> Self {
        Self {
            tmux,
            max_idle_secs: DEFAULT_MAX_IDLE_SECS,
            tick_interval_secs: DEFAULT_TICK_INTERVAL_SECS,
            session_prefix: DEFAULT_SESSION_PREFIX.to_owned(),
            state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Override the idle threshold (seconds).
    #[must_use]
    pub fn with_max_idle_secs(mut self, secs: u64) -> Self {
        self.max_idle_secs = secs;
        self
    }

    /// Override the session-name prefix filter.
    #[must_use]
    pub fn with_session_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.session_prefix = prefix.into();
        self
    }

    /// Override the tick cadence used to compute `idle_secs` from
    /// `Instant` deltas. The registry's actual cadence is independent;
    /// this is only the *labelled* cadence used inside findings.
    #[must_use]
    pub fn with_tick_interval_secs(mut self, secs: u64) -> Self {
        self.tick_interval_secs = secs;
        self
    }
}

#[async_trait]
impl Watchdog for IdlePaneWatchdog {
    fn name(&self) -> &str {
        "idle-pane"
    }

    fn schedule(&self) -> WatchdogSchedule {
        WatchdogSchedule::EveryNSecs(self.tick_interval_secs)
    }

    async fn tick(&self, _ctx: &WatchdogContext) -> Result<TickReport> {
        let sessions = self.tmux.list_sessions().await?;
        let mut findings = Vec::new();
        let mut seen_targets: Vec<String> = Vec::new();
        let now = Instant::now();

        for session in sessions
            .into_iter()
            .filter(|s| s.starts_with(&self.session_prefix))
        {
            let target = format!("{session}:0");
            let captured = match self.tmux.capture_pane(&target, 50).await {
                Ok(s) => s,
                Err(e) => {
                    // A failing capture for one pane shouldn't sink the
                    // whole watchdog — log and skip the pane.
                    tracing::debug!(
                        target = %target,
                        error = %e,
                        "idle-pane: capture failed; skipping pane",
                    );
                    continue;
                }
            };

            let hash = fast_hash(&captured);
            seen_targets.push(target.clone());

            let mut state = self.state.lock().await;
            let pane = state.entry(target.clone()).or_insert(PaneState {
                last_hash: hash,
                last_changed: now,
            });
            if pane.last_hash != hash {
                pane.last_hash = hash;
                pane.last_changed = now;
                continue;
            }
            let idle = now.saturating_duration_since(pane.last_changed).as_secs();
            if idle >= self.max_idle_secs {
                findings.push(Finding::IdlePane {
                    pane: target,
                    idle_secs: idle,
                });
            }
        }

        // Garbage-collect entries for panes that vanished — they'll
        // never tick again, and lingering rows would skew the per-pane
        // `idle_secs` calculation if the same target ever returned.
        {
            let mut state = self.state.lock().await;
            state.retain(|target, _| seen_targets.iter().any(|t| t == target));
        }

        if findings.is_empty() {
            Ok(TickReport::healthy(self.name()))
        } else {
            Ok(TickReport::with_findings(self.name(), findings))
        }
    }
}

/// Cheap stable hash of pane content. We use the `DefaultHasher` —
/// stable across one process lifetime (which is all the watchdog
/// needs) and faster than allocating a SHA. Cryptographic strength
/// is irrelevant; we just need "did the content change?".
fn fast_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::watchdog_ctx;
    use crate::tmux_query::MockTmuxQuery;

    #[tokio::test]
    async fn empty_tmux_yields_healthy_report() {
        let (ctx, _guards) = watchdog_ctx().await;
        let w = IdlePaneWatchdog::new(Arc::new(MockTmuxQuery::new()));
        let report = w.tick(&ctx).await.unwrap();
        assert!(report.healthy);
        assert_eq!(report.findings, vec![Finding::Healthy]);
    }

    #[tokio::test]
    async fn non_claude_session_is_ignored() {
        let (ctx, _guards) = watchdog_ctx().await;
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["random-editor-session"]);
        tmux.set_pane("random-editor-session:0", "vim is editing");
        let w = IdlePaneWatchdog::new(tmux);
        let report = w.tick(&ctx).await.unwrap();
        assert!(report.healthy);
        assert!(matches!(report.findings.as_slice(), [Finding::Healthy]));
    }

    #[tokio::test]
    async fn unchanged_pane_below_threshold_does_not_fire() {
        let (ctx, _guards) = watchdog_ctx().await;
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-a"]);
        tmux.set_pane("claude-a:0", "$ working\n");
        // First tick — record baseline.
        let w = IdlePaneWatchdog::new(tmux.clone()).with_max_idle_secs(3600);
        let r1 = w.tick(&ctx).await.unwrap();
        assert!(r1.healthy);
        // Second tick, same content — idle=0 so far. Still healthy.
        let r2 = w.tick(&ctx).await.unwrap();
        assert!(r2.healthy);
        assert!(matches!(r2.findings.as_slice(), [Finding::Healthy]));
    }

    #[tokio::test]
    async fn unchanged_pane_above_threshold_fires() {
        let (ctx, _guards) = watchdog_ctx().await;
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-stuck"]);
        tmux.set_pane("claude-stuck:0", "$ waiting\n");
        // Threshold of zero means "any unchanged tick fires."
        let w = IdlePaneWatchdog::new(tmux).with_max_idle_secs(0);
        let _ = w.tick(&ctx).await.unwrap(); // seed baseline
        let report = w.tick(&ctx).await.unwrap();
        assert_eq!(report.findings.len(), 1);
        match &report.findings[0] {
            Finding::IdlePane { pane, idle_secs: _ } => {
                assert_eq!(pane, "claude-stuck:0");
            }
            other => panic!("expected IdlePane finding, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn changed_content_resets_idle_clock() {
        let (ctx, _guards) = watchdog_ctx().await;
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-busy"]);
        tmux.set_pane("claude-busy:0", "$ first\n");
        let w = IdlePaneWatchdog::new(tmux.clone()).with_max_idle_secs(0);
        let _ = w.tick(&ctx).await.unwrap();
        // Content changes → clock resets → next tick should NOT fire.
        tmux.set_pane("claude-busy:0", "$ different\n");
        let report = w.tick(&ctx).await.unwrap();
        assert!(report.healthy);
        assert!(
            !report
                .findings
                .iter()
                .any(|f| matches!(f, Finding::IdlePane { .. })),
            "changed content should not fire IdlePane"
        );
    }

    #[tokio::test]
    async fn vanished_pane_is_gc_d_from_state() {
        let (ctx, _guards) = watchdog_ctx().await;
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-temp"]);
        tmux.set_pane("claude-temp:0", "hello");
        let w = IdlePaneWatchdog::new(tmux.clone());
        let _ = w.tick(&ctx).await.unwrap();
        assert_eq!(w.state.lock().await.len(), 1);
        // Pane goes away.
        tmux.drop_session("claude-temp");
        let _ = w.tick(&ctx).await.unwrap();
        assert!(w.state.lock().await.is_empty());
    }
}
