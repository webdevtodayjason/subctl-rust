//! Tier 4 (Cognee) HTTP client — Phase 4 Slice E.
//!
//! Cognee is the durable, queryable graph memory layer that sits on top
//! of Tier 3 (Memori, [`ObservationLog`]). The v3 TypeScript
//! `cognee-client.ts` exposed four canonical operations (`remember`,
//! `recall`, `forget`, `cognify`) over a local HTTP service; this Rust
//! port wraps the two we need for Phase 4:
//!
//! - [`CogneeClient::add_observation`] → `POST /remember` — promote one
//!   observation into the graph with provenance metadata.
//! - [`CogneeClient::query`] → `POST /recall` — semantic retrieval.
//! - [`CogneeClient::health`] → `GET /health` — reachability probe.
//!
//! The promotion ticker ([`crate::promotion::PromotionTicker`]) is the
//! only caller of `add_observation` in Phase 4. `query` is exposed so
//! the retrieval layer can hit Tier 4 directly once it grows a Cognee
//! adapter (Phase 5).
//!
//! Configured via [`CogneeConfig`]. The default endpoint mirrors v3
//! (`http://127.0.0.1:8745`). Operator overrides land via
//! `~/.config/subctl/cognee.json` in v3; the Rust callers thread the
//! resolved string in via `CogneeConfig::endpoint` — wiring lives at the
//! daemon edge (out of scope for this crate).
//!
//! # TODO: Phase 5
//!
//! - LLM-assisted scoring on the way in (see [`crate::promotion`]).
//! - Persist the in-memory promotion watermark.
//! - Verify `/recall` response shape against a live Cognee instance —
//!   the v3 TS surface treated `hits[].score` as optional/free-shape;
//!   this client tolerates missing scores by defaulting to `0.0`.

use std::time::Duration;

use evy_core::{Error, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error;
use crate::observation::Observation;

// ── shared metadata key — round-tripped through Cognee ──────────────

/// Metadata key under which we store the originating observation id on
/// the way in. The same key is parsed back out of
/// `hits[].metadata.source_obs_id` on the way out so callers can trace
/// a Cognee hit back to a Tier-3 observation row.
const META_SOURCE_OBS_ID: &str = "source_obs_id";

// ── config ──────────────────────────────────────────────────────────

/// Connection configuration for [`CogneeClient`].
///
/// The default endpoint matches v3 (`http://127.0.0.1:8745`). Operator
/// overrides should be threaded in at the daemon edge — this crate
/// stays oblivious to the filesystem.
#[derive(Debug, Clone)]
pub struct CogneeConfig {
    /// Base URL of the local Cognee HTTP service, e.g.
    /// `http://127.0.0.1:8745`. Trailing slashes are normalised away
    /// when the client is built.
    pub endpoint: String,
    /// Bearer token. `None` for fully-local dev with no auth gate.
    pub api_key: Option<String>,
    /// Per-request timeout, in seconds. Applied uniformly to all three
    /// endpoints — `add_observation`, `query`, `health`.
    pub timeout_secs: u64,
}

impl Default for CogneeConfig {
    fn default() -> Self {
        Self {
            // v3 default — see components/evy/cognee-client.ts:100.
            endpoint: "http://127.0.0.1:8745".to_string(),
            api_key: None,
            timeout_secs: 15,
        }
    }
}

// ── public types ────────────────────────────────────────────────────

/// One row out of a Cognee `/recall` response.
#[derive(Debug, Clone, PartialEq)]
pub struct CogneeMatch {
    /// The matched text or summary the graph surfaced.
    pub content: String,
    /// Relevance score, `0.0`–`1.0` if Cognee normalised it. Defaults
    /// to `0.0` when the server omits the field — callers should treat
    /// "missing score" as "unknown relevance" rather than "irrelevant".
    pub score: f32,
    /// Originating observation id, parsed back out of the metadata bag
    /// that [`CogneeClient::add_observation`] put in on the way in.
    /// `None` when the hit didn't carry it (e.g. legacy v3 backfill
    /// rows, or non-promotion writes from a different consumer).
    pub source_obs_id: Option<Uuid>,
}

// ── wire types ──────────────────────────────────────────────────────
//
// Kept private to the module — callers see `CogneeMatch`, not the raw
// `/recall` shape. Field names match v3 `cognee-client.ts` so the
// service requires no adapter.

#[derive(Debug, Serialize)]
struct RememberRequest<'a> {
    text: String,
    metadata: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    dataset: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct RememberResponse {
    #[allow(dead_code)] // Cognee sometimes omits — we don't surface it.
    id: Option<String>,
}

#[derive(Debug, Serialize)]
struct RecallRequest<'a> {
    query: &'a str,
    top_k: u32,
}

#[derive(Debug, Deserialize)]
struct RecallResponse {
    #[serde(default)]
    hits: Vec<RecallHit>,
}

#[derive(Debug, Deserialize)]
struct RecallHit {
    text: Option<String>,
    score: Option<f32>,
    #[serde(default)]
    metadata: serde_json::Value,
}

// ── client ──────────────────────────────────────────────────────────

