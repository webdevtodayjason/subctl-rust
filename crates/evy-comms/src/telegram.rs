//! Telegram bridge — outbound `sendMessage` + inbound `getUpdates`
//! long-poll loop + ask round-trip.
//!
//! Architecture
//! ------------
//! ```text
//!   daemon ────────────▶ TelegramBridge.notify(Notification)
//!                              │
//!                              ▼
//!                       reqwest POST /sendMessage ──▶ Telegram
//!
//!   daemon ◀──── mpsc::UnboundedSender<InboundMessage> ◀──── run()
//!                                                              │
//!                                              GET /getUpdates ▼
//!                                                       (long-poll)
//! ```
//!
//! The struct is cheap to `Clone` — internals live behind `Arc<Inner>`
//! — so the daemon spawns `bridge.clone().run(token)` and keeps the
//! original handle for `notify` / `ask`.
//!
//! Errors
//! ------
//! Per the v4.0 workspace constraint we can't add a new `Error::Comms`
//! variant to `evy-core` from this slice (the orchestrator owns that
//! merge). Telegram-shaped failures land on `Error::Provider { kind:
//! ClaudeCode, reason: "telegram: ..." }` — the `"telegram:"` prefix
//! is the breadcrumb that disambiguates at the operator's logs. A
//! TODO comment on every callsite flags the swap once `Error::Comms`
//! is available.
//!
//! Ask-matching strategy
//! ---------------------
//! When `ask()` posts a question, Telegram returns the outbound
//! `message_id`. We remember the mapping `message_id → AskId` in an
//! in-memory map. On inbound `getUpdates`, if a message carries
//! `reply_to_message.message_id` that matches an open ask, we resolve
//! it; otherwise the inbound is dispatched as a normal
//! [`InboundMessage`] for the daemon's command handler. This mirrors
//! the natural Telegram UX (operator replies to the question bubble)
//! and avoids burdening the operator with `/ask <id>` markers.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use evy_core::{Error, ProviderKind, Result};

use crate::ask::{AskId, AskRegistry};
use crate::notification::Notification;

// ─── public types ────────────────────────────────────────────────────────

/// Operator → daemon message after Telegram-side parsing. Anything
/// that's NOT a reply to a tracked ask reaches the daemon through
/// this shape; replies to tracked asks resolve the registry and
/// never appear here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundMessage {
    /// Telegram chat id the message arrived from (echoes the configured
    /// one in single-chat deployments).
    pub chat_id: i64,
    /// Telegram message id (stable per chat).
    pub message_id: i64,
    /// Plaintext body. Empty messages (stickers, media-only) are
    /// filtered before dispatch.
    pub text: String,
    /// Operator's Telegram numeric user id (`None` for messages
    /// without a `from` field, which shouldn't happen for private
    /// chats but Telegram permits).
    pub from_user_id: Option<i64>,
    /// Operator's first name from the Telegram profile.
    pub from_name: Option<String>,
}

/// Static configuration for [`TelegramBridge`].
#[derive(Clone)]
pub struct TelegramConfig {
    /// Bot API token. From env `TELEGRAM_BOT_TOKEN`; never logged.
    pub bot_token: String,
    /// Authorized chat id. Inbound messages from any other chat are
    /// dropped at the listener. From env `TELEGRAM_CHAT_ID`.
    pub chat_id: i64,
    /// How long the inbound loop sleeps between iterations when the
    /// previous `getUpdates` returned no messages.
    pub poll_interval: Duration,
    /// `timeout=` query argument to `getUpdates`. Telegram blocks up
    /// to this long server-side waiting for an update.
    pub long_poll_timeout: Duration,
    /// Base URL — overridden by tests to point at a `wiremock` server.
    /// Defaults to `https://api.telegram.org`.
    pub base_url: String,
    /// Where inbound non-reply messages are forwarded to the daemon's
    /// command handler. `None` is valid (notify-only mode).
    pub inbound: Option<mpsc::UnboundedSender<InboundMessage>>,
    /// Live SSE broadcaster shared with the HTTP server. When present,
    /// every authorized non-ask inbound also emits a named `inbound`
    /// frame onto `/api/evy/events` so the dashboard cockpit's live feed
    /// (`orch.js`) sees operator traffic. `None` keeps the bridge
    /// decoupled in notify-only / headless deployments.
    pub events: Option<crate::sse::EventBroadcaster>,
}

