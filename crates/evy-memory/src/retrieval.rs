//! Decision-time retrieval surface.
//!
//! Layer 6 of the learning loop: before any dispatch / notification /
//! plan, pull the relevant priors. Phase 2 shipped the *trait* and a
//! deliberately naive substring-matching implementation; Phase 3 (this
//! slice) wires the three new learning-loop layers into the same
//! retriever so callers see everything through one API:
//!
//! - Playbooks (layer 4) — operator-authored procedures.
//! - Observations (layer 1) — same-session priors.
//! - Worker-effectiveness scores (layer 3) — surfaced when the query
//!   names a known task class.
//! - Operator preferences (layer 5) — surfaced when the query mentions
//!   a key substring.
//! - Recent feedback (layer 7) — surfaced as time-ordered priors.
//! - claude-mem episodes (layer 2) — cross-session priors.
//!
//! Ranking is still naive: playbooks first, then scores, preferences,
//! observations, feedback, episodes. Phase 4 may swap in an
//! embedding-based ranker behind the same trait.
//!
//! ADR 0020 §"Layer 6 — Decision-time retrieval". The sub-100ms
//! perf-contract / caching requirement is still Phase 4 work.

use std::sync::Arc;

use async_trait::async_trait;
use evy_core::Result;

use crate::claude_mem::{ClaudeMemReader, Episode};
use crate::feedback::{Feedback, FeedbackIngest};
use crate::observation::Observation;
use crate::observation_log::ObservationLog;
use crate::playbook::{Playbook, PlaybookStore};
use crate::preferences::{OperatorPreferenceModel, PreferenceKey, PreferenceValue};
use crate::scoring::{ScoreLedger, WorkerEffectivenessScore};

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
    /// Per-provider × per-task-class effectiveness row (layer 3).
    Score(WorkerEffectivenessScore),
    /// Operator-preference key/value pair (layer 5).
    Preference(PreferenceKey, PreferenceValue),
    /// Recent operator-feedback event (layer 7).
    Feedback(Feedback),
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

/// Naive substring-matching retriever. The six sources are interleaved
/// in priority order:
///
/// 1. Playbooks (trigger substring match)
/// 2. Worker-effectiveness scores (when the query mentions a known
///    task class)
/// 3. Operator preferences (key substring match)
/// 4. Recent observations (kind discriminator substring match)
/// 5. Recent feedback (most-recent first)
/// 6. claude-mem episodes (FTS5 search) — fail-open
///
/// Phase 4 may swap an embedding-based ranker behind the same trait.
#[derive(Clone)]
pub struct NaiveRetriever {
    /// Evy's own append-only event log. Always present.
    pub log: Arc<ObservationLog>,
    /// Cross-session corpus. Optional — the consumer fails open when
    /// `claude-mem` isn't installed.
    pub claude_mem: Option<Arc<ClaudeMemReader>>,
    /// Operator-authored procedures snapshot.
    pub playbooks: Arc<PlaybookStore>,
    /// Worker-effectiveness scoring ledger (layer 3). Optional —
    /// absence is treated as "no scoring data available", not an error.
    pub scoring: Option<Arc<ScoreLedger>>,
    /// Operator-preference model (layer 5). Optional — absence is
    /// treated as "no preference data available".
    pub preferences: Option<Arc<OperatorPreferenceModel>>,
    /// Feedback ingest store (layer 7); used here in read-only mode
    /// for `recent` and `by_kind` queries. Optional.
    pub feedback: Option<Arc<FeedbackIngest>>,
    /// List of known task class labels (`code_change`,
    /// `investigation`, …). When the query contains any of these as a
    /// substring, the matching scores are surfaced. Empty list disables
    /// the score branch.
    pub known_task_classes: Vec<String>,
}

