//! [`VoiceRenderer`] — orchestrates config + TTS client + cache + egress
//! redaction.
//!
//! Each `render()` call:
//!
//! 1. snapshots the live [`VoiceConfig`] from the store (no in-memory
//!    cache; matches v3's "VERSION is the canonical source" rule),
//! 2. refuses early with [`VoiceError::Disabled`] if `enabled` is off,
//!    [`VoiceError::EmptyText`] if the trimmed input is empty,
//!    [`VoiceError::TextTooLong`] if it's above [`MAX_TEXT_CHARS`],
//! 3. runs the input through [`crate::redact::check_egress`] — refuses
//!    on match,
//! 4. computes the cache fingerprint and checks the cache dir; cache
//!    hits skip the network round-trip,
//! 5. calls [`TtsClient::synthesize`] for misses, writes the bytes to
//!    `<cache_dir>/<fingerprint>.wav` and returns the audio URL.
//!
//! ## Cache scheme
//!
//! Fingerprint = `SHA256(text|voice_id|model)` (full 64-char hex). The
//! `|` separator avoids the edge case where one field's trailing bytes
//! happen to match the next field's leading bytes. 24h TTL via mtime
//! check; stale entries are evicted on read.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::client::TtsClient;
use crate::config::VoiceConfigStore;
use crate::error::{Result, VoiceError};
use crate::redact::check_egress;

/// Hard text-length ceiling. Ports v3's `MAX_TEXT_CHARS = 4000` floor —
/// defends the local TTS server from runaway prompts. The voice-porter
/// spec didn't *require* this, but dropping it silently would push the
/// responsibility onto evy-comms, which is the wrong layer.
pub const MAX_TEXT_CHARS: usize = 4000;

/// Cache TTL — matches v3 byte-for-byte.
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// File extension for cached audio. Synthesize returns raw bytes; the
/// TTS server is currently `wav`-only. If a future server starts
/// emitting other codecs we'll plumb format through, but for now the
/// single extension keeps the cache filename scheme stable.
const AUDIO_EXT: &str = "wav";

/// What a successful render returns to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderResult {
    /// URL the dashboard can use to fetch the audio. Format:
    /// `/voice/audio/<fingerprint>.<ext>` — matches v3's route shape so
    /// the v4 dashboard can wire to the same path layout.
    pub audio_url: String,
    /// Absolute path on disk. Useful for evy-comms when it needs to
    /// upload the file to Telegram/Discord (those bridges read from
    /// disk; they don't fetch the dashboard URL).
    pub audio_path: PathBuf,
    /// Number of bytes written (0 if cached).
    pub bytes_written: u64,
    /// `true` if this render was a cache hit.
    pub cached: bool,
}

/// Status snapshot for `/voice/status` callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceStatus {
    /// `enabled` flag from the live config.
    pub enabled: bool,
    /// Currently configured default voice id.
    pub voice_id: String,
    /// Currently configured model.
    pub model: String,
    /// Currently configured TTS server URL.
    pub tts_server: String,
    /// `true` iff a `GET /health` probe just succeeded.
    pub server_reachable: bool,
}

/// Renderer. Hold one per daemon — it's a thin orchestrator and the
/// expensive resources (HTTP client, config store) are shared via
/// [`Arc`].
pub struct VoiceRenderer {
    store: Arc<VoiceConfigStore>,
    client: Arc<TtsClient>,
    cache_dir: PathBuf,
}

impl VoiceRenderer {
    /// Build a renderer. The `cache_dir` is created on first render if
    /// it doesn't exist.
    #[must_use]
    pub fn new(store: Arc<VoiceConfigStore>, client: Arc<TtsClient>, cache_dir: PathBuf) -> Self {
        Self {
            store,
            client,
            cache_dir,
        }
    }

    /// Render `text` to audio. `voice_id` overrides the config default
    /// when `Some`; pass `None` to use whatever the live config says.
    pub async fn render(&self, text: &str, voice_id: Option<&str>) -> Result<RenderResult> {
        let cfg = self.store.current();
        if !cfg.enabled {
            return Err(VoiceError::Disabled);
        }

        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(VoiceError::EmptyText);
        }
        // Length is measured in chars (not bytes) to match v3 byte-for-byte.
        let char_len = trimmed.chars().count();
        if char_len > MAX_TEXT_CHARS {
            return Err(VoiceError::TextTooLong {
                limit: MAX_TEXT_CHARS,
                got: char_len,
            });
        }

        check_egress(trimmed)?;