/// Manual `Debug` so no caller can accidentally log the bot token —
/// the field doc promises "never logged" and this enforces it.
impl std::fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("bot_token", &"[redacted]")
            .field("chat_id", &self.chat_id)
            .field("poll_interval", &self.poll_interval)
            .field("long_poll_timeout", &self.long_poll_timeout)
            .field("base_url", &self.base_url)
            .field("inbound", &self.inbound.is_some())
            .field("events", &self.events.is_some())
            .finish()
    }
}

impl TelegramConfig {
    /// Construct a config with the required fields and conservative
    /// defaults for the timing knobs.
    #[must_use]
    pub fn new(bot_token: String, chat_id: i64) -> Self {
        Self {
            bot_token,
            chat_id,
            poll_interval: Duration::from_secs(1),
            long_poll_timeout: Duration::from_secs(25),
            base_url: "https://api.telegram.org".to_string(),
            inbound: None,
            events: None,
        }
    }
}

/// Cheaply-`Clone`-able handle. Spawn `bridge.clone().run(token)` and
/// keep the original to call `notify` / `ask` from the daemon.
#[derive(Clone)]
pub struct TelegramBridge {
    inner: Arc<Inner>,
}

struct Inner {
    config: TelegramConfig,
    http: reqwest::Client,
    asks: Arc<AskRegistry>,
    /// `message_id` of an outbound ask → `AskId` so the inbound
    /// loop can resolve the registry on a Telegram reply.
    open_asks: Mutex<HashMap<i64, AskId>>,
}

