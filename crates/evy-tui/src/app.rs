//! Application state — the model the run loop mutates and the UI
//! reads. Pure data + transition methods; no I/O.
//!
//! Keeping all I/O out of [`App`] makes the state machine cheap to
//! unit-test: tests construct an `App`, push keyboard / daemon events
//! into the transition methods, and assert on field values without
//! booting a terminal or wiremock server.

use std::collections::VecDeque;

use crate::api::{DaemonEvent, JobSummary, PolicyView, WorkerSummary};

/// Maximum number of events retained in the scrolling log. Older
/// events drop off the back; the operator can see ~5–10 minutes of
/// activity on a typical event rate.
pub const EVENT_LOG_CAPACITY: usize = 200;

/// The four navigable tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    /// `GET /api/evy/workers` + live SSE updates.
    #[default]
    Workers,
    /// `GET /api/evy/scheduler/jobs`.
    Scheduler,
    /// Scrolling SSE event log.
    Events,
    /// `GET /api/evy/policy`.
    Policy,
}

impl Tab {
    /// Ordered list of tabs as the tab bar paints them.
    pub const ALL: [Tab; 4] = [Self::Workers, Self::Scheduler, Self::Events, Self::Policy];

    /// Stable display label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Workers => "Workers",
            Self::Scheduler => "Scheduler",
            Self::Events => "Events",
            Self::Policy => "Policy",
        }
    }

    /// Index of this tab in [`Self::ALL`].
    #[must_use]
    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|&t| t == self)
            .unwrap_or_default()
    }

    /// The tab to the right; wraps.
    #[must_use]
    pub fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    /// The tab to the left; wraps.
    #[must_use]
    pub fn prev(self) -> Self {
        let n = Self::ALL.len();
        Self::ALL[(self.index() + n - 1) % n]
    }
}

/// Lifecycle of the SSE connection. Drives the bottom status bar.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnectionState {
    /// Initial state; never connected yet.
    #[default]
    Connecting,
    /// SSE stream is live.
    Live,
    /// Stream dropped; we're between reconnect attempts.
    Disconnected {
        /// Last error string surfaced to the operator.
        reason: String,
    },
}

impl ConnectionState {
    /// Short fixed-width-ish label for the status bar.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Connecting => "connecting…".to_owned(),
            Self::Live => "live".to_owned(),
            Self::Disconnected { reason } => format!("disconnected: {reason}"),
        }
    }
}

/// Top-level application state.
///
/// Owns: which tab is active, per-tab selection cursors, the cached
/// snapshots, the rolling event log, and the connection status bar.
#[derive(Debug, Clone, Default)]
pub struct App {
    /// Currently-painted tab.
    pub tab: Tab,
    /// Row cursor for the workers table.
    pub workers_cursor: usize,
    /// Row cursor for the jobs table.
    pub jobs_cursor: usize,
    /// Row cursor for the events log (0 = newest).
    pub events_cursor: usize,
    /// Last-fetched workers snapshot.
    pub workers: Vec<WorkerSummary>,
    /// Last-fetched jobs snapshot.
    pub jobs: Vec<JobSummary>,
    /// Last-fetched policy snapshot.
    pub policy: Option<PolicyView>,
    /// Rolling event log. Newest at the front (`push_front`), oldest
    /// dropped from the back when capacity is exceeded.
    pub events: VecDeque<DaemonEvent>,
    /// Most recent connection state to the SSE stream.
    pub connection: ConnectionState,
    /// Set true when `Quit` has been requested. The run loop exits at
    /// the top of its next iteration.
    pub should_quit: bool,
}

impl App {
    /// Construct an empty App on the Workers tab.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Cycle to the next tab (wraps). Resets the previous tab's
    /// selection cursor to 0 to avoid a surprising scroll position
    /// when the operator comes back.
    pub fn cycle_tab_forward(&mut self) {
        self.tab = self.tab.next();
    }

    /// Cycle to the previous tab (wraps).
    pub fn cycle_tab_backward(&mut self) {
        self.tab = self.tab.prev();
    }

    /// Move the selection cursor down within the active tab. Clamped
    /// at the last row.
    pub fn select_next(&mut self) {
        match self.tab {
            Tab::Workers => {
                let max = self.workers.len().saturating_sub(1);
                self.workers_cursor = (self.workers_cursor + 1).min(max);
            }
            Tab::Scheduler => {
                let max = self.jobs.len().saturating_sub(1);
                self.jobs_cursor = (self.jobs_cursor + 1).min(max);
            }
            Tab::Events => {
                let max = self.events.len().saturating_sub(1);
                self.events_cursor = (self.events_cursor + 1).min(max);
            }
            Tab::Policy => {} // policy is a tree, no row cursor
        }
    }

    /// Move the selection cursor up within the active tab. Clamped at
    /// row 0.
    pub fn select_prev(&mut self) {
        match self.tab {
            Tab::Workers => {
                self.workers_cursor = self.workers_cursor.saturating_sub(1);
            }
            Tab::Scheduler => {
                self.jobs_cursor = self.jobs_cursor.saturating_sub(1);
            }
            Tab::Events => {
                self.events_cursor = self.events_cursor.saturating_sub(1);
            }
            Tab::Policy => {}
        }
    }

    /// Request a clean exit.
    pub fn request_quit(&mut self) {
        self.should_quit = true;
    }

