//! `evy-chat-tui` — operator-facing terminal chat client for Evy v4.
//!
//! Phase 6. A standalone binary (`evy-chat`) that connects to a running
//! daemon's chat endpoint (`POST /api/evy/chat`) and gives the operator
//! a two-pane chat surface: scrollback up top, multi-line input at the
//! bottom.
//!
//! # Design choices
//!
//! * **Vanilla ratatui + crossterm.** No third-party widget crates;
//!   keeps the dep graph tight and the binary small.
//! * **Non-streaming.** The endpoint is request/response. Streaming is
//!   Phase 7 once `LlmBackend::respond` grows a streaming sibling.
//! * **No external markdown crate.** Bold and fenced-code blocks render
//!   via a hand-rolled tokeniser in [`render`]. This keeps the dep
//!   graph small; the chat output is mostly plain prose anyway.
//! * **Slash commands.** `/quit`, `/help`, `/clear`, `/new-session`.
//!   See [`input::SlashCommand`] for the full enum.
//!
//! See [`crates/evy-chat-tui/HERMES_TUI_NOTES.md`] for the Hermes
//! patterns we ported and the ones we deliberately did NOT.

#![warn(missing_docs)]

pub mod api;
pub mod app;
pub mod input;
pub mod ui;

pub use api::{ApiClient, ApiError, ChatRequest, ChatResponse};
pub use app::{App, ChatLine, LineKind, Status};
pub use input::{handle_key, KeyOutcome, SlashCommand};
pub use ui::render;
