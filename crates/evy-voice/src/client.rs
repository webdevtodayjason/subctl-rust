//! HTTP client for the local TTS server.
//!
//! The TTS server (separate launchd-managed `com.subctl.tts` service
//! listening on `:8789`) is **outside** v4's ownership — this crate is
//! a *consumer*. We POST text and receive audio bytes; the server picks
//! the codec (we default to `wav` if the `X-Audio-Format` header is
//! missing, matching v3's `voice-render.ts`).
//!
//! ## Surface
//!
//! - [`TtsClient::synthesize`] — `(text, voice_id, model) -> Vec<u8>`
//! - [`TtsClient::health`]     — `GET /health` reachability probe
//!
//! The renderer takes [`Arc<TtsClient>`] so it's cheap to share across
//! tasks. Construction is synchronous; the client lazily reuses a single
//! [`reqwest::Client`] connection pool.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{Result, VoiceError};

/// Quick reachability + latency report from the TTS server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtsHealth {
    /// `true` iff the server returned 2xx on `GET /health`.
    pub reachable: bool,
    /// The base URL probed (no trailing slash).
    pub url: String,
    /// Round-trip latency in milliseconds.
    pub latency_ms: u64,
    /// HTTP status code, if the request reached the server.
    pub status: Option<u16>,
}

/// Request body for the TTS server's `/render` endpoint. Field names
/// match v3's `voice-render.ts` POST so the server doesn't need to
/// adapt.
#[derive(Debug, Serialize)]
struct RenderRequest<'a> {
    text: &'a str,
    voice_id: &'a str,
    model: &'a str,
}

/// HTTP client bound to a single TTS server URL.
pub struct TtsClient {
    server_url: String,
    http: reqwest::Client,
}

impl TtsClient {
    /// Build a client targeted at `server_url`. Trailing slash is
    /// normalised away so callers can be lazy with `tts_server` values
    /// from `voice.json`.
    #[must_use]
    pub fn new(server_url: String) -> Self {
        let trimmed = server_url.trim_end_matches('/').to_owned();
        // 30s render timeout — large enough for a paragraph on the
        // M3, small enough to surface a wedged server quickly. The
        // operator-facing /voice/render route can decide if it wants
        // to surface a longer ceiling on its end.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest::Client::build with no custom TLS / proxy must succeed");
        Self {
            server_url: trimmed,
            http,
        }
    }

    /// POST `text + voice_id + model` to the TTS server, return the
    /// raw audio bytes.
    pub async fn synthesize(&self, text: &str, voice_id: &str, model: &str) -> Result<Vec<u8>> {
        let url = format!("{}/render", self.server_url);
        let body = RenderRequest {
            text,
            voice_id,
            model,
        };

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| VoiceError::TtsTransport(format!("POST {url}: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let detail = resp
                .text()
                .await
                .unwrap_or_else(|_| "<body read failed>".to_owned());
            return Err(VoiceError::TtsServerStatus {
                status: status.as_u16(),
                detail: detail.chars().take(200).collect(),
            });
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| VoiceError::TtsTransport(format!("read body: {e}")))?
            .to_vec();
        if bytes.is_empty() {
            return Err(VoiceError::TtsServerEmpty);
        }
        Ok(bytes)
    }

    /// `GET /health`. Never returns `Err` — transport failures surface
    /// as `reachable: false` so the caller can render a single-table
    /// status line without try/catching.
    pub async fn health(&self) -> Result<TtsHealth> {
        let url = format!("{}/health", self.server_url);
        let start = Instant::now();
        match self.http.get(&url).send().await {
            Ok(resp) => {
                let latency_ms = start.elapsed().as_millis() as u64;
                let status = resp.status();
                Ok(TtsHealth {
                    reachable: status.is_success(),
                    url: self.server_url.clone(),
                    latency_ms,
                    status: Some(status.as_u16()),
                })
            }
            Err(e) => {
                tracing::debug!(error = %e, url = %url, "tts health probe failed");
                Ok(TtsHealth {
                    reachable: false,
                    url: self.server_url.clone(),
                    latency_ms: start.elapsed().as_millis() as u64,
                    status: None,
                })
            }
        }
    }

    /// Base URL the client is bound to (no trailing slash). Surfaced on
    /// the `VoiceStatus` so the dashboard can show what's configured.
    #[must_use]
    pub fn server_url(&self) -> &str {
        &self.server_url
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_slash_is_normalised() {
        let c = TtsClient::new("http://localhost:8789/".to_owned());
        assert_eq!(c.server_url(), "http://localhost:8789");
    }

    #[test]
    fn empty_url_is_kept_as_is() {
        // Operator config errors should be surfaced at use-time
        // (synthesize will fail with a clear transport error), not
        // hidden behind a panic at construction.
        let c = TtsClient::new(String::new());
        assert_eq!(c.server_url(), "");
    }
}
