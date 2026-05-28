//! [`AutoNudgeWatchdog`] — gently prod stuck workers, escalate the
//! incorrigible ones.
//!
//! # What v3 did
//!
//! v3's `components/evy/auto-nudge.ts` ran a per-team state machine
//! against the worker-reply text, classified replies (`completed_idle`,
//! `awaiting_input`, …), and either dispatched an HMAC-authenticated
//! `subctl_orch_msg` directive or escalated through
//! `notifications.ts` to Telegram. v4 has neither the per-team registry
//! state nor the notification pipeline ported yet, so Phase 5 ships the
//! **mechanism** — detect-nudge-escalate — and defers the v3 reply
//! classifier to Phase 6.
//!
//! # The state machine
//!
//! For each Evy-managed tmux pane (filtered by `session_prefix` for
//! parity with [`crate::IdlePaneWatchdog`]):
//!
//! 1. **First sight** — hash the pane content, record it, do nothing
//!    else. The watchdog needs at least one prior observation before it
//!    can call a pane "stuck."
//! 2. **Content changed since last tick** — the worker produced output;
//!    reset its nudge state if any.
//! 3. **Content unchanged AND idle ≥ `idle_threshold_secs`** — the
//!    worker is candidate-stuck. Check the nudge history:
//!    - **No prior nudge** → dispatch nudge #1, emit
//!      [`Finding::WorkerNudged`] with `attempts: 1`.
//!    - **Last nudge < `nudge_cooldown_secs` ago** → hold (silent — give
//!      the worker time to respond).
//!    - **Last nudge ≥ cooldown AND `attempts < escalation_threshold`**
//!      → dispatch nudge #N, emit `WorkerNudged { attempts: N }`.
//!    - **Last nudge ≥ cooldown AND `attempts ≥ escalation_threshold`**
//!      → emit [`Finding::WorkerDead`], drop the worker from the
//!      nudge history (terminal — don't re-nudge a dead worker).
//! 4. **WEB-216 fix carried forward** — a dispatch that returns `Err`
//!    does NOT advance the attempt counter. The worker never saw the
//!    nudge; the sweep cadence is the backoff. Otherwise a flaky
//!    transport (Anthropic 529, etc.) would race the worker to
//!    "unresponsive" without giving it the chance to reply.
//!
//! # Worker identity
//!
//! v4 lacks `Provider::list_workers()`, so the watchdog identifies
//! workers by their tmux pane target (e.g. `"claude-foo:0"`). The first
//! time a target is seen we mint a stable v4 `WorkerId` for it and
//! remember the mapping. The findings on the wire carry that `WorkerId`
//! so dashboard consumers can correlate across ticks; Phase 6 will swap
//! the mapping for real `Provider::list_workers()` ids.
//!
//! TODO: Phase 6 —
//!   - port `classifyWorkerReply` (completed_idle / awaiting_input /
//!     blocked) so a worker that explicitly self-reports "done" is not
//!     nudged at all.
//!   - swap `String` pane targets for `Provider::list_workers()` once
//!     that lands.
//!   - persist the nudge history to disk so a daemon restart doesn't
//!     reset the escalation clock.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use evy_core::{Result, WorkerId};
use tokio::sync::Mutex;

use crate::directive::DirectiveDispatcher;
use crate::report::{Finding, TickReport};
use crate::tmux_query::TmuxQuery;
use crate::trait_def::{Watchdog, WatchdogContext, WatchdogSchedule};

/// Default idle threshold — 10 minutes (matches v3's
/// `staleness_threshold_min: 10`).
pub const DEFAULT_IDLE_THRESHOLD_SECS: u64 = 600;
/// Default cooldown between nudges to the same worker — 5 minutes.
/// v3 used 30 min for the team-level retry; we shorten because
/// v4 nudges at the worker (pane) level which is much cheaper.
pub const DEFAULT_NUDGE_COOLDOWN_SECS: u64 = 300;
/// Default nudge attempts before escalating to [`Finding::WorkerDead`].
/// v3 escalated after one retry; v4 gives three before pulling the
/// dead-worker trigger, matching the spec.
pub const DEFAULT_ESCALATION_THRESHOLD: u32 = 3;
/// Default tick cadence — once every 5 minutes.
pub const DEFAULT_TICK_INTERVAL_SECS: u64 = 300;
/// Default session-name prefix matched against `tmux list-sessions`,
/// mirroring [`crate::idle_pane::DEFAULT_SESSION_PREFIX`].
pub const DEFAULT_SESSION_PREFIX: &str = "claude-";

