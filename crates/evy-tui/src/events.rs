//! Merged event taxonomy for the TUI run loop.
//!
//! The run loop multiplexes three sources over a single
//! `tokio::select!`:
//!
//! 1. **Keyboard input** — `crossterm::event::EventStream` emits
//!    raw terminal events; we filter to key presses and forward
//!    them as [`TuiEvent::Key`].
//! 2. **Daemon events** — a background task connects to the SSE
//!    endpoint, parses each `data:` frame as a [`crate::api::DaemonEvent`],
//!    and forwards via an mpsc channel as [`TuiEvent::Daemon`].
//! 3. **Tick** — a `tokio::time::interval` produces [`TuiEvent::Tick`]
//!    once a second so the UI can update relative timestamps and
//!    reconnect-backoff counters.
//!
//! Status changes for the SSE connection itself are dispatched as
//! [`TuiEvent::Connection`] so the run loop can update the status bar
//! without conflating "the daemon is dead" with "the daemon said
//! something we should display".

use crossterm::event::KeyEvent;

use crate::api::DaemonEvent;
use crate::app::ConnectionState;

/// One unit of work the run loop processes per `select!` iteration.
#[derive(Debug, Clone)]
pub enum TuiEvent {
    /// A key was pressed in the terminal. Mouse events are filtered
    /// out upstream; this slice is keyboard-only.
    Key(KeyEvent),

    /// The daemon emitted an event over the SSE stream.
    Daemon(DaemonEvent),

    /// The SSE connection's lifecycle changed (connecting / live /
    /// disconnected). Drives the bottom status bar; orthogonal to
    /// `DaemonEvent`.
    Connection(ConnectionState),

    /// Periodic tick (~1Hz). Used to refresh relative time displays
    /// and any reconnect-backoff countdowns.
    Tick,

    /// Operator requested a manual refresh of snapshot endpoints
    /// (workers / jobs / policy). Triggered by `r`.
    Refresh,

    /// Quit signal — clean teardown of the alternate screen.
    Quit,
}
