//! HTTP + SSE client for the Evy v4 daemon's operator surface.
//!
//! [`ApiClient`] wraps a `reqwest::Client` and exposes three snapshot
//! fetchers — workers, jobs, policy — plus an SSE consumer that
//! streams [`DaemonEvent`]s onto an mpsc channel for the run loop to
//! drain.
//!
//! # Wire format
//!
//! Shapes mirror `evy-comms`'s public types verbatim (`WorkerSummary`,
//! `JobSummary`, `DaemonEvent`). We re-declare them locally rather
//! than depend on `evy-comms` so that the TUI binary's dep graph stays
//! free of axum + the Telegram bridge. The contract is the wire
//! format; if `evy-comms` ever breaks it, both sides need to update.
//!
//! `PolicyView` deliberately keeps the policy document as a
//! `serde_json::Value`. The full [`evy_policy::Policy`] tree is
//! intricate (nested mode tables, allowlists, escalation rules) and
//! the TUI only renders it; pulling `evy-policy` for one struct
//! definition would couple us tightly to its internal layout.

use std::time::Duration;

use evy_core::{MandateId, ProviderKind, WorkerId, WorkerStatus};
use futures::StreamExt;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Default per-request timeout for snapshot fetches. The SSE stream
/// uses its own timeout via the client builder's lack of a global
/// `timeout()` (long-lived); only point-in-time fetches are bounded.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors surfaced by the API client.
///
/// The TUI converts these to a status-bar message rather than
/// aborting; the operator wants to *see* "daemon unreachable"
/// painted, not have the console crash.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The HTTP request failed (connection refused, DNS, timeout).
    #[error("http transport: {0}")]
    Transport(#[from] reqwest::Error),

    /// The response parsed as JSON but the shape didn't match.
    #[error("decode {endpoint}: {source}")]
    Decode {
        /// Which endpoint produced the malformed body.
        endpoint: &'static str,
        /// The underlying serde error.
        #[source]
        source: serde_json::Error,
    },

    /// The base URL was not a valid URL.
    #[error("invalid base url: {0}")]
    InvalidBaseUrl(String),

    /// An SSE frame body failed UTF-8 decoding or JSON parsing.
    #[error("sse parse: {0}")]
    Sse(String),
}

/// `Result<T, ApiError>` short-hand for callers.
pub type Result<T> = std::result::Result<T, ApiError>;

// ─── wire-shape duplicates ───────────────────────────────────────────

/// Operator-console-shaped projection of an evy-core `WorkerHandle`.
///
/// Mirrors `evy_comms::WorkerSummary`. See the producer side for
/// authoritative field docs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerSummary {
    /// Stable worker id.
    pub id: WorkerId,
    /// Which provider produced it.
    pub provider: ProviderKind,
    /// The mandate the worker is fulfilling.
    pub mandate_id: MandateId,
    /// Latest lifecycle state observed by the daemon.
    pub status: WorkerStatus,
}

/// Operator-console-shaped projection of an evy-scheduler `Job`.
///
/// Mirrors `evy_comms::JobSummary`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobSummary {
    /// Stable job id (UUID-as-string on the wire).
    pub id: serde_json::Value,
    /// Human-readable job name.
    pub name: String,
    /// 5-field cron expression.
    pub cron_expr: String,
    /// Short tag for the action kind.
    pub action_kind: String,
    /// Whether the job is currently armed.
    pub enabled: bool,
}

/// Tagged union mirroring `evy_comms::DaemonEvent`.
///
/// We keep the variant set narrow and structurally identical to the
/// producer's snake_case representation. Unknown future variants are
/// rejected at decode time; the TUI logs and skips rather than
/// dropping connection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonEvent {
    /// Daemon booted with a given version + provider set.
    DaemonBooted {
        /// Workspace version of the daemon binary.
        version: String,
        /// Which providers are wired in.
        providers: Vec<ProviderKind>,
    },
    /// A new worker handle was registered.
    WorkerRegistered {
        /// Stable id of the new worker.
        worker_id: WorkerId,
        /// Which provider produced it.
        provider: ProviderKind,
        /// The mandate the worker is fulfilling.
        mandate_id: MandateId,
    },
    /// An existing worker's lifecycle state changed.
    WorkerStatusChanged {
        /// Stable id of the worker.
        worker_id: WorkerId,
        /// New status reported by the provider.
        status: WorkerStatus,
    },
    /// The scheduler fired a registered job.
    SchedulerFired {
        /// Which job fired (UUID-as-string).
        job_id: serde_json::Value,
        /// Distinct run id.
        run_id: serde_json::Value,
        /// What the fire produced (Succeeded / Failed{reason}).
        outcome: serde_json::Value,
    },
    /// The policy gate evaluated a command.
    PolicyChecked {
        /// The command line that was checked.
        command: String,
        /// Outcome kind (`allow` / `deny` / `gated` / …).
        outcome_kind: String,
    },
    /// Periodic liveness pulse.
    Heartbeat {
        /// When the heartbeat was emitted (UTC, ISO-8601).
        ts: chrono::DateTime<chrono::Utc>,
        /// How many providers passed their last healthcheck.
        providers_healthy: usize,
    },
}

