//! `evy-watchdog` — periodic-check framework + Phase-4 watchdog impls.
//!
//! Ports v3's watchdog machinery to v4 as a unified framework:
//!
//! - [`Watchdog`] trait — `name() + schedule() + tick()`. Each impl is
//!   responsible for one slice of "is the daemon's view of the world
//!   still consistent?"
//! - [`WatchdogRegistry`] — holds N watchdogs, runs a single tokio task
//!   that ticks them on schedule, fans the [`TickReport`]s out as
//!   [`evy_comms::DaemonEvent::WatchdogTick`] events, appends each tick
//!   to the [`evy_memory::ObservationLog`].
//!
//! # Shipped watchdog impls (this crate)
//!
//! | Watchdog | Detects | Phase |
//! |---|---|---|
//! | [`IdlePaneWatchdog`] | tmux panes whose content hasn't changed for N seconds | 4 |
//! | [`TeamGcWatchdog`] | team registrations whose tmux session is dead AND last-activity is stale | 4 |
//! | [`WatchdogPrune`] | defensive sweep removing any team whose tmux session is missing | 4 |
//! | [`AutoNudgeWatchdog`] | stuck workers — dispatches gentle status-check directives + escalates to [`Finding::WorkerDead`] after N unanswered nudges | 5 |
//! | [`TeamStalenessWatchdog`] | the "I dispatched a team but nothing's happening" canary — emits [`Finding::StaleTeam`] without mutating the registry | 5 |
//!
//! # Slice boundaries
//!
//! Phase 5 closes the v3 production bug
//! `2026-05-18-stale-team-watchdog-alerts.md` by splitting v3's
//! `team-staleness` across three crate-local watchdogs:
//!
//! - `AutoNudgeWatchdog` owns the **worker-level** nudge state
//!   machine and the only mutating tmux path
//!   (via [`directive::DirectiveDispatcher`]).
//! - `TeamGcWatchdog` owns "team's session is gone AND activity is
//!   stale" — removes the team from the registry.
//! - `TeamStalenessWatchdog` owns "team is alive but quiet" —
//!   observation only, never deletes, never fires when the session is
//!   already dead. This is the regression sentinel.
//!
//! # Phase 6 deferrals
//!
//! - **Reply classification** — port v3's `classifyWorkerReply`
//!   (completed_idle / awaiting_input / blocked) so AutoNudge skips
//!   workers that explicitly self-reported done/awaiting input.
//! - **`Provider::list_workers()`** — AutoNudge currently mints a
//!   stable `WorkerId` per pane target on first sight; once the
//!   provider trait exposes a real enumeration that mapping
//!   collapses into a direct lookup.
//! - **Cron-scheduled watchdogs** — [`WatchdogSchedule::Cron`] is
//!   still accepted but inert; the registry needs to share the
//!   scheduler's cron evaluator.
//! - **On-disk team-dir archival** — v3's `team-gc.ts` archived the
//!   `policy.snapshot.toml` dir; v4 has no on-disk team dir yet.
//! - **Nudge-history persistence** — current state is in-process;
//!   a daemon restart resets the escalation clock.
//!
//! # Quick start
//!
//! ```no_run
//! use std::sync::Arc;
//! use evy_watchdog::{
//!     IdlePaneWatchdog, WatchdogRegistry, WatchdogContext, RealTmuxQuery,
//! };
//! use tokio_util::sync::CancellationToken;
//!
//! # async fn demo(ctx: WatchdogContext) -> evy_core::Result<()> {
//! let tmux = Arc::new(RealTmuxQuery);
//! let mut reg = WatchdogRegistry::new();
//! reg.add(Arc::new(IdlePaneWatchdog::new(tmux)));
//! let reg = Arc::new(reg);
//! let shutdown = CancellationToken::new();
//! reg.clone().start(ctx, shutdown.clone()).await?;
//! // ... daemon runs ...
//! shutdown.cancel();
//! reg.join().await;
//! # Ok(()) }
//! ```

#![warn(missing_docs)]

pub mod auto_nudge;
pub mod directive;
mod error;
pub mod idle_pane;
pub mod prune;
pub mod registry;
pub mod report;
pub mod team_gc;
pub mod team_registry;
pub mod team_staleness;
pub mod tmux_query;
pub mod trait_def;

#[cfg(test)]
mod test_support;

pub use auto_nudge::{AutoNudgeConfig, AutoNudgeWatchdog};
pub use directive::{DirectiveDispatcher, MockDirectiveDispatcher};
pub use idle_pane::IdlePaneWatchdog;
pub use prune::WatchdogPrune;
pub use registry::WatchdogRegistry;
pub use report::{Finding, TickReport};
pub use team_gc::TeamGcWatchdog;
pub use team_registry::{InMemoryTeamRegistry, TeamRecord, TeamRegistry};
pub use team_staleness::{TeamStalenessConfig, TeamStalenessWatchdog};
pub use tmux_query::{MockTmuxQuery, RealTmuxQuery, TmuxQuery};
pub use trait_def::{Watchdog, WatchdogContext, WatchdogSchedule};
