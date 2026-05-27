//! Tier 3 → Tier 4 promotion ticker — Phase 4 Slice E.
//!
//! The promotion loop periodically scans recent rows in the
//! [`ObservationLog`] (Tier 3, Memori), scores each one for
//! "promotion value" (how much we'd benefit from having this in
//! permanent graph memory), and promotes the ones above a threshold
//! into [`CogneeClient`] (Tier 4).
//!
//! ## Phase 4 design choices
//!
//! ### Scoring is deliberately dumb
//!
//! Phase 4's scorer is a switch statement on [`ObservationKind`]:
//!
//! | kind / outcome                              | score |
//! |---------------------------------------------|-------|
//! | `OperatorMessage`                           | 0.90  |
//! | `WorkerCompleted { status: Succeeded }`     | 0.80  |
//! | `PolicyChecked { outcome ~= "allow*" }`     | 0.50  |
//! | everything else (scored)                    | 0.30  |
//! | `DaemonShutdown`                            | skip  |
//!
//! With the default threshold of `0.7`, only `OperatorMessage` and
//! successful `WorkerCompleted` rows actually land in Cognee. Failed
//! workers, denied policy checks, dispatch records, scheduler ticks,
//! and boot/shutdown bookkeeping all stay Tier-3-only.
//!
//! `DaemonShutdown` is filtered out entirely — it doesn't even enter
//! the "considered" count; the rationale is that shutdown rows are
//! pure runtime telemetry with no decision value for future planning.
//!
//! ### Watermark is in-memory
//!
//! `PromotionTicker` keeps an in-memory `last_promoted_obs_id`. On
//! daemon restart, that resets to `None` and the next tick will
//! re-evaluate the same recent slice — for the kinds Phase 4 actually
//! promotes, re-evaluation is idempotent at the *score* layer but
//! Cognee will get a duplicate `/remember` call. Tier 4 dedupes
//! semantically (graph nodes merge on content), so this is acceptable
//! for Phase 4 but flagged below.
//!
//! ### Phase 5 work
//!
//! - `// TODO: Phase 5` — persist watermark to disk so promotions
//!   don't re-issue on restart.
//! - `// TODO: Phase 5` — swap [`score_observation`] for an
//!   LLM-driven scorer that reads the observation's surrounding
//!   correlation group and assigns a calibrated score.
//! - `// TODO: Phase 5` — back off when Cognee is unreachable rather
//!   than logging every tick.

use std::sync::Arc;
use std::time::Duration;

use evy_core::Result;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::cognee::CogneeClient;
use crate::observation::{Observation, ObservationKind};
use crate::observation_log::ObservationLog;

// ── scoring constants (single source of truth) ──────────────────────

/// Score for `OperatorMessage` rows. The operator is the highest-signal
/// information source we have — anything they say is worth promoting.
pub const SCORE_OPERATOR_MESSAGE: f32 = 0.9;

/// Score for successful `WorkerCompleted` rows. Successes encode the
/// "this is a working solution" signal worth lifting into the graph.
pub const SCORE_WORKER_SUCCEEDED: f32 = 0.8;

/// Score for `PolicyChecked` rows that resolved to an allow. Captures
/// the operator's running set of accepted commands without flooding
/// Tier 4 with the policy gate's per-call decisions.
pub const SCORE_POLICY_ALLOW: f32 = 0.5;

/// Catch-all score for everything else that we *did* consider (i.e.
/// wasn't filtered by `score_observation` returning `None`). With the
/// default threshold this is always below the promotion line.
pub const SCORE_DEFAULT: f32 = 0.3;

/// Default promotion threshold. Anything strictly less than this stays
/// in Tier 3.
pub const DEFAULT_THRESHOLD: f32 = 0.7;

/// Slice size for one tick. Mirrors v3's `DEFAULT_BATCH_LIMIT = 200`.
const TICK_BATCH_LIMIT: usize = 200;

// ── public types ────────────────────────────────────────────────────

