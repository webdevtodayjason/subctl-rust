//! Decision-time retrieval surface.
//!
//! Layer 6 of the learning loop: before any dispatch / notification /
//! plan, pull the relevant priors. Phase 2 ships the *trait* and a
//! deliberately naive substring-matching implementation so the daemon
//! has something to call. Phase 3 may swap in embedding-based retrieval;
//! the trait isolates that.
//!
//! ADR 0020 §"Layer 6 — Decision-time retrieval". Performance contract
//! there (sub-100ms, cacheable, fail-open) is not yet enforced — Phase 3
//! work.

use std::sync::Arc;

use async_trait::async_trait;
use evy_core::Result;

use crate::claude_mem::{ClaudeMemReader, Episode};
use crate::observation::Observation;
use crate::observation_log::ObservationLog;
use crate::playbook::{Playbook, PlaybookStore};

/// A single retrieved prior. Callers decide how much weight to give each
/// variant for their decision.
#[derive(Debug, Clone)]
pub enum RetrievedItem {
    /// Cross-session prior from claude-mem.
    Episode(Episode),
    /// Same-session prior from Evy's own observation log.
    Observation(Observation),
    /// Operator-authored procedure.
    Playbook(Playbook),
}

/// Pluggable retrieval surface. Implementations must be `Send + Sync` so
/// they can sit behind an `Arc<dyn Retriever>` on the daemon's wiring
/// graph.
#[async_trait]
pub trait Retriever: Send + Sync {
    /// Pull up to `limit` priors that look relevant to `query`. The
    /// query is treated as a free-form situation string ("operator just
    /// said X", "about to dispatch Y", …).
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] if any underlying source fails.
    async fn retrieve(&self, query: &str, limit: usize) -> Result<Vec<RetrievedItem>>;
}

/// Naive substring-matching retriever. The three sources are queried in
/// parallel; results are interleaved (playbooks → observations →
/// episodes) and truncated to `limit`.
///
/// This is the substrate the team-protocol slice closes against. Layer 6
/// of the learning loop (sub-100ms, cacheable, ranked) is Phase 3 work.
#[derive(Clone)]
pub struct NaiveRetriever {
    /// Evy's own append-only event log. Always present.
    pub log: Arc<ObservationLog>,
    /// Cross-session corpus. Optional — the consumer fails open when
    /// `claude-mem` isn't installed.
    pub claude_mem: Option<Arc<ClaudeMemReader>>,
    /// Operator-authored procedures snapshot.
    pub playbooks: Arc<PlaybookStore>,
}

impl NaiveRetriever {
    /// Convenience constructor.
    #[must_use]
    pub fn new(
        log: Arc<ObservationLog>,
        claude_mem: Option<Arc<ClaudeMemReader>>,
        playbooks: Arc<PlaybookStore>,
    ) -> Self {
        Self {
            log,
            claude_mem,
            playbooks,
        }
    }
}

