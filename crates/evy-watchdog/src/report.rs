//! `TickReport` + `Finding` — the structured output of one watchdog tick.
//!
//! Each [`crate::Watchdog::tick`] call returns exactly one
//! [`TickReport`]. The registry collects them per loop iteration, fans
//! them out as [`evy_comms::DaemonEvent::WatchdogTick`] events, and
//! appends a corresponding [`evy_memory::Observation`] for the
//! long-term log.
//!
//! Findings are intentionally small and additive — new variants can be
//! added without breaking existing watchdogs or readers.

use chrono::{DateTime, Utc};
use evy_core::WorkerId;
use serde::{Deserialize, Serialize};

/// A single thing a watchdog noticed while ticking.
///
/// Variants are tagged `"finding"` (`snake_case`) when serialised so the
/// dashboard / SSE consumer can discriminate without knowing the Rust
/// enum layout.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "finding", rename_all = "snake_case")]
pub enum Finding {
    /// A tmux pane has produced no output for at least `idle_secs`.
    IdlePane {
        /// `<session>:<window>` identifier — opaque string for portability.
        pane: String,
        /// Seconds since the watchdog last saw the pane's content change.
        idle_secs: u64,
    },

    /// A registered team no longer maps to a live tmux session, or the
    /// team's last activity exceeded the staleness threshold.
    DeadTeam {
        /// Stable team id (matches the v3 team registry key).
        team: String,
        /// Why the watchdog concluded the team is dead.
        reason: String,
    },

    /// A defensive sweep removed a reference to a vanished tmux session.
    PrunedSession {
        /// Session name that was removed from in-memory tracking.
        session: String,
    },

    /// The terminal-worker reap (W6 row ⑨) retired a finished worker's
    /// registry row after the grace window. The hosting tmux session is
    /// left alone — only the bookkeeping retires.
    ReapedWorker {
        /// Worker whose registry row was retired.
        worker_id: WorkerId,
        /// The session that hosted it, when one was recorded.
        session: Option<String>,
    },

    /// Auto-nudge dispatched a status-check directive to a stuck worker.
    /// `attempts` is the running count for *this* worker since it last
    /// produced output — escalation to [`Finding::WorkerDead`] fires when
    /// `attempts` reaches the configured threshold.
    WorkerNudged {
        /// Worker that received the nudge.
        worker_id: WorkerId,
        /// Cumulative nudge count for this worker in the current
        /// stale window. Resets to zero on any observed output change.
        attempts: u32,
    },

    /// Auto-nudge escalated a worker that exhausted its nudge budget
    /// without producing fresh output. Terminal for this worker — the
    /// watchdog drops it from its nudge history.
    WorkerDead {
        /// Worker that exhausted its nudge budget.
        worker_id: WorkerId,
        /// Timestamp of the worker's last observed activity, surfaced
        /// in the operator notification so the alert body shows
        /// "silent for N min".
        last_activity: DateTime<Utc>,
    },

    /// Team-staleness flagged a registered team whose most-recent
    /// activity exceeded the staleness threshold. Distinct from
    /// [`Finding::DeadTeam`] — `StaleTeam` keeps the team in the
    /// registry; it's the operator-facing "I dispatched a team but
    /// nothing's happening" canary.
    StaleTeam {
        /// Team name (`TeamRecord.team_id` from the registry).
        team_name: String,
        /// Last activity timestamp from the team registry. Absent
        /// `last_activity` is folded to the team's effective "born at"
        /// before reaching this finding.
        last_activity: DateTime<Utc>,
    },

    /// No problem found this tick. Emitted instead of an empty findings
    /// vec so the observation log records that the watchdog *did* run.
    Healthy,
}

/// The result of one [`crate::Watchdog::tick`] call.
///
/// `healthy` is independent of `findings.is_empty()`: a watchdog can
/// flag interesting events (e.g. `PrunedSession` on routine cleanup)
/// without considering itself unhealthy. Set `healthy = false` only
/// when the watchdog itself encountered an internal failure
/// (timeout, broken I/O); operator-facing findings stay separately
/// signalled in `findings`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TickReport {
    /// Name of the watchdog that produced this report.
    pub watchdog: String,
    /// UTC timestamp at which the tick began.
    pub ran_at: DateTime<Utc>,
    /// Findings, in detection order.
    pub findings: Vec<Finding>,
    /// True iff the watchdog itself ran cleanly. Findings can still
    /// be non-empty when `healthy` is true.
    pub healthy: bool,
}

