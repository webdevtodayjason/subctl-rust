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
//! # Phase 4 watchdog impls (this crate)
//!
//! | Watchdog | Detects |
//! |---|---|
//! | [`IdlePaneWatchdog`] | tmux panes whose content hasn't changed for N seconds |
//! | [`TeamGcWatchdog`] | team registrations whose tmux session is dead AND last-activity is stale |
//! | [`WatchdogPrune`] | defensive sweep removing any team whose tmux session is missing |
//!
//! # Phase 5 deferrals
//!
//! - **`AutoNudge`** — the directive-composing watchdog that gently
//!   prods stuck workers before escalating. Needs the directive
//!   composition path that hasn't ported from v3 yet. TODO: Phase 5.
//! - **`TeamStaleness`** — escalation watchdog that pages the operator
//!   when a nudged team stays unresponsive. Depends on `AutoNudge`'s
//!   nudge-history. TODO: Phase 5.
//! - **Real tmux-driven worker enumeration** — Phase-4 `IdlePaneWatchdog`
//!   sniffs `claude-*` session names; Phase-5 wants `Provider::list_workers`.
//! - **Cron-scheduled watchdogs** — [`WatchdogSchedule::Cron`] is
//!   accepted at registration time but the registry doesn't fire cron
//!   schedules in Phase 4. TODO: Phase 5.
//! - **On-disk team-dir archival** — v3's `team-gc.ts` archived the
//!   `policy.snapshot.toml` dir. v4 has no on-disk team dir yet (the
//!   per-team HMAC + policy snapshot wiring is Phase 5).
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

mod error;
pub mod idle_pane;
pub mod prune;
pub mod registry;
pub mod report;
pub mod team_gc;
pub mod team_registry;
pub mod tmux_query;
pub mod trait_def;

#[cfg(test)]
mod test_support;

pub use idle_pane::IdlePaneWatchdog;
pub use prune::WatchdogPrune;
pub use registry::WatchdogRegistry;
pub use report::{Finding, TickReport};
pub use team_gc::TeamGcWatchdog;
pub use team_registry::{InMemoryTeamRegistry, TeamRecord, TeamRegistry};
pub use tmux_query::{MockTmuxQuery, RealTmuxQuery, TmuxQuery};
pub use trait_def::{Watchdog, WatchdogContext, WatchdogSchedule};