#[async_trait]
impl Retriever for NaiveRetriever {
    async fn retrieve(&self, query: &str, limit: usize) -> Result<Vec<RetrievedItem>> {
        let mut out: Vec<RetrievedItem> = Vec::new();

        // 1. Playbooks via trigger substring (Vec<&Playbook> → owned
        //    clones to satisfy the `RetrievedItem` variant). Cheap; the
        //    store holds them all in memory.
        for pb in self.playbooks.matching_trigger(query) {
            out.push(RetrievedItem::Playbook(pb.clone()));
            if out.len() >= limit {
                return Ok(out);
            }
        }

        // 2. Recent observations whose `kind` discriminator appears
        //    inside the query, or — if no match — just the most recent
        //    rows. Substring is naive on purpose: Phase 3 may swap to an
        //    embedding-based ranker behind the same trait.
        let lower = query.to_lowercase();
        let observations = self.log.query_recent(limit.saturating_mul(2)).await?;
        for obs in observations {
            let discriminator = obs.kind.discriminator();
            if lower.is_empty() || lower.contains(discriminator) || query.is_empty() {
                out.push(RetrievedItem::Observation(obs));
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }

        // 3. Cross-session priors via claude-mem FTS5. Fail-open: if the
        //    reader errors, we proceed with what we already have. ADR
        //    0020 §"Layer 6" mandates fail-open behaviour.
        if let Some(claude_mem) = self.claude_mem.as_ref() {
            let remaining = limit.saturating_sub(out.len());
            if remaining > 0 {
                match claude_mem.search(query, remaining).await {
                    Ok(eps) => {
                        for ep in eps {
                            out.push(RetrievedItem::Episode(ep));
                            if out.len() >= limit {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "claude-mem search failed; retrieval continuing without cross-session priors"
                        );
                    }
                }
            }
        }

        Ok(out)
    }
}

// TODO: Phase 3 — feedback ingest. Operator corrections should write a
//   correction-typed observation that biases subsequent retrieval. Today
//   the substrate has no notion of "this was wrong"; layer 7 closes the
//   loop.
// TODO: Phase 3 — operator-preference auto-update. Layer 5 of the spec.
// TODO: Phase 3 — worker-effectiveness scoring (provider × task-class
//   success rates). Layer 3 of the spec. Reads from the observation log
//   built here, writes back another observation kind.
// TODO: Phase 3 — sub-100ms perf contract + retrieval cache (ADR 0020
//   §"Layer 6"). The naive substrate is fine for correctness; the cache
//   is what makes the hot path live up to its contract.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::ObservationKind;
    use evy_core::{MandateId, ProviderKind, WorkerId};
    use tempfile::tempdir;

    async fn fixture() -> (tempfile::TempDir, NaiveRetriever) {
        let dir = tempdir().unwrap();

        // Observation log with one dispatch row.
        let log_path = dir.path().join("obs.db");
        let log = Arc::new(ObservationLog::open(&log_path).await.unwrap());
        log.append(Observation::new(ObservationKind::WorkerDispatched {
            worker_id: WorkerId::new(),
            mandate_id: MandateId::new(),
            provider: ProviderKind::ClaudeCode,
        }))
        .await
        .unwrap();

        // Playbook store with one rate-limit playbook.
        let pb_dir = dir.path().join("playbooks");
        std::fs::create_dir(&pb_dir).unwrap();
        std::fs::write(
            pb_dir.join("drain-rate-limited-worker.md"),
            "---\nname: drain-rate-limited-worker\ntriggers: [\"rate limit hit\", \"429\"]\n---\n\n# Drain and re-route\n",
        )
        .unwrap();
        let playbooks = Arc::new(PlaybookStore::load(&pb_dir).unwrap());

        let retriever = NaiveRetriever::new(log, None, playbooks);
        (dir, retriever)
    }

    #[tokio::test]
    async fn retrieves_matching_playbook_first() {
        let (_dir, retriever) = fixture().await;
        let items = retriever
            .retrieve("worker hit 429 in claude-code", 5)
            .await
            .unwrap();
        // First hit is the playbook, by construction of the naive impl.
        assert!(matches!(items.first(), Some(RetrievedItem::Playbook(_))));
    }

    #[tokio::test]
    async fn returns_observations_when_query_mentions_kind() {
        let (_dir, retriever) = fixture().await;
        let items = retriever
            .retrieve("any worker_dispatched lately?", 5)
            .await
            .unwrap();
        // At least one observation comes back when the query namedrops
        // the discriminator string.
        assert!(items
            .iter()
            .any(|it| matches!(it, RetrievedItem::Observation(_))));
    }

    #[tokio::test]
    async fn limit_truncates_results() {
        let (_dir, retriever) = fixture().await;
        let items = retriever.retrieve("anything", 0).await.unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn fail_open_when_claude_mem_absent() {
        // No `claude_mem` field on the retriever; retrieval still works.
        let (_dir, retriever) = fixture().await;
        let items = retriever.retrieve("anything", 5).await.unwrap();
        // No panic, no error, just whatever the log + playbooks can offer.
        let _ = items.len();
    }
}
