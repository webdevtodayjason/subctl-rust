//! `evy-tui` — ratatui operator console for the Evy v4 daemon.
//!
//! A standalone read-only terminal client that connects to a running
//! daemon over HTTP/SSE (Phase 2 Slice 2B1) and renders four tabs:
//!
//! | Tab       | Source                                  | Shows                          |
//! |-----------|-----------------------------------------|--------------------------------|
//! | Workers   | `GET /api/evy/workers` + live SSE       | registered workers + status    |
//! | Scheduler | `GET /api/evy/scheduler/jobs`           | jobs table + last fire outcome |
//! | Events    | `GET /api/evy/events` (SSE)             | scrolling event log (~200)     |
//! | Policy    | `GET /api/evy/policy`                   | loaded policy as a tree-view   |
//!
//! # Out of scope (Phase 3 Slice A)
//!
//! - **Mouse support** — keyboard navigation only.
//! - **Mutations** — no start/stop/cancel; the operator console is
//!   read-only this slice. Command channels arrive later.
//! - **Embedded daemon mode** — the TUI is a separate binary that
//!   talks to a running daemon over the network surface; it does not
//!   spawn its own daemon.
//!
//! # Public surface (library)
//!
//! The library re-exports the inner types so integration tests and
//! downstream consumers (rare) can drive the state machine without
//! booting a terminal:
//!
//! - [`App`] — owns all UI state, drives transitions
//! - [`ApiClient`] — fetches snapshots + opens the SSE stream
//! - [`Tab`] — the four-tab enum + cycling helpers
//! - [`TuiEvent`] — the merged event taxonomy the run-loop dispatches on
//! - [`DaemonEvent`] / [`WorkerSummary`] / [`JobSummary`] / [`PolicyView`] —
//!   wire-shape duplicates of evy-comms types. Re-declared locally so the
//!   TUI doesn't drag axum + the Telegram bridge into a CLI binary's
//!   dep graph.

#![warn(missing_docs)]

pub mod api;
pub mod app;
pub mod events;
pub mod input;
pub mod ui;

pub use api::{ApiClient, ApiError, DaemonEvent, JobSummary, PolicyView, WorkerSummary};
pub use app::{App, ConnectionState, Tab};
pub use events::TuiEvent;
pub use input::handle_key;
pub use ui::render;