/// Summary of one [`PromotionTicker::tick`] invocation.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TickReport {
    /// Number of observations the scorer produced a `Some(score)` for.
    /// Filtered-out kinds (e.g. `DaemonShutdown`) are NOT counted here.
    pub considered: usize,
    /// Number that scored at or above the threshold AND succeeded on
    /// the Cognee `add_observation` round-trip.
    pub promoted: usize,
    /// `considered - promoted - errors.len()` — i.e. rows that were
    /// scored but fell below the threshold. (Rows that scored over the
    /// threshold but failed the remote call land in `errors`, not here.)
    pub skipped: usize,
    /// One entry per failed Cognee call. Bounded by `considered` (we
    /// don't retry within a tick).
    pub errors: Vec<String>,
}

/// Periodic Tier-3 → Tier-4 promotion loop.
///
/// Construct once, share via [`Arc`], drive with [`Self::tick`] (or the
/// longer-lived [`Self::run`] loop). Cheap to clone if you really need
/// to — internally everything is `Arc`-backed.
pub struct PromotionTicker {
    log: Arc<ObservationLog>,
    cognee: Arc<CogneeClient>,
    threshold: f32,
    /// Most-recent obs id we've already considered. Tick reads the
    /// recent slice and stops once it hits this id (or runs out of new
    /// rows). `None` on first tick — full slice is evaluated.
    ///
    /// In-memory only — see module docs for the Phase 5 follow-up.
    last_seen_obs_id: Mutex<Option<Uuid>>,
}

impl PromotionTicker {
    /// Build a new ticker. `threshold` is the minimum score for
    /// promotion; pass [`DEFAULT_THRESHOLD`] (0.7) if you don't have
    /// a better value.
    #[must_use]
    pub fn new(log: Arc<ObservationLog>, cognee: Arc<CogneeClient>, threshold: f32) -> Self {
        Self {
            log,
            cognee,
            threshold,
            last_seen_obs_id: Mutex::new(None),
        }
    }

    /// Run one promotion pass. Always returns a [`TickReport`] — per-row
    /// failures are surfaced in `TickReport::errors`, not raised.
    ///
    /// # Errors
    /// Only ever fails if the [`ObservationLog`] query itself fails
    /// (sqlite I/O). All Cognee failures are absorbed into the report.
    pub async fn tick(&self) -> Result<TickReport> {
        let rows = self.log.query_recent(TICK_BATCH_LIMIT).await?;
        let mut report = TickReport::default();

        // `query_recent` returns newest first. Walk it newest→oldest
        // and break the moment we hit the previous watermark — every
        // row before that point has already been considered. We then
        // process the collected slice in chronological order (oldest
        // first) so any future scorer that depends on prior-row state
        // sees them in the order they happened.
        let mut watermark_guard = self.last_seen_obs_id.lock().await;
        let stop_at = *watermark_guard;
        let mut new_slice: Vec<Observation> = Vec::with_capacity(rows.len());
        for obs in rows {
            if Some(obs.id) == stop_at {
                break;
            }
            new_slice.push(obs);
        }

        // Newest id in the slice → the new high-water mark. Captured
        // BEFORE we reverse so it's the first element of `new_slice`.
        let new_watermark = new_slice.first().map(|o| o.id);
        new_slice.reverse();

        for obs in &new_slice {
            let Some(score) = score_observation(&obs.kind) else {
                continue;
            };
            report.considered += 1;

            if score < self.threshold {
                report.skipped += 1;
                continue;
            }

            match self.cognee.add_observation(obs).await {
                Ok(()) => report.promoted += 1,
                Err(e) => report.errors.push(format!("{}: {e}", obs.id)),
            }
        }

        // Only advance the watermark if we actually walked some new
        // rows. If the entire slice was already-seen, leave it alone.
        if let Some(id) = new_watermark {
            *watermark_guard = Some(id);
        }
        Ok(report)
    }

