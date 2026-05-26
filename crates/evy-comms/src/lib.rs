//! evy-comms — multi-channel router. HTTP/SSE for the web client, a
//! ratatui-style TUI (potentially shipped as a separate binary later),
//! plus Discord and Telegram adapters.
//!
//! Phase 2 Slice 2B2: Telegram bridge. Subsequent slices fill in the
//! remaining channels. See ADR 0020 for the architectural context.

// ── Slice 2B2: Telegram ──────────────────────────────────────────────
pub mod ask;
pub mod notification;
pub mod telegram;

pub use ask::{Ask, AskId, AskRegistry};
pub use notification::Notification;
pub use telegram::{InboundMessage, TelegramBridge, TelegramConfig};