impl DaemonEvent {
    /// Short, fixed-width-ish tag suitable for a log column. Matches
    /// the JSON `type` field for the most part; abbreviated where
    /// brevity matters in a narrow terminal column.
    #[must_use]
    pub fn kind_tag(&self) -> &'static str {
        match self {
            Self::DaemonBooted { .. } => "boot",
            Self::WorkerRegistered { .. } => "worker.reg",
            Self::WorkerStatusChanged { .. } => "worker.status",
            Self::SchedulerFired { .. } => "sched.fire",
            Self::PolicyChecked { .. } => "policy.check",
            Self::Heartbeat { .. } => "heartbeat",
        }
    }

    /// Single-line human summary for the events log.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::DaemonBooted { version, providers } => {
                format!("daemon {} booted ({} providers)", version, providers.len())
            }
            Self::WorkerRegistered {
                worker_id,
                provider,
                ..
            } => format!("worker {worker_id:?} registered on {provider:?}"),
            Self::WorkerStatusChanged { worker_id, status } => {
                format!("worker {worker_id:?} -> {status:?}")
            }
            Self::SchedulerFired {
                job_id, outcome, ..
            } => format!("job {job_id} fired -> {outcome}"),
            Self::PolicyChecked {
                command,
                outcome_kind,
            } => format!("policy {outcome_kind}: {command}"),
            Self::Heartbeat {
                providers_healthy, ..
            } => format!("heartbeat ({providers_healthy} healthy)"),
        }
    }
}

/// The currently-loaded policy as the dashboard sees it. Kept as a
/// raw JSON value so the tree-view renderer can walk the structure
/// without coupling to `evy-policy`'s internal layout.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PolicyView(pub serde_json::Value);

// ─── client ──────────────────────────────────────────────────────────

/// HTTP + SSE client. Cheap to clone (wraps an `Arc` internally).
#[derive(Debug, Clone)]
pub struct ApiClient {
    http: Client,
    base: Url,
}

impl ApiClient {
    /// Build a client with the given base URL (e.g. `http://127.0.0.1:8787`).
    ///
    /// # Errors
    /// Returns [`ApiError::InvalidBaseUrl`] when `base` doesn't parse,
    /// or [`ApiError::Transport`] when the underlying reqwest client
    /// can't be constructed (rare; only on TLS init failure).
    pub fn new(base: &str) -> Result<Self> {
        let base: Url = base
            .parse()
            .map_err(|_| ApiError::InvalidBaseUrl(base.to_owned()))?;
        // Note: NO global `timeout()` on the client — the SSE stream
        // is long-lived. Per-snapshot fetches set their own timeout
        // via `RequestBuilder::timeout()`.
        let http = Client::builder()
            .user_agent(concat!("evy-tui/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { http, base })
    }

    /// The configured base URL (host + port only, no path).
    #[must_use]
    pub fn base(&self) -> &Url {
        &self.base
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        self.base
            .join(path)
            .map_err(|_| ApiError::InvalidBaseUrl(format!("{}{path}", self.base)))
    }

    /// Fetch the workers snapshot.
    ///
    /// # Errors
    /// [`ApiError::Transport`] on HTTP failure; [`ApiError::Decode`]
    /// on shape mismatch.
    pub async fn fetch_workers(&self) -> Result<Vec<WorkerSummary>> {
        let url = self.endpoint("/api/evy/workers")?;
        let body = self
            .http
            .get(url)
            .timeout(SNAPSHOT_TIMEOUT)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        serde_json::from_str(&body).map_err(|source| ApiError::Decode {
            endpoint: "workers",
            source,
        })
    }

    /// Fetch the scheduler jobs snapshot.
    ///
    /// # Errors
    /// As [`Self::fetch_workers`].
    pub async fn fetch_jobs(&self) -> Result<Vec<JobSummary>> {
        let url = self.endpoint("/api/evy/scheduler/jobs")?;
        let body = self
            .http
            .get(url)
            .timeout(SNAPSHOT_TIMEOUT)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        serde_json::from_str(&body).map_err(|source| ApiError::Decode {
            endpoint: "jobs",
            source,
        })
    }

    /// Fetch the loaded policy.
    ///
    /// # Errors
    /// As [`Self::fetch_workers`].
    pub async fn fetch_policy(&self) -> Result<PolicyView> {
        let url = self.endpoint("/api/evy/policy")?;
        let body = self
            .http
            .get(url)
            .timeout(SNAPSHOT_TIMEOUT)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let value: serde_json::Value =
            serde_json::from_str(&body).map_err(|source| ApiError::Decode {
                endpoint: "policy",
                source,
            })?;
        Ok(PolicyView(value))
    }

    /// Stream daemon events from the SSE endpoint into `out`.
    ///
    /// Runs until `shutdown` fires or the upstream stream ends.
    /// Connection errors do NOT propagate — they are logged via
    /// `tracing` and the function returns so the caller can decide
    /// whether to reconnect. The caller drives reconnect-with-backoff.
    ///
    /// `connected` fires `()` exactly once, after the SSE handshake
    /// succeeds (response status check passed) and before the event
    /// loop starts pulling frames. The supervisor maps this to a
    /// `Live` connection-state badge so the status bar transitions
    /// out of `connecting…`. The channel is dropped on return,
    /// which is harmless — a receiver that never saw a unit knows
    /// the handshake never landed.
    pub async fn stream_events(
        &self,
        out: mpsc::Sender<DaemonEvent>,
        connected: mpsc::Sender<()>,
        shutdown: CancellationToken,
    ) {
        let url = match self.endpoint("/api/evy/events") {
            Ok(u) => u,
            Err(e) => {
                tracing::error!(error = %e, "sse: invalid endpoint url");
                return;
            }
        };

        let response = match self.http.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "sse: connect failed");
                return;
            }
        };