    /// Run the ticker until `shutdown` is cancelled. Sleeps `interval`
    /// between ticks (the first tick fires immediately).
    ///
    /// Errors from individual ticks are logged and swallowed — the
    /// loop only returns when shutdown is signalled.
    ///
    /// # Errors
    /// Currently infallible past the first dispatch — kept as a
    /// `Result` for API symmetry with future variants that may surface
    /// fatal setup failures.
    pub async fn run(
        self: Arc<Self>,
        interval: Duration,
        shutdown: CancellationToken,
    ) -> Result<()> {
        loop {
            if shutdown.is_cancelled() {
                tracing::debug!("cognee promotion ticker: shutdown signalled");
                return Ok(());
            }
            match self.tick().await {
                Ok(report) => {
                    if report.promoted > 0 || !report.errors.is_empty() {
                        tracing::info!(
                            considered = report.considered,
                            promoted = report.promoted,
                            skipped = report.skipped,
                            errors = report.errors.len(),
                            "cognee promotion tick"
                        );
                    } else {
                        tracing::debug!(
                            considered = report.considered,
                            skipped = report.skipped,
                            "cognee promotion tick (idle)"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "cognee promotion tick: log query failed");
                }
            }
            tokio::select! {
                () = shutdown.cancelled() => return Ok(()),
                () = tokio::time::sleep(interval) => {}
            }
        }
    }
}

// ── scoring (pure fn — easy to test) ────────────────────────────────

/// Score one observation by its kind alone (Phase 4 keeps it dumb;
/// Phase 5 will take the full [`Observation`] so it can see metadata
/// and correlation).
///
/// Returns `None` for kinds we filter out entirely (`DaemonShutdown`);
/// returns `Some(score)` otherwise. Score is in `0.0..=1.0`. Callers
/// compare against [`DEFAULT_THRESHOLD`] (or their configured value).
#[must_use]
pub fn score_observation(kind: &ObservationKind) -> Option<f32> {
    use evy_core::WorkerStatus;
    match kind {
        ObservationKind::DaemonShutdown { .. } => None,
        ObservationKind::OperatorMessage { .. } => Some(SCORE_OPERATOR_MESSAGE),
        ObservationKind::WorkerCompleted {
            status: WorkerStatus::Succeeded,
            ..
        } => Some(SCORE_WORKER_SUCCEEDED),
        ObservationKind::PolicyChecked { outcome, .. } if is_allow_outcome(outcome) => {
            Some(SCORE_POLICY_ALLOW)
        }
        _ => Some(SCORE_DEFAULT),
    }
}

/// Lenient match for "this policy decision was an allow".
///
/// v4 has not yet ported the call sites that emit `PolicyChecked`
/// rows, so the canonical outcome string isn't pinned. Pending that
/// port we accept the obvious lower-case forms — `"allow"`, `"allowed"`,
/// case-insensitive. If Phase 5 lands a typed `PolicyOutcome` enum
/// this helper becomes a single `matches!` arm.
fn is_allow_outcome(outcome: &str) -> bool {
    let trimmed = outcome.trim();
    trimmed.eq_ignore_ascii_case("allow") || trimmed.eq_ignore_ascii_case("allowed")
}

// ── render helper (test-only at the moment) ─────────────────────────

/// Decide whether `score` clears the promotion threshold. Pulled out
/// so it's easy to assert against directly in tests without standing
/// up a full ticker.
#[must_use]
#[allow(dead_code)] // public would-be helper; gated to silence unused warning.
pub fn promotes(score: f32, threshold: f32) -> bool {
    score >= threshold
}

// Silence unused-import lint when only the doc-link `Observation` is
// referenced through the module docs.
#[allow(dead_code)]
fn _doc_link_anchor(_o: Observation) {}

#[cfg(test)]
mod tests {
    use super::*;
    use evy_core::{MandateId, ProviderKind, WorkerId, WorkerStatus};

    // ── parameterised score table ──────────────────────────────────

