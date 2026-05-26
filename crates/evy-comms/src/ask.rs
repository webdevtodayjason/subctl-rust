//! Ask round-trip primitive.
//!
//! An `Ask` is a single question Evy posts to the operator (today: via
//! Telegram, later: any channel) and a slot for the eventual reply.
//! [`AskRegistry`] stores open asks in-memory, hands callers a
//! [`tokio::sync::oneshot`] receiver to await the answer with a timeout,
//! and resolves the right one when the inbound channel delivers a reply.
//!
//! Concurrency
//! -----------
//! The internal `HashMap` is guarded by a `tokio::sync::Mutex`. Critical
//! sections are O(1) hash ops; the await happens on the oneshot
//! receiver outside the lock. `wait_for` uses `tokio::time::timeout`
//! so a stuck operator can never block the daemon's task tree forever.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, Mutex};
use uuid::Uuid;

use evy_core::{Error, Result};

/// Opaque identifier for a pending ask.
///
/// Wrapped UUID v4. Public field intentionally — see [`evy_core::MandateId`]
/// for the same pattern; downstream code (HTTP/SSE renderer, tests)
/// matches the surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AskId(pub Uuid);

impl AskId {
    /// Mint a fresh v4 UUID-backed ask id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for AskId {
    fn default() -> Self {
        Self::new()
    }
}

/// A single question the operator may answer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ask {
    /// Stable id minted at `open()` time.
    pub id: AskId,
    /// Operator-visible prompt.
    pub question: String,
    /// When the question was posted.
    pub posted_at: DateTime<Utc>,
    /// When the operator's reply landed (None until resolved).
    pub answered_at: Option<DateTime<Utc>>,
    /// The operator's reply (None until resolved).
    pub answer: Option<String>,
}

/// Internal slot: the public-shape `Ask` plus the one-shot the waiter
/// is parked on. `Option<Sender<_>>` so `resolve()` can `.take()` it
/// (oneshot::Sender::send consumes `self`).
struct Slot {
    ask: Ask,
    notify: Option<oneshot::Sender<String>>,
}

/// In-memory registry of open + recently-resolved asks.
///
/// `Default` is implemented and idiomatic.
pub struct AskRegistry {
    slots: Mutex<HashMap<AskId, Slot>>,
}

impl AskRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
        }
    }

    /// Open a new pending ask and return its id.
    ///
    /// The waiter created internally is parked until [`Self::resolve`]
    /// is called with the matching id, or [`Self::wait_for`] times out.
    pub async fn open(&self, question: String) -> AskId {
        let id = AskId::new();
        let (tx, _rx) = oneshot::channel::<String>();
        // _rx is dropped here on purpose — callers that want to await
        // an answer must use `wait_for`, which installs its own pair.
        // Storing a sender is what lets `resolve` notify any future
        // waiter set up by `wait_for`. Concretely, we don't pre-park a
        // listener; `wait_for` swaps the slot's `notify` in atomically.
        let slot = Slot {
            ask: Ask {
                id,
                question,
                posted_at: Utc::now(),
                answered_at: None,
                answer: None,
            },
            notify: Some(tx),
        };
        let mut slots = self.slots.lock().await;
        slots.insert(id, slot);
        id
    }

    /// Resolve a pending ask with the operator's answer.
    ///
    /// Idempotent: a second call on the same id returns
    /// [`Error::InvalidMandate`] (we don't have a `NotFound` variant
    /// for asks; mandate-shaped is the closest fit and the message
    /// makes the cause clear).
    ///
    /// # Errors
    /// Returns [`Error::InvalidMandate`] if the ask is unknown or
    /// already resolved.
    pub async fn resolve(&self, id: AskId, answer: String) -> Result<()> {
        let mut slots = self.slots.lock().await;
        let slot = slots
            .get_mut(&id)
            .ok_or_else(|| Error::InvalidMandate(format!("ask {id:?} not open")))?;
        if slot.ask.answered_at.is_some() {
            return Err(Error::InvalidMandate(format!(
                "ask {id:?} already resolved"
            )));
        }
        slot.ask.answered_at = Some(Utc::now());
        slot.ask.answer = Some(answer.clone());
        if let Some(tx) = slot.notify.take() {
            // The receiver may have been dropped (waiter gave up on
            // timeout); that's fine, swallow the send error.
            let _ = tx.send(answer);
        }
        Ok(())
    }

    /// List currently-pending asks (those with no answer yet) in
    /// arbitrary order.
    pub async fn pending(&self) -> Vec<Ask> {
        let slots = self.slots.lock().await;
        slots
            .values()
            .filter(|s| s.ask.answered_at.is_none())
            .map(|s| s.ask.clone())
            .collect()
    }

    /// All asks (pending and resolved) — useful for tests + future audit.
    pub async fn all(&self) -> Vec<Ask> {
        let slots = self.slots.lock().await;
        slots.values().map(|s| s.ask.clone()).collect()
    }

    /// Block (with timeout) until the named ask resolves; returns the
    /// operator's answer.
    ///
    /// Installs a fresh oneshot pair into the slot so the next
    /// `resolve()` notifies us. If the slot was already resolved before
    /// this call returns the recorded answer immediately.
    ///
    /// # Errors
    /// - [`Error::InvalidMandate`] if `id` is unknown.
    /// - [`Error::WorkerFailed`] on timeout (closest fit; the operator
    ///   simply did not answer in time and the caller will treat that
    ///   the same as a failed dispatch).
    pub async fn wait_for(&self, id: AskId, timeout: Duration) -> Result<String> {
        // Phase 1: install a receiver while holding the lock, then drop
        // the lock before awaiting so other tasks can resolve.
        let rx = {
            let mut slots = self.slots.lock().await;
            let slot = slots
                .get_mut(&id)
                .ok_or_else(|| Error::InvalidMandate(format!("ask {id:?} not open")))?;
            if let Some(answer) = slot.ask.answer.clone() {
                return Ok(answer);
            }
            let (tx, rx) = oneshot::channel::<String>();
            slot.notify = Some(tx);
            rx
        };

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(_recv_err)) => {
                // Sender was dropped without sending. This can only
                // happen if the slot was overwritten by a concurrent
                // `wait_for` on the same id — surface as a generic
                // failure.
                Err(Error::WorkerFailed(format!(
                    "ask {id:?} cancelled before answer"
                )))
            }
            Err(_elapsed) => Err(Error::WorkerFailed(format!(
                "ask {id:?} timed out after {timeout:?}"
            ))),
        }
    }
}