/// Tunable thresholds for [`AutoNudgeWatchdog`].
#[derive(Debug, Clone)]
pub struct AutoNudgeConfig {
    /// Seconds of unchanged pane content before the worker is considered
    /// stuck and a first nudge is dispatched.
    pub idle_threshold_secs: u64,
    /// Seconds that must elapse between nudges to the same worker.
    pub nudge_cooldown_secs: u64,
    /// Nudges to dispatch before escalating to
    /// [`Finding::WorkerDead`]. The Nth nudge fires when
    /// `attempts == escalation_threshold - 1` and the next overdue
    /// re-tick fires the escalation.
    pub escalation_threshold: u32,
}

impl Default for AutoNudgeConfig {
    fn default() -> Self {
        Self {
            idle_threshold_secs: DEFAULT_IDLE_THRESHOLD_SECS,
            nudge_cooldown_secs: DEFAULT_NUDGE_COOLDOWN_SECS,
            escalation_threshold: DEFAULT_ESCALATION_THRESHOLD,
        }
    }
}

/// Per-pane bookkeeping. `Instant` for wall-clock deltas (survives Utc
/// clock skew); `WorkerId` minted on first sight and pinned for the
/// lifetime of the watchdog process.
#[derive(Debug)]
struct PaneState {
    worker_id: WorkerId,
    last_hash: u64,
    last_changed: Instant,
    nudge: Option<NudgeRecord>,
}

#[derive(Debug, Clone)]
struct NudgeRecord {
    last_nudge_at: Instant,
    attempts: u32,
    /// Utc capture of when the pane last *changed*, recorded at
    /// nudge time and folded into [`Finding::WorkerDead`] on escalation.
    last_activity: chrono::DateTime<Utc>,
}

/// Watchdog that detects stuck workers, dispatches gentle nudge
/// directives, and escalates to [`Finding::WorkerDead`] after a
/// configurable number of unanswered nudges.
pub struct AutoNudgeWatchdog {
    /// Tunable thresholds. Public so callers can tweak without
    /// reconstructing the watchdog.
    pub config: AutoNudgeConfig,
    /// Read-only tmux interface — used to enumerate sessions and hash
    /// pane content.
    tmux: Arc<dyn TmuxQuery>,
    /// Write surface for delivering nudges.
    dispatcher: Arc<dyn DirectiveDispatcher>,
    /// Cadence at which the registry will call `tick`.
    pub tick_interval_secs: u64,
    /// Only inspect sessions whose name starts with this prefix.
    pub session_prefix: String,
    /// Per-pane state, keyed by the `<session>:<window>` target string.
    /// The `WorkerId` lives inside [`PaneState`] so the mapping survives
    /// across ticks without a second lookup table.
    state: Arc<Mutex<HashMap<String, PaneState>>>,
}

impl AutoNudgeWatchdog {
    /// Build with default thresholds and the supplied tmux + dispatcher.
    #[must_use]
    pub fn new(tmux: Arc<dyn TmuxQuery>, dispatcher: Arc<dyn DirectiveDispatcher>) -> Self {
        Self {
            config: AutoNudgeConfig::default(),
            tmux,
            dispatcher,
            tick_interval_secs: DEFAULT_TICK_INTERVAL_SECS,
            session_prefix: DEFAULT_SESSION_PREFIX.to_owned(),
            state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Replace the entire config.
    #[must_use]
    pub fn with_config(mut self, config: AutoNudgeConfig) -> Self {
        self.config = config;
        self
    }

    /// Override the tick cadence.
    #[must_use]
    pub fn with_tick_interval_secs(mut self, secs: u64) -> Self {
        self.tick_interval_secs = secs;
        self
    }

    /// Override the session-name prefix.
    #[must_use]
    pub fn with_session_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.session_prefix = prefix.into();
        self
    }
}

#[async_trait]
impl Watchdog for AutoNudgeWatchdog {
    fn name(&self) -> &str {
        "auto-nudge"
    }

    fn schedule(&self) -> WatchdogSchedule {
        WatchdogSchedule::EveryNSecs(self.tick_interval_secs)
    }

