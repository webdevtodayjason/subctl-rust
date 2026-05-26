//! Feedback ingest — layer 7 of the learning loop.
//!
//! ADR 0020 §"Layer 7 — Feedback ingest". Operator corrections are
//! captured as [`Feedback`] rows in a dedicated table; ingest mirrors
//! each event into the observation log (so time-ordered retrieval sees
//! it), updates the scoring ledger when the feedback names a worker,
//! and updates the preference model when the feedback is an explicit
//! preference statement.
//!
//! ### Scoring update convention
//!
//! `FeedbackContext` does not carry the `(provider, task_class,
//! duration_ms)` triple that [`super::scoring::ScoreLedger::record`]
//! needs. Callers attach those through `metadata`:
//!
//! - `metadata["provider"]` — variant name of `ProviderKind`
//!   (`"ClaudeCode"`, `"Codex"`, `"DeepSeek"`).
//! - `metadata["task_class"]` — coarse task label.
//! - `metadata["duration_ms"]` — base-10 integer string.
//!
//! When any of those is missing or unparseable, the scoring update is
//! skipped (with a `tracing::warn`) and the feedback is otherwise
//! processed normally. Callers that don't have a worker in mind leave
//! `related_worker = None`; the scoring update is then also skipped,
//! without a warning, since the absence is intentional.
//!
//! ### OperatorPreference update convention
//!
//! When `FeedbackKind::OperatorPreference { key, value }` is ingested,
//! the value is stored as a [`super::preferences::PreferenceValue::Text`]
//! against the key. Richer typed values (boolean / number / list) can be
//! set via [`super::preferences::OperatorPreferenceModel::set`] directly
//! by callers who already have a typed value in hand.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use evy_core::{MandateId, ProviderKind, Result, WorkerId, WorkerStatus};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::error;
use crate::observation::{Observation, ObservationKind};
use crate::observation_log::ObservationLog;
use crate::preferences::{OperatorPreferenceModel, PreferenceKey, PreferenceValue};
use crate::scoring::ScoreLedger;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// One operator-feedback event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Feedback {
    /// Stable id; minted v4 UUID by default.
    pub id: Uuid,
    /// Wall-clock timestamp at which the feedback arrived.
    pub ts: DateTime<Utc>,
    /// The shape of the feedback signal.
    pub kind: FeedbackKind,
    /// What Evy was doing / proposing when the feedback arrived.
    pub context: FeedbackContext,
}

impl Feedback {
    /// Build a fresh feedback envelope with a minted id and `Utc::now()`
    /// timestamp.
    #[must_use]
    pub fn new(kind: FeedbackKind, context: FeedbackContext) -> Self {
        Self {
            id: Uuid::new_v4(),
            ts: Utc::now(),
            kind,
            context,
        }
    }
}

/// Tagged enum of operator-feedback signals.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FeedbackKind {
    /// Operator confirmed Evy's proposed action.
    Approved,
    /// Operator rejected the proposal.
    Rejected {
        /// Free-form rationale captured from the operator's message.
        reason: String,
    },
    /// Operator supplied an alternative action.
    Corrected {
        /// Replacement action the operator would prefer.
        new_action: String,
        /// Optional rationale for the correction.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Operator stated a preference (not tied to one action).
    OperatorPreference {
        /// Preference key, e.g. `"prefer_codex_for_rust"`.
        key: String,
        /// Stringified value. Typed shapes go through the preference
        /// model directly; this variant stores everything as text.
        value: String,
    },
}

impl FeedbackKind {
    /// Discriminator string matching the serde tag.
    #[must_use]
    pub fn discriminator(&self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Rejected { .. } => "rejected",
            Self::Corrected { .. } => "corrected",
            Self::OperatorPreference { .. } => "operator_preference",
        }
    }
}

/// What Evy was doing or proposing when the feedback arrived.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct FeedbackContext {
    /// Worker the feedback targets, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related_worker: Option<WorkerId>,
    /// Mandate the feedback targets, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related_mandate: Option<MandateId>,
    /// Name of the playbook the feedback targets, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related_playbook: Option<String>,
    /// The action Evy proposed when feedback arrived. Empty string when
    /// the feedback is unsolicited (e.g., a standalone preference).
    pub action_proposed: String,
    /// Free-form string-keyed metadata. See module docs for the
    /// well-known keys (`provider`, `task_class`, `duration_ms`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