impl Default for AskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// `Arc<AskRegistry>` is the type the daemon hands around. Re-exported
/// shorthand keeps call sites tidy.
pub type SharedAskRegistry = Arc<AskRegistry>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_then_resolve_records_answer() {
        let reg = AskRegistry::new();
        let id = reg.open("continue?".into()).await;
        assert_eq!(reg.pending().await.len(), 1);
        reg.resolve(id, "yes".into()).await.expect("resolve");
        let pending = reg.pending().await;
        assert!(pending.is_empty(), "resolved ask should not be pending");
        let all = reg.all().await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].answer.as_deref(), Some("yes"));
        assert!(all[0].answered_at.is_some());
    }

    #[tokio::test]
    async fn wait_for_returns_answer() {
        let reg = Arc::new(AskRegistry::new());
        let id = reg.open("q?".into()).await;

        let reg2 = reg.clone();
        let resolver = tokio::spawn(async move {
            // Give the waiter a moment to install its oneshot.
            tokio::time::sleep(Duration::from_millis(20)).await;
            reg2.resolve(id, "answered".into()).await.expect("resolve");
        });

        let answer = reg
            .wait_for(id, Duration::from_secs(2))
            .await
            .expect("wait");
        assert_eq!(answer, "answered");
        resolver.await.expect("resolver task");
    }

    #[tokio::test]
    async fn wait_for_times_out() {
        let reg = AskRegistry::new();
        let id = reg.open("forever".into()).await;
        let err = reg
            .wait_for(id, Duration::from_millis(20))
            .await
            .expect_err("must timeout");
        match err {
            Error::WorkerFailed(msg) => assert!(msg.contains("timed out"), "msg={msg}"),
            other => panic!("expected WorkerFailed timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wait_for_unknown_id_errors() {
        let reg = AskRegistry::new();
        let bogus = AskId::new();
        let err = reg
            .wait_for(bogus, Duration::from_millis(20))
            .await
            .expect_err("unknown should error");
        assert!(matches!(err, Error::InvalidMandate(_)));
    }

    #[tokio::test]
    async fn resolve_idempotency_second_call_errors() {
        let reg = AskRegistry::new();
        let id = reg.open("q?".into()).await;
        reg.resolve(id, "first".into()).await.expect("first ok");
        let err = reg
            .resolve(id, "second".into())
            .await
            .expect_err("second should error");
        assert!(matches!(err, Error::InvalidMandate(_)));
    }

    #[tokio::test]
    async fn wait_for_already_resolved_returns_immediately() {
        let reg = AskRegistry::new();
        let id = reg.open("q?".into()).await;
        reg.resolve(id, "pre-answered".into())
            .await
            .expect("resolve");
        let answer = reg
            .wait_for(id, Duration::from_millis(50))
            .await
            .expect("wait");
        assert_eq!(answer, "pre-answered");
    }

    #[test]
    fn ask_id_serde_roundtrip() {
        let id = AskId::new();
        let s = serde_json::to_string(&id).expect("serialize");
        let back: AskId = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(id, back);
    }
}