impl TelegramBridge {
    /// Construct a bridge. `asks` is shared with the daemon so other
    /// surfaces (HTTP, future TUI) can see / answer pending asks too.
    #[must_use]
    pub fn new(config: TelegramConfig, asks: Arc<AskRegistry>) -> Self {
        let http = reqwest::Client::builder()
            // Telegram's long-poll already enforces server-side; this
            // guards us against an idle TCP socket if the upstream
            // never responds at all.
            .timeout(Duration::from_secs(60))
            .build()
            .expect("reqwest client builder is infallible in this config");
        Self {
            inner: Arc::new(Inner {
                config,
                http,
                asks,
                open_asks: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The chat id this bridge is configured to target. Surfaced in the
    /// `POST /api/settings/telegram/test` response so the operator can confirm
    /// which chat the live bridge would message.
    #[must_use]
    pub fn chat_id(&self) -> i64 {
        self.inner.config.chat_id
    }

    /// Send a [`Notification`] to the configured chat.
    ///
    /// # Errors
    /// [`Error::Provider`] with `"telegram:"`-prefixed reason on any
    /// transport / API failure.
    pub async fn notify(&self, notification: Notification) -> Result<()> {
        let text = notification.render_text();
        let _ = self.send_message(&text).await?;
        Ok(())
    }

    /// Post a question and block (with timeout) until the operator
    /// replies via Telegram.
    ///
    /// Internally:
    /// 1. `AskRegistry::open(question)` mints an [`AskId`].
    /// 2. `sendMessage` posts the prompt; Telegram returns
    ///    `message_id`.
    /// 3. We map `message_id → AskId` so the inbound loop can resolve
    ///    the right one when a reply arrives.
    /// 4. `AskRegistry::wait_for(id, timeout)` parks until resolved
    ///    or the deadline elapses.
    ///
    /// # Errors
    /// - [`Error::Provider`] on Telegram transport.
    /// - [`Error::WorkerFailed`] when the operator doesn't reply
    ///   before `timeout` elapses (see [`AskRegistry::wait_for`]).
    pub async fn ask(&self, question: String, timeout: Duration) -> Result<String> {
        let ask_id = self.inner.asks.open(question.clone()).await;
        let prompt = Notification::AskPending {
            ask_id,
            question: question.clone(),
        }
        .render_text();
        // Hold the open_asks lock across send_message so the inbound
        // poll loop (which acquires the same lock in handle_update)
        // can't observe the operator's reply before our message_id →
        // AskId mapping is in place. Without this guard, a fast
        // operator reply lands during the window between Telegram
        // ACK'ing our send and the bridge inserting the mapping,
        // and the reply gets misrouted as a non-ask inbound.
        {
            let mut open = self.inner.open_asks.lock().await;
            let result = self.send_message(&prompt).await?;
            open.insert(result.message_id, ask_id);
        }
        let outcome = self.inner.asks.wait_for(ask_id, timeout).await;
        // Clear the mapping on ANY exit (resolution already removed it;
        // timeout did not). A dead entry would otherwise make the
        // plain-message fallback in `handle_update` see phantom open
        // asks forever.
        {
            let mut open = self.inner.open_asks.lock().await;
            open.retain(|_, v| *v != ask_id);
        }
        outcome
    }

    /// Long-poll Telegram for inbound updates until `shutdown` is
    /// cancelled. Consumes the bridge handle; clone before spawning
    /// if you need the original.
    ///
    /// # Errors
    /// Propagates a fatal transport error only when the loop cannot
    /// usefully continue (e.g., a 4xx other than 409). Transient
    /// 5xx / network failures are logged and retried.
    pub async fn run(self, shutdown: CancellationToken) -> Result<()> {
        let mut offset: i64 = 0;
        info!("telegram bridge: starting getUpdates long-poll");

        loop {
            if shutdown.is_cancelled() {
                info!("telegram bridge: shutdown signalled, exiting poll loop");
                return Ok(());
            }

            let poll = self.poll_once(offset);
            let outcome = tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    info!("telegram bridge: cancelled during poll");
                    return Ok(());
                }
                r = poll => r,
            };

            match outcome {
                Ok(updates) => {
                    for update in updates {
                        if let Some(next) = update.update_id.checked_add(1) {
                            if next > offset {
                                offset = next;
                            }
                        }
                        if let Err(e) = self.handle_update(update).await {
                            warn!(error = %e, "telegram bridge: handle_update failed");
                        }
                    }
                }
                Err(e) => {
                    error!(error = %e, "telegram bridge: poll error; backing off");
                    // Use `tokio::time::sleep` so cancellation can
                    // still pre-empt via the select! above on the next
                    // iteration.
                    tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => return Ok(()),
                        _ = tokio::time::sleep(self.inner.config.poll_interval) => {}
                    }
                }
            }
        }
    }

    // ─── internals ──────────────────────────────────────────────────

    async fn send_message(&self, text: &str) -> Result<SendMessageResult> {
        let url = format!(
            "{}/bot{}/sendMessage",
            self.inner.config.base_url, self.inner.config.bot_token
        );
        let body = serde_json::json!({
            "chat_id": self.inner.config.chat_id,
            "text": text,
        });
        let resp = self
            .inner
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            // TODO(team-lead): replace with Error::Comms once added to evy-core.
            // `.without_url()` strips the URL component from reqwest's
            // `Display` impl so the bot token (embedded in the URL path
            // as `bot<TOKEN>/sendMessage`) does NOT land in logs / error
            // surfaces. The token is a Telegram credential.
            .map_err(|e| Error::Provider {
                kind: ProviderKind::ClaudeCode,
                reason: format!("telegram: sendMessage transport: {}", e.without_url()),
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let snippet = resp.text().await.unwrap_or_default();
            // TODO(team-lead): replace with Error::Comms once added to evy-core.
            return Err(Error::Provider {
                kind: ProviderKind::ClaudeCode,
                reason: format!(
                    "telegram: sendMessage HTTP {status}: {}",
                    snippet.chars().take(200).collect::<String>()
                ),
            });
        }

        let parsed: SendMessageResponse = resp.json().await.map_err(|e| Error::Provider {
            kind: ProviderKind::ClaudeCode,
            // .without_url() — see send_message transport error for rationale.
            reason: format!("telegram: sendMessage decode: {}", e.without_url()),
        })?;

        if !parsed.ok {
            // TODO(team-lead): replace with Error::Comms once added to evy-core.
            return Err(Error::Provider {
                kind: ProviderKind::ClaudeCode,
                reason: format!(
                    "telegram: sendMessage api: {}",
                    parsed.description.unwrap_or_default()
                ),
            });
        }

        parsed.result.ok_or_else(|| Error::Provider {
            kind: ProviderKind::ClaudeCode,
            reason: "telegram: sendMessage returned ok=true with no result".to_string(),
        })
    }

    async fn poll_once(&self, offset: i64) -> Result<Vec<TelegramUpdate>> {
        let url = format!(
            "{}/bot{}/getUpdates",
            self.inner.config.base_url, self.inner.config.bot_token
        );
        let mut req = self
            .inner
            .http
            .get(&url)
            .query(&[(
                "timeout",
                self.inner.config.long_poll_timeout.as_secs().to_string(),
            )])
            .query(&[("allowed_updates", "[\"message\"]")]);
        if offset > 0 {
            req = req.query(&[("offset", offset.to_string())]);
        }

        let resp = req.send().await.map_err(|e| Error::Provider {
            kind: ProviderKind::ClaudeCode,
            // .without_url() — strips the bot-token-bearing URL from
            // reqwest's Display impl so the credential never lands in logs.
            reason: format!("telegram: getUpdates transport: {}", e.without_url()),
        })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let snippet = resp.text().await.unwrap_or_default();
            return Err(Error::Provider {
                kind: ProviderKind::ClaudeCode,
                reason: format!(
                    "telegram: getUpdates HTTP {status}: {}",
                    snippet.chars().take(200).collect::<String>()
                ),
            });
        }

        let parsed: GetUpdatesResponse = resp.json().await.map_err(|e| Error::Provider {
            kind: ProviderKind::ClaudeCode,
            // .without_url() — see getUpdates transport error for rationale.
            reason: format!("telegram: getUpdates decode: {}", e.without_url()),
        })?;

        if !parsed.ok {
            return Err(Error::Provider {
                kind: ProviderKind::ClaudeCode,
                reason: format!(
                    "telegram: getUpdates api: {}",
                    parsed.description.unwrap_or_default()
                ),
            });
        }

        Ok(parsed.result.unwrap_or_default())
    }

    async fn handle_update(&self, update: TelegramUpdate) -> Result<()> {
        let Some(message) = update.message else {
            debug!(
                update_id = update.update_id,
                "telegram: ignoring non-message update"
            );
            return Ok(());
        };

        let Some(text) = message
            .text
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            debug!("telegram: ignoring empty / non-text message");
            return Ok(());
        };

        // Auth: drop messages from any chat other than the configured one.
        if let Some(chat) = &message.chat {
            if chat.id != self.inner.config.chat_id {
                warn!(
                    chat_id = chat.id,
                    "telegram: dropping message from unauthorized chat"
                );
                return Ok(());
            }
        }

        // Reply to a tracked ask?
        if let Some(reply_to) = &message.reply_to_message {
            let resolved = {
                let mut open = self.inner.open_asks.lock().await;
                open.remove(&reply_to.message_id)
            };
            if let Some(ask_id) = resolved {
                self.inner.asks.resolve(ask_id, text.to_string()).await?;
                debug!(
                    ?ask_id,
                    message_id = reply_to.message_id,
                    "telegram: resolved ask via in-reply-to"
                );
                return Ok(());
            }
        }

        // Single-operator ergonomics: a plain message (or a reply to a
        // stale bubble) answers the ask when EXACTLY ONE is open —
        // operators type answers directly far more often than they use
        // Telegram's reply-to (live finding, 2026-06-09). Two or more
        // open asks are ambiguous and still require reply-to.
        let lone_ask = {
            let mut open = self.inner.open_asks.lock().await;
            if open.len() == 1 {
                let key = *open.keys().next().expect("len == 1");
                open.remove(&key)
            } else {
                None
            }
        };
        if let Some(ask_id) = lone_ask {
            self.inner.asks.resolve(ask_id, text.to_string()).await?;
            debug!(
                ?ask_id,
                "telegram: resolved lone open ask via plain message"
            );
            return Ok(());
        }

        // Otherwise dispatch as a normal inbound message. Surface it on
        // the dashboard live feed first — the cockpit (`orch.js`) listens
        // for the named `inbound` frame regardless of whether a daemon
        // command handler is wired.
        if let Some(events) = self.inner.config.events.as_ref() {
            events.emit(crate::events::DaemonEvent::dashboard_inbound(
                "telegram", text,
            ));
        }
        if let Some(tx) = self.inner.config.inbound.as_ref() {
            let inbound = InboundMessage {
                chat_id: message.chat.as_ref().map(|c| c.id).unwrap_or(0),
                message_id: message.message_id,
                text: text.to_string(),
                from_user_id: message.from.as_ref().map(|u| u.id),
                from_name: message.from.as_ref().and_then(|u| u.first_name.clone()),
            };
            if tx.send(inbound).is_err() {
                warn!("telegram: inbound channel closed; dropping message");
            }
        } else {
            debug!("telegram: no inbound handler configured; dropping message");
        }

        Ok(())
    }
}