impl NaiveRetriever {
    /// Construct a retriever with only the Phase 2C substrate
    /// (observations / claude-mem / playbooks). Phase 3 sources are
    /// `None`; the resulting retriever behaves exactly as the Phase 2C
    /// implementation did.
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
            scoring: None,
            preferences: None,
            feedback: None,
            known_task_classes: Vec::new(),
        }
    }

    /// Builder: attach a [`ScoreLedger`].
    #[must_use]
    pub fn with_scoring(mut self, scoring: Arc<ScoreLedger>) -> Self {
        self.scoring = Some(scoring);
        self
    }

    /// Builder: attach an [`OperatorPreferenceModel`].
    #[must_use]
    pub fn with_preferences(mut self, preferences: Arc<OperatorPreferenceModel>) -> Self {
        self.preferences = Some(preferences);
        self
    }

    /// Builder: attach a [`FeedbackIngest`] (used read-only here).
    #[must_use]
    pub fn with_feedback(mut self, feedback: Arc<FeedbackIngest>) -> Self {
        self.feedback = Some(feedback);
        self
    }

    /// Builder: declare the task-class taxonomy the retriever should
    /// look for inside queries. Substring-match, case-insensitive.
    #[must_use]
    pub fn with_known_task_classes(mut self, classes: Vec<String>) -> Self {
        self.known_task_classes = classes;
        self
    }
}