    #[test]
    fn score_table_matches_spec() {
        // (kind, expected score, expected promote at 0.7)
        let cases: Vec<(ObservationKind, Option<f32>, bool)> = vec![
            (
                ObservationKind::OperatorMessage {
                    channel: "telegram".into(),
                    text: "hi".into(),
                },
                Some(SCORE_OPERATOR_MESSAGE),
                true,
            ),
            (
                ObservationKind::WorkerCompleted {
                    worker_id: WorkerId::new(),
                    status: WorkerStatus::Succeeded,
                },
                Some(SCORE_WORKER_SUCCEEDED),
                true,
            ),
            (
                ObservationKind::WorkerCompleted {
                    worker_id: WorkerId::new(),
                    status: WorkerStatus::Failed("oom".into()),
                },
                Some(SCORE_DEFAULT),
                false,
            ),
            (
                ObservationKind::PolicyChecked {
                    command: "ls".into(),
                    outcome: "allow".into(),
                },
                Some(SCORE_POLICY_ALLOW),
                false,
            ),
            (
                ObservationKind::PolicyChecked {
                    command: "ls".into(),
                    outcome: "ALLOWED".into(),
                },
                Some(SCORE_POLICY_ALLOW),
                false,
            ),
            (
                ObservationKind::PolicyChecked {
                    command: "rm -rf /".into(),
                    outcome: "blocked: sealed".into(),
                },
                Some(SCORE_DEFAULT),
                false,
            ),
            (
                ObservationKind::WorkerDispatched {
                    worker_id: WorkerId::new(),
                    mandate_id: MandateId::new(),
                    provider: ProviderKind::ClaudeCode,
                },
                Some(SCORE_DEFAULT),
                false,
            ),
            (
                ObservationKind::SchedulerFiredJob {
                    job_name: "rotate".into(),
                    outcome: "ok".into(),
                },
                Some(SCORE_DEFAULT),
                false,
            ),
            (
                ObservationKind::DaemonBooted {
                    version: "0.1.0".into(),
                },
                Some(SCORE_DEFAULT),
                false,
            ),
            (
                ObservationKind::DaemonShutdown {
                    reason: "sigterm".into(),
                },
                None,
                false,
            ),
            (
                ObservationKind::FeedbackReceived {
                    feedback_id: Uuid::new_v4(),
                    feedback_kind: "approved".into(),
                },
                Some(SCORE_DEFAULT),
                false,
            ),
        ];
        for (kind, expected_score, expected_promote) in cases {
            let actual = score_observation(&kind);
            assert_eq!(
                actual,
                expected_score,
                "score mismatch for kind `{}`",
                kind.discriminator()
            );
            if let Some(s) = actual {
                assert_eq!(
                    promotes(s, DEFAULT_THRESHOLD),
                    expected_promote,
                    "promote mismatch for kind `{}` (score {s})",
                    kind.discriminator()
                );
            }
        }
    }

    #[test]
    fn allow_outcome_match_is_case_and_whitespace_lenient() {
        assert!(is_allow_outcome("allow"));
        assert!(is_allow_outcome("ALLOWED"));
        assert!(is_allow_outcome("  Allow  "));
        assert!(!is_allow_outcome("denied"));
        assert!(!is_allow_outcome("allow-with-suffix"));
    }

    #[test]
    fn default_threshold_promotes_operator_and_succeeded_only() {
        // Belt-and-braces: pin the exact "Phase 4 promotes these two"
        // contract from the module docs. Const-block asserts so the
        // contract is enforced at compile time too — drift the
        // constants and the crate stops building.
        const {
            assert!(SCORE_OPERATOR_MESSAGE >= DEFAULT_THRESHOLD);
            assert!(SCORE_WORKER_SUCCEEDED >= DEFAULT_THRESHOLD);
            assert!(SCORE_POLICY_ALLOW < DEFAULT_THRESHOLD);
            assert!(SCORE_DEFAULT < DEFAULT_THRESHOLD);
        }
    }
}