// ─── Telegram Bot API wire shapes (only the fields we read) ──────────────

#[derive(Debug, Deserialize)]
struct GetUpdatesResponse {
    ok: bool,
    #[serde(default)]
    result: Option<Vec<TelegramUpdate>>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SendMessageResponse {
    ok: bool,
    #[serde(default)]
    result: Option<SendMessageResult>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SendMessageResult {
    message_id: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    #[serde(default)]
    message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    message_id: i64,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    chat: Option<TelegramChat>,
    #[serde(default)]
    from: Option<TelegramUser>,
    #[serde(default)]
    reply_to_message: Option<Box<TelegramMessage>>,
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramUser {
    id: i64,
    #[serde(default)]
    first_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_bot_token() {
        let c = TelegramConfig::new("SECRET123".to_string(), 1);
        let rendered = format!("{c:?}");
        assert!(!rendered.contains("SECRET123"), "token leaked: {rendered}");
        assert!(rendered.contains("[redacted]"));
    }

    #[test]
    fn config_defaults_are_sensible() {
        let c = TelegramConfig::new("tok".into(), 1234);
        assert_eq!(c.bot_token, "tok");
        assert_eq!(c.chat_id, 1234);
        assert_eq!(c.base_url, "https://api.telegram.org");
        assert!(c.long_poll_timeout > Duration::from_secs(0));
    }

    #[test]
    fn send_message_response_decodes_ok_shape() {
        let body = r#"{"ok":true,"result":{"message_id":42}}"#;
        let parsed: SendMessageResponse = serde_json::from_str(body).expect("decode");
        assert!(parsed.ok);
        assert_eq!(parsed.result.unwrap().message_id, 42);
    }

    #[test]
    fn get_updates_response_decodes_empty_result() {
        let body = r#"{"ok":true,"result":[]}"#;
        let parsed: GetUpdatesResponse = serde_json::from_str(body).expect("decode");
        assert!(parsed.ok);
        assert!(parsed.result.unwrap().is_empty());
    }

    #[test]
    fn update_with_reply_decodes() {
        let body = r#"{
            "update_id": 100,
            "message": {
                "message_id": 51,
                "text": "ok then",
                "chat": {"id": 1234},
                "from": {"id": 99, "first_name": "Jason"},
                "reply_to_message": {"message_id": 50}
            }
        }"#;
        let parsed: TelegramUpdate = serde_json::from_str(body).expect("decode");
        let m = parsed.message.unwrap();
        assert_eq!(m.message_id, 51);
        assert_eq!(m.reply_to_message.unwrap().message_id, 50);
    }
}