#[async_trait]
impl Retriever for NaiveRetriever {
    async fn retrieve(&self, query: &str, limit: usize) -> Result<Vec<RetrievedItem>> {
        let mut out: Vec<RetrievedItem> = Vec::new();

        // 1. Playbooks via trigger substring.
        for pb in self.playbooks.matching_trigger(query) {
            out.push(RetrievedItem::Playbook(pb.clone()));
            if out.len() >= limit {
                return Ok(out);
            }
        }

        let lower = query.to_lowercase();

        // 2. Scores — only when the query mentions a known task class.
        if let Some(scoring) = self.scoring.as_ref() {
            for class in &self.known_task_classes {
                if lower.contains(&class.to_lowercase()) {
                    match scoring.scores_for_task(class).await {
                        Ok(scores) => {
                            for s in scores {
                                out.push(RetrievedItem::Score(s));
                                if out.len() >= limit {
                                    return Ok(out);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                task_class = %class,
                                "score lookup failed; retrieval continuing"
                            );
                        }
                    }
                }
            }
        }

        // 3. Preferences — surface preferences whose key appears in the
        //    query. We use a token-style match: split the query on
        //    whitespace and look each token up as a substring.
        if let Some(prefs) = self.preferences.as_ref() {
            let mut seen_keys = std::collections::HashSet::new();
            for token in lower.split_whitespace() {
                if token.is_empty() {
                    continue;
                }
                match prefs.search_keys(token).await {
                    Ok(hits) => {
                        for (k, v) in hits {
                            if seen_keys.insert(k.0.clone()) {
                                out.push(RetrievedItem::Preference(k, v));
                                if out.len() >= limit {
                                    return Ok(out);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            token = %token,
                            "preference lookup failed; retrieval continuing"
                        );
                    }
                }
            }
        }

        // 4. Recent observations whose `kind` discriminator appears
        //    inside the query, or — if no match — just the most recent
        //    rows. Substring is naive on purpose.
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

        // 5. Recent feedback (layer 7). Bounded to avoid drowning out
        //    other priors when the operator has been chatty.
        if let Some(feedback) = self.feedback.as_ref() {
            let remaining = limit.saturating_sub(out.len());
            if remaining > 0 {
                match feedback.recent(remaining.min(8)).await {
                    Ok(rows) => {
                        for fb in rows {
                            out.push(RetrievedItem::Feedback(fb));
                            if out.len() >= limit {
                                return Ok(out);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "feedback lookup failed; retrieval continuing"
                        );
                    }
                }
            }
        }

        // 6. Cross-session priors via claude-mem FTS5. Fail-open.
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

// TODO: Phase 4 — embedding-based ranker. The naive substring substrate
//   is correct but bound to lexical match. A small local embedding
//   model + cosine ranking behind the same `Retriever` trait would let
//   "the dispatch keeps timing out" find playbooks tagged `429` and
//   `rate limit hit` without keyword agreement.
// TODO: Phase 4 — LLM-driven playbook distillation. ADR 0020 §"Layer
//   4" defers the question of how new playbooks are created; the
//   natural source is session_summaries from claude-mem, distilled by
//   an LLM into operator-reviewable markdown drafts.
// TODO: Phase 4 — cross-machine sync. Today every store is
//   single-machine; the operator runs the daemon on the local M3 and
//   the M3 in the lab independently. Shared state (especially
//   preferences and scoring) needs a sync story before v4 ships beyond
//   one host.
// TODO: Phase 4 — perf contract. ADR 0020 §"Layer 6" mandates sub-100ms
//   hot-path retrieval with a 60s cache. The naive impl satisfies
//   correctness; the cache + budget enforcement is the next step.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feedback::{Feedback, FeedbackContext, FeedbackIngest, FeedbackKind};
    use crate::observation::ObservationKind;
    use crate::preferences::{OperatorPreferenceModel, PreferenceKey, PreferenceValue};
    use crate::scoring::ScoreLedger;
    use evy_core::{MandateId, ProviderKind, WorkerId, WorkerStatus};
    use tempfile::tempdir;

    async fn fixture() -> (tempfile::TempDir, NaiveRetriever) {
        let dir = tempdir().unwrap();

        // Observation log with one dispatch row.
        let log_path = dir.path().join("memory.db");
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
        assert!(matches!(items.first(), Some(RetrievedItem::Playbook(_))));
    }

    #[tokio::test]
    async fn returns_observations_when_query_mentions_kind() {
        let (_dir, retriever) = fixture().await;
        let items = retriever
            .retrieve("any worker_dispatched lately?", 5)
            .await
            .unwrap();
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
        let (_dir, retriever) = fixture().await;
        let items = retriever.retrieve("anything", 5).await.unwrap();
        let _ = items.len();
    }

    #[tokio::test]
    async fn retrieves_score_when_task_class_mentioned() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.db");
        let log = Arc::new(ObservationLog::open(&path).await.unwrap());
        let scoring = Arc::new(ScoreLedger::open(&path).await.unwrap());
        scoring
            .record(
                ProviderKind::Codex,
                "rust-refactor",
                WorkerStatus::Succeeded,
                1000,
            )
            .await
            .unwrap();
        let pb_dir = dir.path().join("pb");
        std::fs::create_dir(&pb_dir).unwrap();
        let playbooks = Arc::new(PlaybookStore::load(&pb_dir).unwrap());
        let retriever = NaiveRetriever::new(log, None, playbooks)
            .with_scoring(scoring)
            .with_known_task_classes(vec!["rust-refactor".into()]);
        let items = retriever
            .retrieve("about to dispatch a rust-refactor", 5)
            .await
            .unwrap();
        assert!(items.iter().any(|it| matches!(it, RetrievedItem::Score(_))));
    }

    #[tokio::test]
    async fn retrieves_preference_when_key_token_in_query() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.db");
        let log = Arc::new(ObservationLog::open(&path).await.unwrap());
        let prefs = Arc::new(OperatorPreferenceModel::open(&path).await.unwrap());
        prefs
            .set(
                PreferenceKey::from("idle_banner"),
                PreferenceValue::Boolean(true),
            )
            .await
            .unwrap();
        let pb_dir = dir.path().join("pb");
        std::fs::create_dir(&pb_dir).unwrap();
        let playbooks = Arc::new(PlaybookStore::load(&pb_dir).unwrap());
        let retriever = NaiveRetriever::new(log, None, playbooks).with_preferences(prefs);
        let items = retriever
            .retrieve("idle_banner should it be loud?", 5)
            .await
            .unwrap();
        assert!(items
            .iter()
            .any(|it| matches!(it, RetrievedItem::Preference(_, _))));
    }

    #[tokio::test]
    async fn retrieves_recent_feedback() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.db");
        let log = Arc::new(ObservationLog::open(&path).await.unwrap());
        let scoring = Arc::new(ScoreLedger::open(&path).await.unwrap());
        let prefs = Arc::new(OperatorPreferenceModel::open(&path).await.unwrap());
        let ingest = Arc::new(
            FeedbackIngest::open(&path, log.clone(), scoring, prefs)
                .await
                .unwrap(),
        );
        ingest
            .ingest(Feedback::new(
                FeedbackKind::Approved,
                FeedbackContext {
                    action_proposed: "ship".into(),
                    ..FeedbackContext::default()
                },
            ))
            .await
            .unwrap();
        let pb_dir = dir.path().join("pb");
        std::fs::create_dir(&pb_dir).unwrap();
        let playbooks = Arc::new(PlaybookStore::load(&pb_dir).unwrap());
        let retriever = NaiveRetriever::new(log, None, playbooks).with_feedback(ingest);
        let items = retriever.retrieve("anything", 5).await.unwrap();
        assert!(items
            .iter()
            .any(|it| matches!(it, RetrievedItem::Feedback(_))));
    }
}