/// HTTP client bound to a single Cognee endpoint.
///
/// Cheap to clone — internally an `Arc`-shared [`reqwest::Client`]
/// connection pool plus a base URL string. Pass `Arc<CogneeClient>`
/// around when sharing across tasks (the promotion ticker does this).
#[derive(Debug, Clone)]
pub struct CogneeClient {
    endpoint: String,
    api_key: Option<String>,
    http: reqwest::Client,
}

impl CogneeClient {
    /// Build a client from `config`. The endpoint's trailing slash is
    /// normalised away so callers can compose paths safely.
    #[must_use]
    pub fn new(config: CogneeConfig) -> Self {
        let endpoint = config.endpoint.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .expect("reqwest::Client::build with no custom TLS / proxy must succeed");
        Self {
            endpoint,
            api_key: config.api_key,
            http,
        }
    }

    /// Base endpoint URL (no trailing slash). Useful for status pages.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// `GET /health`. Returns `Ok(true)` iff Cognee responded 2xx.
    /// Transport failures (DNS, refused connection, timeout) yield
    /// `Ok(false)` so the daemon's status surface can render a single
    /// reachable / unreachable bit without try/catching.
    ///
    /// # Errors
    /// This method intentionally does not surface transport errors as
    /// `Err`; the only `Err` path is a logically-impossible internal
    /// failure (currently unreachable). Future versions may surface
    /// auth failures (401/403) as `Err` to distinguish "configured
    /// wrong" from "service down".
    pub async fn health(&self) -> Result<bool> {
        let url = format!("{}/health", self.endpoint);
        let req = self.http.get(&url);
        let req = if let Some(token) = &self.api_key {
            req.bearer_auth(token)
        } else {
            req
        };
        match req.send().await {
            Ok(resp) => Ok(resp.status().is_success()),
            Err(e) => {
                tracing::debug!(error = %e, url = %url, "cognee /health probe failed");
                Ok(false)
            }
        }
    }

    /// `POST /remember` — promote one observation into Cognee.
    ///
    /// The observation is serialised to a single line of text via
    /// [`render_observation_text`]; provenance (kind discriminator,
    /// timestamp, correlation id, and the observation UUID under
    /// `source_obs_id`) is attached as structured metadata so future
    /// `/recall` responses can trace hits back to a Tier-3 row.
    ///
    /// # Errors
    /// - [`Error::Io`] for transport failures.
    /// - [`Error::Io`] for non-2xx HTTP responses (the body is included
    ///   in the message, truncated to 400 chars).
    /// - [`Error::Serde`] for response-body JSON parse failures.
    pub async fn add_observation(&self, obs: &Observation) -> Result<()> {
        let url = format!("{}/remember", self.endpoint);
        let body = RememberRequest {
            text: render_observation_text(obs),
            metadata: build_observation_metadata(obs),
            dataset: None,
        };
        let req = self.http.post(&url).json(&body);
        let req = if let Some(token) = &self.api_key {
            req.bearer_auth(token)
        } else {
            req
        };
        let resp = req.send().await.map_err(error::from_reqwest)?;
        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            return Err(error::cognee_status(
                status.as_u16(),
                detail.chars().take(400).collect::<String>(),
            ));
        }
        // The response body is ignored on success — Cognee may or may
        // not echo an id, and we don't surface it. We still parse it
        // (and tolerate a missing body) so a malformed response is
        // surfaced as `Error::Serde` rather than silently ignored.
        let bytes = resp.bytes().await.map_err(error::from_reqwest)?;
        if !bytes.is_empty() {
            let _: RememberResponse = serde_json::from_slice(&bytes)?;
        }
        Ok(())
    }

    /// `POST /recall` — semantic retrieval against the graph.
    ///
    /// `top_k` defaults to 10, matching v3.
    ///
    /// # Errors
    /// - [`Error::Io`] for transport failures.
    /// - [`Error::Io`] for non-2xx HTTP responses.
    /// - [`Error::Serde`] for response-body JSON parse failures.
    pub async fn query(&self, query: &str) -> Result<Vec<CogneeMatch>> {
        let url = format!("{}/recall", self.endpoint);
        let body = RecallRequest { query, top_k: 10 };
        let req = self.http.post(&url).json(&body);
        let req = if let Some(token) = &self.api_key {
            req.bearer_auth(token)
        } else {
            req
        };
        let resp = req.send().await.map_err(error::from_reqwest)?;
        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            return Err(error::cognee_status(
                status.as_u16(),
                detail.chars().take(400).collect::<String>(),
            ));
        }
        let parsed: RecallResponse = resp.json().await.map_err(error::from_reqwest)?;
        let out = parsed
            .hits
            .into_iter()
            .map(|hit| CogneeMatch {
                content: hit.text.unwrap_or_default(),
                score: hit.score.unwrap_or(0.0),
                source_obs_id: extract_source_obs_id(&hit.metadata),
            })
            .collect();
        Ok(out)
    }
}

// ── helpers ─────────────────────────────────────────────────────────