/// Ingest pipeline. Holds Arc-shared handles to the three downstream
/// stores; owns its own pool for the `feedback` table.
#[derive(Debug, Clone)]
pub struct FeedbackIngest {
    log: Arc<ObservationLog>,
    scoring: Arc<ScoreLedger>,
    preferences: Arc<OperatorPreferenceModel>,
    pool: SqlitePool,
}

impl FeedbackIngest {
    /// Open the feedback store at `db_path` and wire it to the supplied
    /// downstream handles. Runs all evy-memory migrations; safe to call
    /// against a db another store has already opened.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] if the database cannot be opened
    /// or if migrations fail.
    pub async fn open(
        db_path: &Path,
        log: Arc<ObservationLog>,
        scoring: Arc<ScoreLedger>,
        preferences: Arc<OperatorPreferenceModel>,
    ) -> Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await
            .map_err(error::from_sqlx)?;
        MIGRATOR.run(&pool).await.map_err(error::from_migrate)?;
        Ok(Self {
            log,
            scoring,
            preferences,
            pool,
        })
    }

    /// Convenience constructor for callers wiring everything against
    /// one shared pool (e.g. tests, single-db deployments). Equivalent
    /// to `open(db_path, …)`; the migrator is idempotent.
    ///
    /// # Errors
    /// As [`FeedbackIngest::open`].
    pub async fn new(
        db_path: &Path,
        log: Arc<ObservationLog>,
        scoring: Arc<ScoreLedger>,
        preferences: Arc<OperatorPreferenceModel>,
    ) -> Result<Self> {
        Self::open(db_path, log, scoring, preferences).await
    }

    /// Borrow the underlying observation log handle. Useful for callers
    /// that hold an `Arc<FeedbackIngest>` and need to append unrelated
    /// observations without re-passing the log around.
    #[must_use]
    pub fn observation_log(&self) -> &Arc<ObservationLog> {
        &self.log
    }

    /// Ingest one feedback event. Steps:
    ///
    /// 1. Persist the feedback row to the `feedback` table.
    /// 2. Mirror an `ObservationKind::FeedbackReceived` observation to
    ///    the log (correlation id = feedback id).
    /// 3. If the feedback names a worker and its metadata carries the
    ///    well-known `provider` + `task_class` keys, record a score in
    ///    the scoring ledger (Approved → success; Rejected/Corrected →
    ///    failure; OperatorPreference doesn't update scoring).
    /// 4. If the feedback is `OperatorPreference`, upsert the preference
    ///    into the preference model as a `PreferenceValue::Text`.
    ///
    /// Any step beyond (1) and (2) that cannot be performed is logged
    /// and skipped — the feedback row is canonical regardless.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] on insert failure,
    /// [`evy_core::Error::Serde`] on JSON encode failure for the
    /// feedback payload.
    pub async fn ingest(&self, feedback: Feedback) -> Result<()> {
        // 1. Persist to the feedback table.
        let kind_json = serde_json::to_string(&feedback.kind)?;
        let context_json = serde_json::to_string(&feedback.context)?;
        sqlx::query(
            "INSERT INTO feedback (id, ts, kind, context) \
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(feedback.id.to_string())
        .bind(feedback.ts.to_rfc3339())
        .bind(&kind_json)
        .bind(&context_json)
        .execute(&self.pool)
        .await
        .map_err(error::from_sqlx)?;

        // 2. Mirror to the observation log so time-ordered retrieval
        //    sees the event without a JOIN.
        let mirror = Observation::new(ObservationKind::FeedbackReceived {
            feedback_id: feedback.id,
            feedback_kind: feedback.kind.discriminator().to_owned(),
        })
        .with_correlation(feedback.id);
        self.log.append(mirror).await?;

        // 3. Update scoring when caller attached the necessary metadata.
        if feedback.context.related_worker.is_some() {
            self.maybe_update_scoring(&feedback).await;
        }

        // 4. Update preferences for explicit preference signals.
        if let FeedbackKind::OperatorPreference { key, value } = &feedback.kind {
            self.preferences
                .set(
                    PreferenceKey::from(key.as_str()),
                    PreferenceValue::Text(value.clone()),
                )
                .await?;
        }

        Ok(())
    }

    /// Look up `feedback_id` and rehydrate the row, if it exists.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] on query failure,
    /// [`evy_core::Error::Serde`] if a row's payload is malformed.
    pub async fn get(&self, feedback_id: Uuid) -> Result<Option<Feedback>> {
        let row = sqlx::query("SELECT id, ts, kind, context FROM feedback WHERE id = ?1")
            .bind(feedback_id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(error::from_sqlx)?;
        row.map(decode_feedback).transpose()
    }

    /// Most-recent `limit` feedback rows, newest first.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] on query failure,
    /// [`evy_core::Error::Serde`] if a row's payload is malformed.
    pub async fn recent(&self, limit: usize) -> Result<Vec<Feedback>> {
        let rows = sqlx::query(
            "SELECT id, ts, kind, context FROM feedback \
             ORDER BY ts DESC, id DESC LIMIT ?1",
        )
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(error::from_sqlx)?;
        rows.into_iter().map(decode_feedback).collect()
    }

    /// Feedback rows whose serialised kind matches `kind_prefix`. Pass
    /// the bare discriminator (`"approved"`, `"rejected"`, …) to filter
    /// by variant.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] on query failure,
    /// [`evy_core::Error::Serde`] if a row's payload is malformed.
    pub async fn by_kind(&self, kind_prefix: &str, limit: usize) -> Result<Vec<Feedback>> {
        // The `kind` column stores serialised JSON of the FeedbackKind
        // variant; anchor the LIKE pattern on the `"kind":"…"` tag so
        // a `Rejected { reason: "this was approved earlier" }` row
        // can't false-match `by_kind("approved", _)`.
        let pattern = format!("%\"kind\":\"{kind_prefix}\"%");
        let rows = sqlx::query(
            "SELECT id, ts, kind, context FROM feedback \
             WHERE kind LIKE ?1 \
             ORDER BY ts DESC, id DESC LIMIT ?2",
        )
        .bind(pattern)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(error::from_sqlx)?;
        rows.into_iter().map(decode_feedback).collect()
    }

    async fn maybe_update_scoring(&self, feedback: &Feedback) {
        // OperatorPreference feedback never updates scoring.
        let outcome = match &feedback.kind {
            FeedbackKind::Approved => WorkerStatus::Succeeded,
            FeedbackKind::Rejected { reason } => WorkerStatus::Failed(reason.clone()),
            FeedbackKind::Corrected { reason, .. } => {
                WorkerStatus::Failed(reason.clone().unwrap_or_else(|| "corrected".into()))
            }
            FeedbackKind::OperatorPreference { .. } => return,
        };
        let meta = &feedback.context.metadata;
        let Some(provider_s) = meta.get("provider") else {
            tracing::warn!(
                feedback_id = %feedback.id,
                "feedback ingest: skipping scoring update — `provider` missing from metadata"
            );
            return;
        };
        let Some(task_class) = meta.get("task_class") else {
            tracing::warn!(
                feedback_id = %feedback.id,
                "feedback ingest: skipping scoring update — `task_class` missing from metadata"
            );
            return;
        };
        let provider = match parse_provider(provider_s) {
            Some(p) => p,
            None => {
                tracing::warn!(
                    feedback_id = %feedback.id,
                    provider = %provider_s,
                    "feedback ingest: skipping scoring update — unknown provider"
                );
                return;
            }
        };
        let duration_ms: u64 = meta
            .get("duration_ms")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if let Err(e) = self
            .scoring
            .record(provider, task_class, outcome, duration_ms)
            .await
        {
            tracing::warn!(
                feedback_id = %feedback.id,
                error = %e,
                "feedback ingest: scoring update failed"
            );
        }
    }
}

fn decode_feedback(row: sqlx::sqlite::SqliteRow) -> Result<Feedback> {
    let id_s: String = row.try_get("id").map_err(error::from_sqlx)?;
    let ts_s: String = row.try_get("ts").map_err(error::from_sqlx)?;
    let kind_s: String = row.try_get("kind").map_err(error::from_sqlx)?;
    let context_s: String = row.try_get("context").map_err(error::from_sqlx)?;
    let id =
        Uuid::parse_str(&id_s).map_err(|e| error::bad_row(format!("feedback id `{id_s}`: {e}")))?;
    let ts = DateTime::parse_from_rfc3339(&ts_s)
        .map_err(|e| error::bad_row(format!("feedback ts `{ts_s}`: {e}")))?
        .with_timezone(&Utc);
    let kind: FeedbackKind = serde_json::from_str(&kind_s)?;
    let context: FeedbackContext = serde_json::from_str(&context_s)?;
    Ok(Feedback {
        id,
        ts,
        kind,
        context,
    })
}

fn parse_provider(s: &str) -> Option<ProviderKind> {
    match s {
        "ClaudeCode" => Some(ProviderKind::ClaudeCode),
        "Codex" => Some(ProviderKind::Codex),
        "DeepSeek" => Some(ProviderKind::DeepSeek),
        _ => None,
    }
}

// TODO: Phase 4 — correction-classifier. Today every operator message
//   the orchestrator hands us is presumed to be feedback; ADR 0020
//   §"Layer 7" envisions a small locally-runnable classifier that
//   filters status questions / clarifications out before they reach
//   ingest.
// TODO: Phase 4 — implicit preference inference from Corrected events.
//   Repeated "do it with Codex instead of ClaudeCode" corrections
//   should auto-update `prefer_codex_for_rust`-style preferences
//   without an explicit OperatorPreference event.
// TODO: Phase 4 — provider/task_class derivation from related_worker.
//   The current convention (caller attaches metadata) keeps ingest
//   decoupled but pushes responsibility onto the orchestrator. A
//   lookup over the WorkerDispatched observation would let callers
//   pass `related_worker` alone.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::ObservationKind;
    use tempfile::tempdir;

    async fn fresh_stack() -> (
        tempfile::TempDir,
        Arc<ObservationLog>,
        Arc<ScoreLedger>,
        Arc<OperatorPreferenceModel>,
        FeedbackIngest,
    ) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.db");
        let log = Arc::new(ObservationLog::open(&path).await.unwrap());
        let scoring = Arc::new(ScoreLedger::open(&path).await.unwrap());
        let prefs = Arc::new(OperatorPreferenceModel::open(&path).await.unwrap());
        let ingest = FeedbackIngest::open(&path, log.clone(), scoring.clone(), prefs.clone())
            .await
            .unwrap();
        (dir, log, scoring, prefs, ingest)
    }

    fn approved_for_worker(worker: WorkerId) -> Feedback {
        let mut meta = HashMap::new();
        meta.insert("provider".into(), "Codex".into());
        meta.insert("task_class".into(), "rust-refactor".into());
        meta.insert("duration_ms".into(), "1500".into());
        Feedback::new(
            FeedbackKind::Approved,
            FeedbackContext {
                related_worker: Some(worker),
                action_proposed: "ship slice".into(),
                metadata: meta,
                ..FeedbackContext::default()
            },
        )
    }

    #[tokio::test]
    async fn approved_feedback_updates_scoring_successes() {
        let (_dir, log, scoring, _prefs, ingest) = fresh_stack().await;
        let worker = WorkerId::new();
        ingest.ingest(approved_for_worker(worker)).await.unwrap();

        let score = scoring
            .score(ProviderKind::Codex, "rust-refactor")
            .await
            .unwrap()
            .expect("score row");
        assert_eq!(score.successes, 1);
        assert_eq!(score.failures, 0);

        // Mirror observation must exist.
        let obs = log.query_by_kind("feedback_received", 10).await.unwrap();
        assert_eq!(obs.len(), 1);
        assert!(matches!(
            obs[0].kind,
            ObservationKind::FeedbackReceived { .. }
        ));
    }

    #[tokio::test]
    async fn rejected_feedback_updates_scoring_failures() {
        let (_dir, _log, scoring, _prefs, ingest) = fresh_stack().await;
        let mut meta = HashMap::new();
        meta.insert("provider".into(), "ClaudeCode".into());
        meta.insert("task_class".into(), "rust-refactor".into());
        meta.insert("duration_ms".into(), "2000".into());
        let fb = Feedback::new(
            FeedbackKind::Rejected {
                reason: "wrong approach".into(),
            },
            FeedbackContext {
                related_worker: Some(WorkerId::new()),
                action_proposed: "merge as-is".into(),
                metadata: meta,
                ..FeedbackContext::default()
            },
        );
        ingest.ingest(fb).await.unwrap();
        let score = scoring
            .score(ProviderKind::ClaudeCode, "rust-refactor")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(score.successes, 0);
        assert_eq!(score.failures, 1);
    }

    #[tokio::test]
    async fn operator_preference_feedback_updates_preferences() {
        let (_dir, _log, _scoring, prefs, ingest) = fresh_stack().await;
        let fb = Feedback::new(
            FeedbackKind::OperatorPreference {
                key: "prefer_codex_for_rust".into(),
                value: "true".into(),
            },
            FeedbackContext {
                action_proposed: String::new(),
                ..FeedbackContext::default()
            },
        );
        ingest.ingest(fb).await.unwrap();
        let got = prefs
            .get(&PreferenceKey::from("prefer_codex_for_rust"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got, PreferenceValue::Text("true".into()));
    }

    #[tokio::test]
    async fn ingest_without_worker_or_metadata_still_persists() {
        let (_dir, log, scoring, _prefs, ingest) = fresh_stack().await;
        let fb = Feedback::new(
            FeedbackKind::Approved,
            FeedbackContext {
                action_proposed: "no related worker".into(),
                ..FeedbackContext::default()
            },
        );
        ingest.ingest(fb.clone()).await.unwrap();
        // Feedback row exists.
        let got = ingest.get(fb.id).await.unwrap().unwrap();
        assert_eq!(got, fb);
        // Mirror observation exists.
        let obs = log.query_by_kind("feedback_received", 10).await.unwrap();
        assert_eq!(obs.len(), 1);
        // No score recorded because metadata didn't supply provider/task_class.
        let any_score = scoring
            .score(ProviderKind::ClaudeCode, "rust-refactor")
            .await
            .unwrap();
        assert!(any_score.is_none());
    }

    #[tokio::test]
    async fn ingest_with_worker_but_missing_metadata_skips_scoring_loudly() {
        let (_dir, _log, scoring, _prefs, ingest) = fresh_stack().await;
        // related_worker is Some but metadata is empty — should warn-log
        // and skip the score update, not error.
        let fb = Feedback::new(
            FeedbackKind::Approved,
            FeedbackContext {
                related_worker: Some(WorkerId::new()),
                action_proposed: "x".into(),
                ..FeedbackContext::default()
            },
        );
        ingest.ingest(fb).await.unwrap();
        // No rows in the scoring table.
        let any = scoring.scores_for_task("rust-refactor").await.unwrap();
        assert!(any.is_empty());
    }

    #[tokio::test]
    async fn recent_returns_newest_first() {
        let (_dir, _log, _scoring, _prefs, ingest) = fresh_stack().await;
        let worker = WorkerId::new();
        let first = approved_for_worker(worker);
        ingest.ingest(first.clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let second = approved_for_worker(worker);
        ingest.ingest(second.clone()).await.unwrap();
        let listed = ingest.recent(10).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, second.id);
        assert_eq!(listed[1].id, first.id);
    }

    #[tokio::test]
    async fn by_kind_filters_to_discriminator() {
        let (_dir, _log, _scoring, _prefs, ingest) = fresh_stack().await;
        let approved = approved_for_worker(WorkerId::new());
        ingest.ingest(approved).await.unwrap();
        ingest
            .ingest(Feedback::new(
                FeedbackKind::Rejected {
                    reason: "nope".into(),
                },
                FeedbackContext {
                    action_proposed: "x".into(),
                    ..FeedbackContext::default()
                },
            ))
            .await
            .unwrap();
        let approved_only = ingest.by_kind("approved", 10).await.unwrap();
        assert_eq!(approved_only.len(), 1);
        let rejected_only = ingest.by_kind("rejected", 10).await.unwrap();
        assert_eq!(rejected_only.len(), 1);
    }

    #[tokio::test]
    async fn by_kind_does_not_false_match_on_reason_text() {
        // A Rejected feedback whose `reason` contains the literal word
        // `approved` must not appear under by_kind("approved", _). The
        // anchored LIKE pattern (`"kind":"…"`) guarantees this.
        let (_dir, _log, _scoring, _prefs, ingest) = fresh_stack().await;
        ingest
            .ingest(Feedback::new(
                FeedbackKind::Rejected {
                    reason: "this was approved earlier and shouldn't be now".into(),
                },
                FeedbackContext {
                    action_proposed: "merge".into(),
                    ..FeedbackContext::default()
                },
            ))
            .await
            .unwrap();
        let approved_only = ingest.by_kind("approved", 10).await.unwrap();
        assert!(
            approved_only.is_empty(),
            "Rejected feedback whose reason mentions 'approved' must not match by_kind(\"approved\")"
        );
        let rejected_only = ingest.by_kind("rejected", 10).await.unwrap();
        assert_eq!(rejected_only.len(), 1);
    }

    #[test]
    fn feedback_kind_serde_roundtrip() {
        let cases = vec![
            FeedbackKind::Approved,
            FeedbackKind::Rejected {
                reason: "no".into(),
            },
            FeedbackKind::Corrected {
                new_action: "rebase".into(),
                reason: Some("stale".into()),
            },
            FeedbackKind::OperatorPreference {
                key: "k".into(),
                value: "v".into(),
            },
        ];
        for k in cases {
            let s = serde_json::to_string(&k).unwrap();
            let back: FeedbackKind = serde_json::from_str(&s).unwrap();
            assert_eq!(back, k);
        }
    }
}