    async fn tick(&self, _ctx: &WatchdogContext) -> Result<TickReport> {
        let sessions = self.tmux.list_sessions().await?;
        let mut findings = Vec::new();
        let mut seen_targets: Vec<String> = Vec::new();
        let now = Instant::now();
        let now_utc = Utc::now();

        for session in sessions
            .into_iter()
            .filter(|s| s.starts_with(&self.session_prefix))
        {
            let target = format!("{session}:0");
            let captured = match self.tmux.capture_pane(&target, 50).await {
                Ok(s) => s,
                Err(e) => {
                    // A single failing capture must not sink the whole
                    // watchdog — log + skip, mirroring IdlePaneWatchdog.
                    tracing::debug!(
                        target = %target,
                        error = %e,
                        "auto-nudge: capture failed; skipping pane",
                    );
                    continue;
                }
            };
            seen_targets.push(target.clone());

            let hash = fast_hash(&captured);
            // Determine the WorkerId + idle calculation under the lock,
            // then drop the lock BEFORE the await on the dispatcher so
            // multi-pane ticks don't serialise on the dispatch path.
            //
            // First-sight handling: a pane we've never observed before
            // gets seeded and skipped — we have no prior baseline to
            // declare it "stuck." Without this carve-out, an aggressive
            // threshold (idle = 0) would nudge every pane on its very
            // first sighting because `last_changed = now` makes the
            // idle delta exactly 0 which satisfies `>= 0`.
            let (worker_id, idle_secs, prior_nudge, first_sight) = {
                let mut state = self.state.lock().await;
                let first_sight = !state.contains_key(&target);
                let entry = state.entry(target.clone()).or_insert_with(|| PaneState {
                    worker_id: WorkerId::new(),
                    last_hash: hash,
                    last_changed: now,
                    nudge: None,
                });
                if entry.last_hash != hash {
                    // Worker produced fresh output — reset the clock
                    // and clear any in-flight nudge state. The next
                    // tick that catches them stuck starts fresh.
                    entry.last_hash = hash;
                    entry.last_changed = now;
                    entry.nudge = None;
                    continue;
                }
                let idle = now.saturating_duration_since(entry.last_changed).as_secs();
                (entry.worker_id, idle, entry.nudge.clone(), first_sight)
            };

            if first_sight {
                // Seeded just now — wait for the next tick before
                // calling this pane stuck.
                continue;
            }

            if idle_secs < self.config.idle_threshold_secs {
                continue;
            }

            // Cooldown gate.
            if let Some(record) = &prior_nudge {
                let since_last = now
                    .saturating_duration_since(record.last_nudge_at)
                    .as_secs();
                if since_last < self.config.nudge_cooldown_secs {
                    continue;
                }
                // Escalation — terminal for this worker.
                if record.attempts >= self.config.escalation_threshold {
                    findings.push(Finding::WorkerDead {
                        worker_id,
                        last_activity: record.last_activity,
                    });
                    self.state.lock().await.remove(&target);
                    continue;
                }
            }

            // Compose + dispatch the nudge. WEB-216: only advance the
            // attempt counter on a successful dispatch.
            let next_attempts = prior_nudge.as_ref().map_or(1, |r| r.attempts + 1);
            let body = nudge_body(idle_secs, next_attempts);
            match self.dispatcher.dispatch_directive(worker_id, &body).await {
                Ok(()) => {
                    let mut state = self.state.lock().await;
                    if let Some(entry) = state.get_mut(&target) {
                        // last_activity is the wall-clock UTC of when
                        // the pane was last seen to change — captured
                        // at the most recent "content changed" branch
                        // by recomputing from the Instant delta.
                        let last_activity = now_utc
                            - chrono::Duration::seconds(
                                i64::try_from(idle_secs).unwrap_or(i64::MAX),
                            );
                        entry.nudge = Some(NudgeRecord {
                            last_nudge_at: now,
                            attempts: next_attempts,
                            last_activity,
                        });
                    }
                    findings.push(Finding::WorkerNudged {
                        worker_id,
                        attempts: next_attempts,
                    });
                }
                Err(e) => {
                    // Delivery failed: the worker never saw the nudge.
                    // Do NOT advance the counter; the sweep cadence
                    // becomes the backoff and we retry on the next
                    // tick that finds them still idle.
                    tracing::warn!(
                        target = %target,
                        attempts = next_attempts,
                        error = %e,
                        "auto-nudge: directive dispatch failed; not advancing counter",
                    );
                }
            }
        }

        // GC vanished panes — same logic as IdlePaneWatchdog. A pane
        // that never reappears would otherwise hold its nudge slot
        // forever and spam stale `WorkerDead` findings if the same
        // target returned with the same content hash.
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

/// Compose the directive body delivered to a stuck worker. Kept as a
/// free function so tests can pin the wording without instantiating a
/// watchdog. Mirrors v3's `[auto-nudge]` prefix so existing log
/// filters keep matching.
fn nudge_body(idle_secs: u64, attempt: u32) -> String {
    let minutes = idle_secs / 60;
    if attempt == 1 {
        format!(
            "[auto-nudge] You've been silent for ~{minutes} min. Reply with current status — \
             working on it, blocked, done, or awaiting input?",
        )
    } else {
        format!(
            "[auto-nudge · attempt {attempt}] Still no output after ~{minutes} min. \
             Reply with current status or this worker will be flagged dead.",
        )
    }
}

/// Cheap stable hash of pane content (one process lifetime is plenty).
/// Identical to `idle_pane::fast_hash` — duplicated rather than shared
/// because Phase 6 may switch the auto-nudge predicate from
/// "content unchanged" to a structural predicate that's not just a
/// hash, and we'd rather copy the trivial helper than couple the two.
fn fast_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directive::MockDirectiveDispatcher;
    use crate::test_support::watchdog_ctx;
    use crate::tmux_query::MockTmuxQuery;

    /// Build an AutoNudge that is "instantly past every threshold."
    /// idle_threshold = 0 + cooldown = 0 means every tick acts on the
    /// state machine without us having to sleep real-time.
    fn aggressive_nudger(
        tmux: Arc<MockTmuxQuery>,
        dispatcher: Arc<MockDirectiveDispatcher>,
        escalation: u32,
    ) -> AutoNudgeWatchdog {
        AutoNudgeWatchdog::new(tmux, dispatcher).with_config(AutoNudgeConfig {
            idle_threshold_secs: 0,
            nudge_cooldown_secs: 0,
            escalation_threshold: escalation,
        })
    }

    #[tokio::test]
    async fn empty_tmux_yields_healthy_report() {
        let (ctx, _guards) = watchdog_ctx().await;
        let w = AutoNudgeWatchdog::new(
            Arc::new(MockTmuxQuery::new()),
            Arc::new(MockDirectiveDispatcher::new()),
        );
        let r = w.tick(&ctx).await.unwrap();
        assert!(r.healthy);
        assert_eq!(r.findings, vec![Finding::Healthy]);
    }

    #[tokio::test]
    async fn non_prefix_session_is_ignored() {
        let (ctx, _guards) = watchdog_ctx().await;
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["operator-shell"]);
        tmux.set_pane("operator-shell:0", "$ vim");
        let dispatcher = Arc::new(MockDirectiveDispatcher::new());
        let w = aggressive_nudger(tmux, dispatcher.clone(), 3);
        let _ = w.tick(&ctx).await.unwrap();
        let _ = w.tick(&ctx).await.unwrap();
        assert_eq!(dispatcher.count(), 0);
    }

