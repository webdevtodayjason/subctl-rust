//! Server-Sent Events broadcaster.
//!
//! [`EventBroadcaster`] is the daemon-wide fan-out point for
//! [`crate::DaemonEvent`]s. Internally it's a `tokio::sync::broadcast`
//! channel, which is the right shape because:
//!
//! 1. Multiple SSE clients connect; each gets a [`broadcast::Receiver`].
//! 2. Slow clients lag rather than back-pressuring producers — a
//!    misbehaving browser must not stall the daemon.
//! 3. Zero clients is a valid steady state — `emit` is cheap when no one
//!    is listening (per `broadcast::Sender` semantics).
//!
//! When a new client connects the SSE handler just starts forwarding
//! live events; we deliberately do NOT replay a synthetic
//! `DaemonBooted` from the broadcaster's buffer. The daemon emits one
//! `DaemonBooted` at startup; reconnecting clients receive the next
//! `Heartbeat` within seconds, which is sufficient for "is the daemon
//! alive?". Replaying old events would invite double-handling of
//! state-changing events on every reconnect.

use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::Stream;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::events::DaemonEvent;

/// Fan-out point for `DaemonEvent`s.
///
/// Construct one per daemon process. Clone freely — the underlying
/// `broadcast::Sender` is internally reference-counted.
#[derive(Clone)]
pub struct EventBroadcaster {
    tx: broadcast::Sender<DaemonEvent>,
}

impl EventBroadcaster {
    /// Build a new broadcaster with the given per-subscriber buffer
    /// capacity. A slow subscriber that falls more than `capacity`
    /// events behind sees `RecvError::Lagged` and we drop the oldest
    /// undelivered events — the SSE handler logs and skips lagged
    /// events rather than disconnecting the client.
    ///
    /// `capacity` of 0 panics inside `broadcast::channel`; callers
    /// should pick a sensible production value (256+).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Broadcast an event to every connected subscriber.
    ///
    /// Returns silently when no subscribers are listening — by design,
    /// per the module-level rationale.
    pub fn emit(&self, event: DaemonEvent) {
        // `send` only errors when there are zero receivers; that's a
        // perfectly normal steady state for the daemon. Drop the result.
        let _ = self.tx.send(event);
    }

    /// Subscribe to the live event stream. Each call mints a fresh
    /// receiver; the caller is responsible for either draining it or
    /// dropping it.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.tx.subscribe()
    }

    /// True iff the broadcaster has at least one live subscriber. Used
    /// by the daemon's "is anyone watching?" diagnostics.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for EventBroadcaster {
    /// Default capacity = 256. Generous enough that a few seconds of
    /// burst events don't lag the slowest realistic subscriber.
    fn default() -> Self {
        Self::new(256)
    }
}

/// Turn a `broadcast::Receiver<DaemonEvent>` into the axum SSE response
/// shape. Each event becomes an SSE `data:` frame carrying the JSON
/// encoding of the event. Lagged events (slow client) are logged and
/// skipped; the connection stays open.
///
/// `KeepAlive` emits a `:` comment every 15 seconds so a quiet stream
/// doesn't get reaped by intermediate proxies / browsers.
pub fn into_sse_response(
    rx: broadcast::Receiver<DaemonEvent>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(rx).filter_map(|item| match item {
        // `DashboardFrame` is a pre-formatted NAMED SSE event (absorbed from
        // the v3 BFF): emit `event: <event>\ndata: <data>` verbatim rather than
        // the default-event `{type,...}` shape the monitoring variants use.
        Ok(DaemonEvent::DashboardFrame { event, data }) => {
            Some(Ok(Event::default().event(event).data(data)))
        }
        Ok(ev) => match Event::default().json_data(&ev) {
            Ok(frame) => Some(Ok(frame)),
            Err(e) => {
                tracing::warn!(error = %e, "failed to serialize DaemonEvent for SSE; dropping");
                None
            }
        },
        Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(skipped)) => {
            tracing::warn!(skipped, "SSE subscriber lagged; events dropped");
            None
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tokio::time::{timeout, Duration as TokioDuration};

    fn sample_event() -> DaemonEvent {
        DaemonEvent::Heartbeat {
            ts: Utc::now(),
            providers_healthy: 1,
        }
    }

    #[tokio::test]
    async fn emit_delivers_to_subscriber() {
        let b = EventBroadcaster::new(16);
        let mut rx = b.subscribe();
        b.emit(sample_event());
        let got = timeout(TokioDuration::from_millis(100), rx.recv())
            .await
            .expect("recv timed out")
            .expect("recv returned err");
        assert!(matches!(got, DaemonEvent::Heartbeat { .. }));
    }

    #[tokio::test]
    async fn emit_with_no_subscribers_does_not_panic() {
        let b = EventBroadcaster::new(8);
        b.emit(sample_event());
        assert_eq!(b.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn each_subscriber_sees_each_event() {
        let b = EventBroadcaster::new(8);
        let mut rx1 = b.subscribe();
        let mut rx2 = b.subscribe();
        b.emit(sample_event());
        let g1 = timeout(TokioDuration::from_millis(100), rx1.recv())
            .await
            .unwrap()
            .unwrap();
        let g2 = timeout(TokioDuration::from_millis(100), rx2.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(g1, DaemonEvent::Heartbeat { .. }));
        assert!(matches!(g2, DaemonEvent::Heartbeat { .. }));
    }

    #[tokio::test]
    async fn cloned_broadcaster_shares_state() {
        let b = EventBroadcaster::new(8);
        let mut rx = b.subscribe();
        let b2 = b.clone();
        b2.emit(sample_event());
        let got = timeout(TokioDuration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(got, DaemonEvent::Heartbeat { .. }));
    }

    #[test]
    fn default_uses_reasonable_capacity() {
        let b = EventBroadcaster::default();
        assert_eq!(b.subscriber_count(), 0);
    }
}
