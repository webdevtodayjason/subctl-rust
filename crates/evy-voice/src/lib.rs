//! `evy-voice` — voice layer for Evy v4.
//!
//! Phase 4 Slice B port of v3's voice stack
//! (`components/evy/voice-config.ts` + `voice-render.ts`). The crate is
//! a **TTS client**, not a server — the TTS server itself
//! (`com.subctl.tts`, port `:8789`) is a separate launchd service and
//! v4 just consumes it.
//!
//! ## Pieces
//!
//! - [`VoiceConfig`] / [`VoiceConfigStore`] — JSON-backed config at
//!   `~/.config/subctl/voice.json` with a live `tokio::sync::watch`
//!   broadcast channel. File-watcher reloads on external edit with a
//!   200ms debounce — operator can flip `enabled` from the dashboard,
//!   CLI, or `$EDITOR` without a daemon restart.
//! - [`TtsClient`] — `reqwest` client targeting the local TTS server.
//!   `synthesize(text, voice_id, model) -> Vec<u8>`. `rustls-tls` so
//!   we stay off the OpenSSL footgun.
//! - [`VoiceRenderer`] — orchestrates config + client + cache + egress
//!   redaction. Caches rendered audio at
//!   `<cache_dir>/<fingerprint>.wav` with a 24h TTL.
//! - [`check_egress`] — refusal-on-match egress floor. The renderer
//!   calls this on every input; tests can also call it directly.
//!
//! ## Disabled by default
//!
//! `VoiceConfig::default()` ships with `enabled: false`. The renderer
//! refuses every call with [`VoiceError::Disabled`] until the operator
//! flips the flag (CLI / dashboard / direct edit). This matches v3's
//! "operator must opt in" policy and avoids surprise TTS spam if the
//! TTS server happens to be running.
//!
//! ## Wiring (preview for evy-comms / dashboard)
//!
//! ```no_run
//! use std::sync::Arc;
//! use evy_voice::{
//!     default_path, TtsClient, VoiceConfigStore, VoiceRenderer,
//! };
//!
//! # async fn demo() -> evy_voice::Result<()> {
//! let store = Arc::new(VoiceConfigStore::open(&default_path()).await?);
//! let cfg = store.current();
//! let client = Arc::new(TtsClient::new(cfg.tts_server.clone()));
//! let cache_dir = std::env::var("HOME")
//!     .map(|h| std::path::PathBuf::from(h).join(".local/state/subctl/voice/cache"))
//!     .unwrap_or_else(|_| std::path::PathBuf::from("./voice-cache"));
//! let renderer = VoiceRenderer::new(store, client, cache_dir);
//!
//! // Render → returns the audio URL (and on-disk path for bridge uploads).
//! let out = renderer.render("hello jason", None).await?;
//! println!("audio at {} (cached={})", out.audio_path.display(), out.cached);
//! # Ok(()) }
//! ```

#![warn(missing_docs)]

pub mod client;
pub mod config;
pub mod error;
pub mod redact;
pub mod renderer;

// ── Public re-exports — the surface the daemon binary and evy-comms
//    bridge consume. ────────────────────────────────────────────────────

pub use client::{TtsClient, TtsHealth};
pub use config::{
    default_path, load_from_path, SharedVoiceConfigStore, VoiceConfig, VoiceConfigStore,
};
pub use error::{Result, VoiceError};
pub use redact::check_egress;
pub use renderer::{fingerprint, RenderResult, VoiceRenderer, VoiceStatus, MAX_TEXT_CHARS};