    #[tokio::test]
    async fn first_sight_does_not_nudge() {
        // The watchdog needs at least one prior observation before it
        // can call a pane stuck. First sight: record + skip.
        let (ctx, _guards) = watchdog_ctx().await;
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-fresh"]);
        tmux.set_pane("claude-fresh:0", "$ booting");
        let dispatcher = Arc::new(MockDirectiveDispatcher::new());
        let w = aggressive_nudger(tmux, dispatcher.clone(), 3);
        let r = w.tick(&ctx).await.unwrap();
        assert!(r.healthy);
        assert_eq!(dispatcher.count(), 0, "first sight must not dispatch");
    }

    #[tokio::test]
    async fn unchanged_content_triggers_first_nudge_then_holds_in_cooldown() {
        let (ctx, _guards) = watchdog_ctx().await;
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-stuck"]);
        tmux.set_pane("claude-stuck:0", "$ waiting");
        let dispatcher = Arc::new(MockDirectiveDispatcher::new());
        // Cooldown = 60s, idle = 0 → first re-tick nudges, second
        // re-tick holds because cooldown hasn't elapsed in real time.
        let w = AutoNudgeWatchdog::new(tmux, dispatcher.clone()).with_config(AutoNudgeConfig {
            idle_threshold_secs: 0,
            nudge_cooldown_secs: 60,
            escalation_threshold: 3,
        });
        let _ = w.tick(&ctx).await.unwrap(); // seed
        let r = w.tick(&ctx).await.unwrap(); // first nudge
        assert_eq!(dispatcher.count(), 1);
        assert!(
            r.findings
                .iter()
                .any(|f| matches!(f, Finding::WorkerNudged { attempts: 1, .. })),
            "expected WorkerNudged{{ attempts: 1 }}; got {:?}",
            r.findings
        );
        // Third tick is still inside the 60s cooldown → no new nudge.
        let r2 = w.tick(&ctx).await.unwrap();
        assert_eq!(dispatcher.count(), 1, "cooldown must hold");
        assert!(r2.healthy);
    }

