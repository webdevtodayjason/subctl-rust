//! Error helpers for `evy-watchdog`.
//!
//! The public surface returns [`evy_core::Result`] so watchdog errors are
//! interchangeable with the rest of the workspace. This module exists for
//! conversions specific to watchdog plumbing (timeouts, tmux helpers) and
//! to keep the crate's failure points named.
//!
//! Watchdog ticks must not propagate failures into the daemon. The
//! registry catches per-tick errors and folds them into an unhealthy
//! [`TickReport`](crate::report::TickReport) — see [`registry`](crate::registry).

use evy_core::Error;

/// Build an [`Error::Provider`] tagged with a synthetic-but-stable kind.
///
/// Watchdogs surface tmux / I/O / sub-process failures through
/// `Error::Provider` to keep one error surface across the workspace.
/// Since watchdogs are provider-agnostic, the variant uses
/// [`evy_core::ProviderKind::ClaudeCode`] as the carrier — the watchdog
/// name is embedded in `reason` so log readers don't lose context.
pub(crate) fn watchdog_io_error(watchdog: &str, reason: impl Into<String>) -> Error {
    Error::Provider {
        kind: evy_core::ProviderKind::ClaudeCode,
        reason: format!("[watchdog {watchdog}] {}", reason.into()),
    }
}