        let resolved_voice = voice_id
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&cfg.default_voice_id)
            .to_owned();
        let model = cfg.model.clone();
        let fp = fingerprint(trimmed, &resolved_voice, &model);

        // Ensure cache dir exists.
        if !self.cache_dir.exists() {
            std::fs::create_dir_all(&self.cache_dir).map_err(|source| VoiceError::Io {
                path: self.cache_dir.clone(),
                source,
            })?;
        }

        let cached_path = self.cache_dir.join(format!("{fp}.{AUDIO_EXT}"));

        // Cache lookup — evict if expired.
        if cached_path.exists() {
            match std::fs::metadata(&cached_path) {
                Ok(meta) => match meta.modified() {
                    Ok(mtime) => {
                        if SystemTime::now()
                            .duration_since(mtime)
                            .map(|age| age < CACHE_TTL)
                            .unwrap_or(false)
                        {
                            tracing::debug!(
                                fingerprint = %fp,
                                "voice cache hit"
                            );
                            return Ok(RenderResult {
                                audio_url: format!("/voice/audio/{fp}.{AUDIO_EXT}"),
                                audio_path: cached_path,
                                bytes_written: 0,
                                cached: true,
                            });
                        }
                        // Expired — best-effort eviction.
                        let _ = std::fs::remove_file(&cached_path);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "voice cache mtime read failed; treating as miss");
                    }
                },
                Err(e) => {
                    tracing::warn!(error = %e, "voice cache metadata read failed; treating as miss");
                }
            }
        }

        // Cache miss — synthesize.
        tracing::debug!(
            fingerprint = %fp,
            voice = %resolved_voice,
            model = %model,
            "voice cache miss, calling tts server",
        );
        let bytes = self
            .client
            .synthesize(trimmed, &resolved_voice, &model)
            .await?;

        let bytes_written = bytes.len() as u64;
        std::fs::write(&cached_path, &bytes).map_err(|source| VoiceError::Io {
            path: cached_path.clone(),
            source,
        })?;

        Ok(RenderResult {
            audio_url: format!("/voice/audio/{fp}.{AUDIO_EXT}"),
            audio_path: cached_path,
            bytes_written,
            cached: false,
        })
    }

    /// Snapshot of the operator-facing voice status, including a fresh
    /// `/health` probe. Used by the eventual `/voice/status` HTTP route.
    pub async fn status(&self) -> Result<VoiceStatus> {
        let cfg = self.store.current();
        let health = self.client.health().await?;
        Ok(VoiceStatus {
            enabled: cfg.enabled,
            voice_id: cfg.default_voice_id,
            model: cfg.model,
            tts_server: cfg.tts_server,
            server_reachable: health.reachable,
        })
    }

    /// Resolve a `<fingerprint>.<ext>` filename back to its on-disk path
    /// for serving from `GET /voice/audio/<file>`. Returns `None` if the
    /// filename fails the safety checks (path-traversal defence) or if
    /// the entry is missing / expired.
    #[must_use]
    pub fn resolve_cached(&self, file: &str) -> Option<PathBuf> {
        // Strict shape: hex fingerprint + dot + short extension. Mirrors
        // v3's `resolveCachedAudio` defence-in-depth.
        let mut chars = file.chars();
        let hex_ok = (&mut chars)
            .take_while(|c| *c != '.')
            .all(|c| c.is_ascii_hexdigit());
        if !hex_ok {
            return None;
        }
        if file.contains('/') || file.contains('\\') || file.contains("..") {
            return None;
        }
        let path = self.cache_dir.join(file);
        if !path.starts_with(&self.cache_dir) {
            return None;
        }
        let meta = std::fs::metadata(&path).ok()?;
        let mtime = meta.modified().ok()?;
        if SystemTime::now().duration_since(mtime).ok()? > CACHE_TTL {
            let _ = std::fs::remove_file(&path);
            return None;
        }
        Some(path)
    }

    /// Path the cache writes audio to.
    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }
}

/// Cache fingerprint. `SHA256(text | voice_id | model)`, full 64-char
/// hex. The `|` separator removes the ambiguous-concatenation collision
/// edge case (where one field's trailing bytes could overlap with the
/// next field's leading bytes).
#[must_use]
pub fn fingerprint(text: &str, voice_id: &str, model: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    h.update(b"|");
    h.update(voice_id.as_bytes());
    h.update(b"|");
    h.update(model.as_bytes());
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_deterministic() {
        let a = fingerprint("hello", "evy-rachel-weisz", "voxcpm-0.5b");
        let b = fingerprint("hello", "evy-rachel-weisz", "voxcpm-0.5b");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn fingerprint_changes_on_each_input() {
        let base = fingerprint("hello", "evy-rachel-weisz", "voxcpm-0.5b");
        let text = fingerprint("hello!", "evy-rachel-weisz", "voxcpm-0.5b");
        let voice = fingerprint("hello", "evy-other", "voxcpm-0.5b");
        let model = fingerprint("hello", "evy-rachel-weisz", "voxcpm-1.0b");
        assert_ne!(base, text);
        assert_ne!(base, voice);
        assert_ne!(base, model);
    }

    #[test]
    fn fingerprint_separator_blocks_concat_collision() {
        // Without the `|` separator, ("a","bc","d") and ("ab","c","d")
        // would hash to the same blob. With it, they differ.
        let a = fingerprint("a", "bc", "d");
        let b = fingerprint("ab", "c", "d");
        assert_ne!(a, b);
    }
}