    #[tokio::test]
    async fn changed_content_resets_nudge_state() {
        let (ctx, _guards) = watchdog_ctx().await;
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-talky"]);
        tmux.set_pane("claude-talky:0", "$ step 1");
        let dispatcher = Arc::new(MockDirectiveDispatcher::new());
        let w = aggressive_nudger(tmux.clone(), dispatcher.clone(), 3);
        // Seed + first nudge.
        let _ = w.tick(&ctx).await.unwrap();
        let _ = w.tick(&ctx).await.unwrap();
        assert_eq!(dispatcher.count(), 1);
        // Worker produces output → state should reset.
        tmux.set_pane("claude-talky:0", "$ step 2");
        let r = w.tick(&ctx).await.unwrap();
        assert!(r.healthy);
        assert!(
            !r.findings
                .iter()
                .any(|f| matches!(f, Finding::WorkerNudged { .. } | Finding::WorkerDead { .. })),
            "fresh output must clear nudge state",
        );
        // And the next tick, with content frozen again, fires
        // attempts: 1 (not 2) — the prior nudge state was dropped.
        let r2 = w.tick(&ctx).await.unwrap();
        let nudges: Vec<_> = r2
            .findings
            .iter()
            .filter_map(|f| match f {
                Finding::WorkerNudged { attempts, .. } => Some(*attempts),
                _ => None,
            })
            .collect();
        assert_eq!(nudges, vec![1], "reset means next nudge is attempt 1");
    }

    #[tokio::test]
    async fn escalation_fires_worker_dead_after_threshold() {
        let (ctx, _guards) = watchdog_ctx().await;
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-zombie"]);
        tmux.set_pane("claude-zombie:0", "$ frozen");
        let dispatcher = Arc::new(MockDirectiveDispatcher::new());
        // threshold = 2: after 2 successful nudges, the next overdue
        // tick escalates instead of nudging.
        let w = aggressive_nudger(tmux, dispatcher.clone(), 2);
        let _ = w.tick(&ctx).await.unwrap(); // seed
        let r1 = w.tick(&ctx).await.unwrap();
        let r2 = w.tick(&ctx).await.unwrap();
        let r3 = w.tick(&ctx).await.unwrap();
        assert!(
            r1.findings
                .iter()
                .any(|f| matches!(f, Finding::WorkerNudged { attempts: 1, .. })),
            "tick 1 should nudge attempt 1; got {:?}",
            r1.findings,
        );
        assert!(
            r2.findings
                .iter()
                .any(|f| matches!(f, Finding::WorkerNudged { attempts: 2, .. })),
            "tick 2 should nudge attempt 2; got {:?}",
            r2.findings,
        );
        assert!(
            r3.findings
                .iter()
                .any(|f| matches!(f, Finding::WorkerDead { .. })),
            "tick 3 should escalate; got {:?}",
            r3.findings,
        );
        assert_eq!(dispatcher.count(), 2, "no nudge dispatched on escalation");
        // Subsequent tick: the pane has been dropped from history, so
        // it gets re-seeded as a fresh observation — no findings beyond
        // Healthy.
        let r4 = w.tick(&ctx).await.unwrap();
        assert!(r4.healthy);
        assert!(
            !r4.findings
                .iter()
                .any(|f| matches!(f, Finding::WorkerDead { .. } | Finding::WorkerNudged { .. })),
            "dead worker must not re-emit findings on subsequent ticks",
        );
    }