/// Render one observation to a single-line text representation that
/// Cognee can store + embed. Kept stable so future ports can grep for
/// existing rows: `"[kind] <human-readable summary>"`.
fn render_observation_text(obs: &Observation) -> String {
    use crate::observation::ObservationKind as K;
    let head = obs.kind.discriminator();
    let summary = match &obs.kind {
        K::WorkerDispatched {
            worker_id,
            mandate_id,
            provider,
        } => format!(
            "worker {} dispatched on {:?} for mandate {}",
            worker_id.0, provider, mandate_id.0
        ),
        K::WorkerCompleted { worker_id, status } => {
            format!("worker {} completed: {:?}", worker_id.0, status)
        }
        K::PolicyChecked { command, outcome } => {
            format!("policy check `{command}` → {outcome}")
        }
        K::SchedulerFiredJob { job_name, outcome } => {
            format!("scheduler `{job_name}` → {outcome}")
        }
        K::OperatorMessage { channel, text } => {
            format!("operator via {channel}: {text}")
        }
        K::DaemonBooted { version } => format!("daemon booted v{version}"),
        K::DaemonShutdown { reason } => format!("daemon shutdown ({reason})"),
        K::FeedbackReceived {
            feedback_id,
            feedback_kind,
        } => format!("feedback {feedback_kind} ({feedback_id})"),
    };
    format!("[{head}] {summary}")
}

/// Build the metadata bag posted to `/remember`. The shape matches
/// what [`CogneeClient::query`] expects to read back out, anchored by
/// [`META_SOURCE_OBS_ID`].
fn build_observation_metadata(obs: &Observation) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert(
        META_SOURCE_OBS_ID.to_string(),
        serde_json::Value::String(obs.id.to_string()),
    );
    map.insert(
        "kind".to_string(),
        serde_json::Value::String(obs.kind.discriminator().to_string()),
    );
    map.insert(
        "ts".to_string(),
        serde_json::Value::String(obs.ts.to_rfc3339()),
    );
    if let Some(corr) = obs.correlation_id {
        map.insert(
            "correlation_id".to_string(),
            serde_json::Value::String(corr.to_string()),
        );
    }
    // Carry through any user-attached metadata pairs. Stored under
    // `metadata` so the key namespace stays flat at the top level.
    if !obs.metadata.is_empty() {
        let nested: serde_json::Map<String, serde_json::Value> = obs
            .metadata
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        map.insert("metadata".to_string(), serde_json::Value::Object(nested));
    }
    serde_json::Value::Object(map)
}

/// Pull the originating observation id back out of a `/recall` hit's
/// metadata bag. Tolerates absence + malformed UUIDs (returns `None`).
fn extract_source_obs_id(metadata: &serde_json::Value) -> Option<Uuid> {
    metadata
        .get(META_SOURCE_OBS_ID)?
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
}

// Allow `Error` import only for doc links / future use without cluttering
// the public surface.
#[allow(dead_code)]
fn _doc_link_anchor(_e: Error) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::ObservationKind;
    use evy_core::WorkerId;

    #[test]
    fn default_endpoint_matches_v3() {
        let cfg = CogneeConfig::default();
        assert_eq!(cfg.endpoint, "http://127.0.0.1:8745");
        assert!(cfg.api_key.is_none());
        assert_eq!(cfg.timeout_secs, 15);
    }

    #[test]
    fn trailing_slash_is_normalised() {
        let c = CogneeClient::new(CogneeConfig {
            endpoint: "http://localhost:8745///".into(),
            api_key: None,
            timeout_secs: 5,
        });
        assert_eq!(c.endpoint(), "http://localhost:8745");
    }

    #[test]
    fn render_text_includes_discriminator_prefix() {
        let obs = Observation::new(ObservationKind::DaemonBooted {
            version: "0.1.0".into(),
        });
        let text = render_observation_text(&obs);
        assert!(text.starts_with("[daemon_booted]"), "got: {text}");
        assert!(text.contains("0.1.0"));
    }

    #[test]
    fn metadata_round_trips_source_obs_id() {
        let obs = Observation::new(ObservationKind::WorkerCompleted {
            worker_id: WorkerId::new(),
            status: evy_core::WorkerStatus::Succeeded,
        });
        let meta = build_observation_metadata(&obs);
        let parsed = extract_source_obs_id(&meta).expect("should round-trip");
        assert_eq!(parsed, obs.id);
    }

    #[test]
    fn metadata_carries_correlation_id_when_set() {
        let corr = Uuid::new_v4();
        let obs = Observation::new(ObservationKind::OperatorMessage {
            channel: "telegram".into(),
            text: "hi".into(),
        })
        .with_correlation(corr);
        let meta = build_observation_metadata(&obs);
        assert_eq!(
            meta.get("correlation_id").and_then(|v| v.as_str()),
            Some(corr.to_string().as_str())
        );
    }

    #[test]
    fn extract_source_obs_id_tolerates_missing_or_malformed() {
        let v = serde_json::json!({});
        assert!(extract_source_obs_id(&v).is_none());
        let v = serde_json::json!({ "source_obs_id": "not-a-uuid" });
        assert!(extract_source_obs_id(&v).is_none());
    }
}