    /// Replace the workers snapshot. Clamps the cursor to the new
    /// length so a shorter list doesn't leave the cursor past the end.
    pub fn set_workers(&mut self, workers: Vec<WorkerSummary>) {
        let max = workers.len().saturating_sub(1);
        self.workers_cursor = self.workers_cursor.min(max);
        self.workers = workers;
    }

    /// Replace the jobs snapshot.
    pub fn set_jobs(&mut self, jobs: Vec<JobSummary>) {
        let max = jobs.len().saturating_sub(1);
        self.jobs_cursor = self.jobs_cursor.min(max);
        self.jobs = jobs;
    }

    /// Replace the policy snapshot.
    pub fn set_policy(&mut self, policy: PolicyView) {
        self.policy = Some(policy);
    }

    /// Update the connection state badge.
    pub fn set_connection(&mut self, state: ConnectionState) {
        self.connection = state;
    }

    /// Push a daemon event to the front of the log; drops the oldest
    /// when capacity is exceeded. Also folds the event into the
    /// workers snapshot for status changes so the Workers tab stays
    /// current between manual refreshes.
    pub fn push_event(&mut self, event: DaemonEvent) {
        // Reflect status changes into the cached workers list — the
        // operator's expectation is "I see status update live, even on
        // the Workers tab without pressing 'r'".
        if let DaemonEvent::WorkerStatusChanged { worker_id, status } = &event {
            for w in &mut self.workers {
                if w.id == *worker_id {
                    w.status = status.clone();
                    break;
                }
            }
        }

        self.events.push_front(event);
        while self.events.len() > EVENT_LOG_CAPACITY {
            self.events.pop_back();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evy_core::{MandateId, ProviderKind, WorkerId, WorkerStatus};

    fn worker(id: WorkerId, status: WorkerStatus) -> WorkerSummary {
        WorkerSummary {
            id,
            provider: ProviderKind::ClaudeCode,
            mandate_id: MandateId::new(),
            status,
        }
    }

    #[test]
    fn tab_cycles_forward_through_all_then_wraps() {
        let mut app = App::new();
        assert_eq!(app.tab, Tab::Workers);
        app.cycle_tab_forward();
        assert_eq!(app.tab, Tab::Scheduler);
        app.cycle_tab_forward();
        assert_eq!(app.tab, Tab::Events);
        app.cycle_tab_forward();
        assert_eq!(app.tab, Tab::Policy);
        app.cycle_tab_forward();
        assert_eq!(app.tab, Tab::Workers);
    }

    #[test]
    fn tab_cycles_backward_wraps() {
        let mut app = App::new();
        app.cycle_tab_backward();
        assert_eq!(app.tab, Tab::Policy);
    }

    #[test]
    fn select_next_on_empty_workers_stays_at_zero() {
        let mut app = App::new();
        app.select_next();
        assert_eq!(app.workers_cursor, 0);
    }

    #[test]
    fn select_next_clamps_to_last_row() {
        let mut app = App::new();
        app.workers = vec![
            worker(WorkerId::new(), WorkerStatus::Pending),
            worker(WorkerId::new(), WorkerStatus::Running),
        ];
        app.select_next();
        assert_eq!(app.workers_cursor, 1);
        app.select_next();
        assert_eq!(app.workers_cursor, 1, "clamped at last row");
    }

    #[test]
    fn select_prev_clamps_at_zero() {
        let mut app = App::new();
        app.select_prev();
        assert_eq!(app.workers_cursor, 0);
    }

    #[test]
    fn push_event_grows_then_caps_at_capacity() {
        let mut app = App::new();
        for _ in 0..EVENT_LOG_CAPACITY + 50 {
            app.push_event(DaemonEvent::Heartbeat {
                ts: chrono::Utc::now(),
                providers_healthy: 1,
            });
        }
        assert_eq!(app.events.len(), EVENT_LOG_CAPACITY);
    }

    #[test]
    fn worker_status_change_event_updates_workers_snapshot() {
        let mut app = App::new();
        let wid = WorkerId::new();
        app.set_workers(vec![worker(wid, WorkerStatus::Pending)]);
        app.push_event(DaemonEvent::WorkerStatusChanged {
            worker_id: wid,
            status: WorkerStatus::Running,
        });
        assert_eq!(app.workers[0].status, WorkerStatus::Running);
    }

    #[test]
    fn set_workers_clamps_cursor_to_new_length() {
        let mut app = App::new();
        app.set_workers(vec![
            worker(WorkerId::new(), WorkerStatus::Running),
            worker(WorkerId::new(), WorkerStatus::Running),
            worker(WorkerId::new(), WorkerStatus::Running),
        ]);
        app.workers_cursor = 2;
        // Shrink the list.
        app.set_workers(vec![worker(WorkerId::new(), WorkerStatus::Running)]);
        assert_eq!(app.workers_cursor, 0);
    }

    #[test]
    fn request_quit_sets_flag() {
        let mut app = App::new();
        assert!(!app.should_quit);
        app.request_quit();
        assert!(app.should_quit);
    }

    #[test]
    fn connection_state_label_is_human_readable() {
        assert_eq!(ConnectionState::Connecting.label(), "connecting…");
        assert_eq!(ConnectionState::Live.label(), "live");
        let lbl = ConnectionState::Disconnected {
            reason: "timeout".into(),
        }
        .label();
        assert!(lbl.contains("timeout"));
    }
}