    #[tokio::test]
    async fn delivery_failure_does_not_advance_attempts() {
        // WEB-216: a worker that never received the nudge must not be
        // escalated. Failed dispatches keep the counter where it is.
        let (ctx, _guards) = watchdog_ctx().await;
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-flaky"]);
        tmux.set_pane("claude-flaky:0", "$ stuck");
        let dispatcher = Arc::new(MockDirectiveDispatcher::new());
        dispatcher.fail_next_dispatches_with("simulated transport 5xx");
        let w = aggressive_nudger(tmux, dispatcher.clone(), 3);
        let _ = w.tick(&ctx).await.unwrap(); // seed
        let r1 = w.tick(&ctx).await.unwrap();
        let r2 = w.tick(&ctx).await.unwrap();
        // Neither tick should record a WorkerNudged finding because
        // neither dispatch succeeded; counter stays at 0.
        assert!(
            !r1.findings
                .iter()
                .any(|f| matches!(f, Finding::WorkerNudged { .. } | Finding::WorkerDead { .. })),
            "failed dispatch must not emit WorkerNudged",
        );
        assert!(
            !r2.findings
                .iter()
                .any(|f| matches!(f, Finding::WorkerNudged { .. } | Finding::WorkerDead { .. })),
            "failed dispatch must not emit WorkerNudged",
        );
        assert_eq!(dispatcher.count(), 0);
        // Now succeed — should be attempt 1 (not 3).
        dispatcher.clear_failure();
        let r3 = w.tick(&ctx).await.unwrap();
        assert!(
            r3.findings
                .iter()
                .any(|f| matches!(f, Finding::WorkerNudged { attempts: 1, .. })),
            "after delivery recovers, next nudge is attempt 1 not 3; got {:?}",
            r3.findings,
        );
    }

    #[tokio::test]
    async fn worker_id_is_stable_across_ticks() {
        let (ctx, _guards) = watchdog_ctx().await;
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-pinned"]);
        tmux.set_pane("claude-pinned:0", "$ stuck");
        let dispatcher = Arc::new(MockDirectiveDispatcher::new());
        let w = aggressive_nudger(tmux, dispatcher.clone(), 5);
        let _ = w.tick(&ctx).await.unwrap();
        let r1 = w.tick(&ctx).await.unwrap();
        let r2 = w.tick(&ctx).await.unwrap();
        let id1 = r1.findings.iter().find_map(|f| match f {
            Finding::WorkerNudged { worker_id, .. } => Some(*worker_id),
            _ => None,
        });
        let id2 = r2.findings.iter().find_map(|f| match f {
            Finding::WorkerNudged { worker_id, .. } => Some(*worker_id),
            _ => None,
        });
        assert_eq!(id1, id2);
        assert!(id1.is_some());
    }

    #[tokio::test]
    async fn vanished_pane_is_gc_d_from_history() {
        let (ctx, _guards) = watchdog_ctx().await;
        let tmux = Arc::new(MockTmuxQuery::new());
        tmux.set_sessions(["claude-temp"]);
        tmux.set_pane("claude-temp:0", "hello");
        let dispatcher = Arc::new(MockDirectiveDispatcher::new());
        let w = aggressive_nudger(tmux.clone(), dispatcher, 3);
        let _ = w.tick(&ctx).await.unwrap();
        assert_eq!(w.state.lock().await.len(), 1);
        tmux.drop_session("claude-temp");
        let _ = w.tick(&ctx).await.unwrap();
        assert!(w.state.lock().await.is_empty());
    }

    #[test]
    fn nudge_body_first_attempt_uses_silent_phrasing() {
        let body = nudge_body(600, 1);
        assert!(body.contains("[auto-nudge]"));
        assert!(body.contains("10 min"));
        assert!(!body.contains("attempt"));
    }

    #[test]
    fn nudge_body_repeat_attempt_carries_count() {
        let body = nudge_body(1200, 3);
        assert!(body.contains("attempt 3"));
        assert!(body.contains("20 min"));
        assert!(body.contains("flagged dead"));
    }

    #[test]
    fn default_config_matches_constants() {
        let cfg = AutoNudgeConfig::default();
        assert_eq!(cfg.idle_threshold_secs, DEFAULT_IDLE_THRESHOLD_SECS);
        assert_eq!(cfg.nudge_cooldown_secs, DEFAULT_NUDGE_COOLDOWN_SECS);
        assert_eq!(cfg.escalation_threshold, DEFAULT_ESCALATION_THRESHOLD);
    }
}