        if let Err(e) = response.error_for_status_ref() {
            tracing::warn!(error = %e, "sse: bad status");
            return;
        }

        // Handshake complete — signal "live" before we start blocking
        // on the event stream. Send-failure means the supervisor
        // already gave up on us; nothing to do.
        let _ = connected.send(()).await;

        // `eventsource_stream` decodes the SSE framing; each `Event`
        // carries the raw `data` payload we then JSON-decode.
        let byte_stream = response.bytes_stream();
        let mut events = eventsource_stream::EventStream::new(byte_stream);

        loop {
            tokio::select! {
                () = shutdown.cancelled() => {
                    tracing::debug!("sse: shutdown signaled, draining");
                    return;
                }
                next = events.next() => {
                    match next {
                        Some(Ok(frame)) => {
                            // Skip keep-alive comments + empty frames.
                            if frame.data.is_empty() {
                                continue;
                            }
                            match serde_json::from_str::<DaemonEvent>(&frame.data) {
                                Ok(ev) => {
                                    if out.send(ev).await.is_err() {
                                        // Receiver dropped → run loop
                                        // is shutting down; exit clean.
                                        return;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, frame = %frame.data, "sse: undecodable event");
                                }
                            }
                        }
                        Some(Err(e)) => {
                            tracing::warn!(error = %e, "sse: stream error");
                            return;
                        }
                        None => {
                            tracing::info!("sse: stream ended");
                            return;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_summary_roundtrips_through_json() {
        let w = WorkerSummary {
            id: WorkerId::new(),
            provider: ProviderKind::ClaudeCode,
            mandate_id: MandateId::new(),
            status: WorkerStatus::Running,
        };
        let s = serde_json::to_string(&w).unwrap();
        let back: WorkerSummary = serde_json::from_str(&s).unwrap();
        assert_eq!(w, back);
    }

    #[test]
    fn daemon_event_tag_is_snake_case() {
        let ev = DaemonEvent::PolicyChecked {
            command: "ls".into(),
            outcome_kind: "allow".into(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"type\":\"policy_checked\""));
        let back: DaemonEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn daemon_event_kind_tags_are_stable() {
        let heartbeat = DaemonEvent::Heartbeat {
            ts: chrono::Utc::now(),
            providers_healthy: 0,
        };
        assert_eq!(heartbeat.kind_tag(), "heartbeat");
        assert!(heartbeat.summary().contains("healthy"));
    }

    #[test]
    fn client_rejects_invalid_base() {
        let err = ApiClient::new("not a url").unwrap_err();
        assert!(matches!(err, ApiError::InvalidBaseUrl(_)));
    }

    #[test]
    fn client_accepts_loopback_url() {
        let c = ApiClient::new("http://127.0.0.1:8787").unwrap();
        assert_eq!(c.base().as_str(), "http://127.0.0.1:8787/");
    }
}