impl TickReport {
    /// Convenience: a healthy report with no findings beyond `Healthy`.
    #[must_use]
    pub fn healthy(watchdog: impl Into<String>) -> Self {
        Self {
            watchdog: watchdog.into(),
            ran_at: Utc::now(),
            findings: vec![Finding::Healthy],
            healthy: true,
        }
    }

    /// Build a report with explicit findings. Healthy is inferred from
    /// the caller; use [`TickReport::unhealthy`] for the failure path.
    #[must_use]
    pub fn with_findings(watchdog: impl Into<String>, findings: Vec<Finding>) -> Self {
        Self {
            watchdog: watchdog.into(),
            ran_at: Utc::now(),
            findings,
            healthy: true,
        }
    }

    /// Build a report flagged unhealthy — the watchdog itself failed.
    /// The `reason` is folded into a `DeadTeam`-style finding so the
    /// log carries the error verbatim.
    #[must_use]
    pub fn unhealthy(watchdog: impl Into<String>, reason: impl Into<String>) -> Self {
        let watchdog = watchdog.into();
        Self {
            findings: vec![Finding::DeadTeam {
                team: watchdog.clone(),
                reason: reason.into(),
            }],
            watchdog,
            ran_at: Utc::now(),
            healthy: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finding_roundtrips_via_json() {
        let cases = vec![
            Finding::IdlePane {
                pane: "claude-team:0".into(),
                idle_secs: 600,
            },
            Finding::DeadTeam {
                team: "stale-team".into(),
                reason: "tmux session gone".into(),
            },
            Finding::PrunedSession {
                session: "ghost-session".into(),
            },
            Finding::WorkerNudged {
                worker_id: WorkerId::new(),
                attempts: 2,
            },
            Finding::WorkerDead {
                worker_id: WorkerId::new(),
                last_activity: Utc::now(),
            },
            Finding::StaleTeam {
                team_name: "claude-quiet".into(),
                last_activity: Utc::now(),
            },
            Finding::Healthy,
        ];
        for f in cases {
            let s = serde_json::to_string(&f).expect("serialize");
            let back: Finding = serde_json::from_str(&s).expect("deserialize");
            assert_eq!(f, back);
        }
    }

    #[test]
    fn phase5_finding_tags_are_snake_case() {
        // Dashboard / SSE consumers match on the `"finding"` tag string;
        // lock the spelling in so a rename can't break readers silently.
        let nudged = Finding::WorkerNudged {
            worker_id: WorkerId::new(),
            attempts: 1,
        };
        let dead = Finding::WorkerDead {
            worker_id: WorkerId::new(),
            last_activity: Utc::now(),
        };
        let stale = Finding::StaleTeam {
            team_name: "claude-quiet".into(),
            last_activity: Utc::now(),
        };
        assert!(serde_json::to_string(&nudged)
            .unwrap()
            .contains("\"finding\":\"worker_nudged\""),);
        assert!(serde_json::to_string(&dead)
            .unwrap()
            .contains("\"finding\":\"worker_dead\""),);
        assert!(serde_json::to_string(&stale)
            .unwrap()
            .contains("\"finding\":\"stale_team\""),);
    }

    #[test]
    fn finding_tag_is_snake_case() {
        let f = Finding::IdlePane {
            pane: "p".into(),
            idle_secs: 1,
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("\"finding\":\"idle_pane\""), "got {s}");
    }

    #[test]
    fn healthy_constructor_marks_healthy_true() {
        let r = TickReport::healthy("idle-pane");
        assert!(r.healthy);
        assert_eq!(r.findings, vec![Finding::Healthy]);
        assert_eq!(r.watchdog, "idle-pane");
    }

    #[test]
    fn unhealthy_constructor_marks_healthy_false() {
        let r = TickReport::unhealthy("idle-pane", "timeout");
        assert!(!r.healthy);
        assert_eq!(r.findings.len(), 1);
    }

    #[test]
    fn report_roundtrips_via_json() {
        let r = TickReport {
            watchdog: "idle-pane".into(),
            ran_at: Utc::now(),
            findings: vec![Finding::IdlePane {
                pane: "claude-test:0".into(),
                idle_secs: 300,
            }],
            healthy: true,
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: TickReport = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }
}
