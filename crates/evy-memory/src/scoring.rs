//! Worker-effectiveness scoring — layer 3 of the learning loop.
//!
//! For each `(provider, task_class)` pair, maintain rolling success /
//! failure counters plus a running mean of completion duration. Dispatch
//! callers consult [`ScoreLedger::recommend`] when the operator's stated
//! requirement allows a choice of provider; the rest of the time they
//! still call [`ScoreLedger::record`] on every worker terminal state so
//! the table stays current.
//!
//! ADR 0020 §"Layer 3 — Worker-effectiveness scoring".
//!
//! ### Storage
//!
//! The table (`worker_effectiveness_scores`) has one row per
//! `(provider, task_class)` pair, with success/failure counters and a
//! `REAL` running average. Updates use sqlite's `ON CONFLICT DO UPDATE`
//! upsert; the running mean is recomputed inside the SQL so each record
//! is a single round-trip.

use std::path::Path;

use chrono::{DateTime, Utc};
use evy_core::{ProviderKind, Result, WorkerStatus};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::error;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// One row of the scoring table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkerEffectivenessScore {
    /// Which provider this row scores.
    pub provider: ProviderKind,
    /// Coarse task class label, e.g. `code_change`, `code_review`,
    /// `investigation`. ADR 0020 §"Layer 3" pins the v4.0 taxonomy.
    pub task_class: String,
    /// Number of successful terminal outcomes.
    pub successes: u64,
    /// Number of failed terminal outcomes (`Failed(_)` or `Cancelled`).
    pub failures: u64,
    /// Running mean of completion durations, in milliseconds.
    pub avg_duration_ms: f64,
    /// Wall-clock timestamp of the most recent recorded outcome.
    pub last_seen: DateTime<Utc>,
}

impl WorkerEffectivenessScore {
    /// Cheap success ratio in `[0.0, 1.0]`, or `None` if the row has no
    /// recorded outcomes (defensive — the storage layer never inserts
    /// rows with both counters at zero).
    #[must_use]
    pub fn success_rate(&self) -> Option<f64> {
        let total = self.successes + self.failures;
        if total == 0 {
            None
        } else {
            #[allow(clippy::cast_precision_loss)]
            Some(self.successes as f64 / total as f64)
        }
    }

    /// Total sample count (`successes + failures`).
    #[must_use]
    pub fn sample_count(&self) -> u64 {
        self.successes + self.failures
    }
}

/// Sqlx-backed scoring table handle. Cheap to clone — internally an
/// `Arc`-shared pool.
#[derive(Debug, Clone)]
pub struct ScoreLedger {
    pool: SqlitePool,
}

impl ScoreLedger {
    /// Open (or create) the ledger at `db_path` and run all evy-memory
    /// migrations. Safe to call against a db that another evy-memory
    /// store (observations, preferences, feedback) has already opened —
    /// migrations are idempotent.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] if the database cannot be opened
    /// or if migrations fail.
    pub async fn open(db_path: &Path) -> Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await
            .map_err(error::from_sqlx)?;
        MIGRATOR.run(&pool).await.map_err(error::from_migrate)?;
        Ok(Self { pool })
    }

    /// Record one terminal outcome for `(provider, task_class)`.
    ///
    /// `outcome` is mapped to a success/failure bucket:
    /// `WorkerStatus::Succeeded` increments `successes`; everything else
    /// (`Failed`, `Cancelled`, `Pending`, `Running` — the last two should
    /// not be reached at a terminal state but are mapped to `failures`
    /// defensively) increments `failures`.
    ///
    /// `duration_ms` is folded into the running mean via an upsert
    /// expression evaluated inside sqlite — see the SQL for the
    /// recurrence form.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] on insert/update failure.
    pub async fn record(
        &self,
        provider: ProviderKind,
        task_class: &str,
        outcome: WorkerStatus,
        duration_ms: u64,
    ) -> Result<()> {
        let (succ_inc, fail_inc) = match outcome {
            WorkerStatus::Succeeded => (1_i64, 0_i64),
            // Failed / Cancelled / Pending / Running all count as
            // non-success; only Succeeded raises the success counter.
            _ => (0_i64, 1_i64),
        };
        #[allow(clippy::cast_precision_loss)]
        let duration_f64 = duration_ms as f64;
        let ts = Utc::now().to_rfc3339();
        // Single-statement upsert. On insert, the new average is just
        // the incoming duration (denominator = 1). On conflict, the
        // recurrence is:
        //
        //   new_avg = (old_avg * old_total + new_dur * new_total) /
        //            (old_total + new_total)
        //
        // where bare column refs in the SET clause resolve to the
        // pre-update row values (sqlite semantics), and `excluded.*`
        // refers to the would-be-inserted row.
        sqlx::query(
            "INSERT INTO worker_effectiveness_scores \
                (provider, task_class, successes, failures, avg_duration_ms, last_seen) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(provider, task_class) DO UPDATE SET \
                avg_duration_ms = \
                    ((avg_duration_ms * (successes + failures)) \
                     + (excluded.avg_duration_ms * (excluded.successes + excluded.failures))) \
                    / (successes + failures + excluded.successes + excluded.failures), \
                successes = successes + excluded.successes, \
                failures = failures + excluded.failures, \
                last_seen = excluded.last_seen",
        )
        .bind(provider_to_str(provider))
        .bind(task_class)
        .bind(succ_inc)
        .bind(fail_inc)
        .bind(duration_f64)
        .bind(ts)
        .execute(&self.pool)
        .await
        .map_err(error::from_sqlx)?;
        Ok(())
    }

    /// Fetch the score row for `(provider, task_class)`, or `None` if
    /// nothing has been recorded yet.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] on query failure or
    /// [`evy_core::Error::InvalidMandate`] if the row carries data we
    /// cannot decode (bad provider string, malformed timestamp).
    pub async fn score(
        &self,
        provider: ProviderKind,
        task_class: &str,
    ) -> Result<Option<WorkerEffectivenessScore>> {
        let row = sqlx::query(
            "SELECT provider, task_class, successes, failures, avg_duration_ms, last_seen \
             FROM worker_effectiveness_scores \
             WHERE provider = ?1 AND task_class = ?2",
        )
        .bind(provider_to_str(provider))
        .bind(task_class)
        .fetch_optional(&self.pool)
        .await
        .map_err(error::from_sqlx)?;
        row.map(decode_score).transpose()
    }

    /// Every row for the given `task_class`, regardless of provider.
    /// Used by retrieval to surface relevant priors at decision time.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] on query failure or
    /// [`evy_core::Error::InvalidMandate`] if a row cannot be decoded.
    pub async fn scores_for_task(&self, task_class: &str) -> Result<Vec<WorkerEffectivenessScore>> {
        let rows = sqlx::query(
            "SELECT provider, task_class, successes, failures, avg_duration_ms, last_seen \
             FROM worker_effectiveness_scores \
             WHERE task_class = ?1 \
             ORDER BY (CAST(successes AS REAL) / NULLIF(successes + failures, 0)) DESC, \
                      (successes + failures) DESC",
        )
        .bind(task_class)
        .fetch_all(&self.pool)
        .await
        .map_err(error::from_sqlx)?;
        rows.into_iter().map(decode_score).collect()
    }

    /// Recommend the provider with the highest success rate for the
    /// given task class. Ties broken by sample count (more samples
    /// wins), then by latency (lower wins). Returns `None` if no rows
    /// exist for the task class.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] on query failure or
    /// [`evy_core::Error::InvalidMandate`] if a row cannot be decoded.
    pub async fn recommend(&self, task_class: &str) -> Result<Option<ProviderKind>> {
        let scores = self.scores_for_task(task_class).await?;
        Ok(scores
            .into_iter()
            .max_by(|a, b| {
                let sr_a = a.success_rate().unwrap_or(0.0);
                let sr_b = b.success_rate().unwrap_or(0.0);
                sr_a.partial_cmp(&sr_b)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.sample_count().cmp(&b.sample_count()))
                    .then_with(|| {
                        // Lower latency is better → flip ordering so the
                        // lower-latency row is `Greater` under max_by.
                        b.avg_duration_ms
                            .partial_cmp(&a.avg_duration_ms)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
            })
            .map(|s| s.provider))
    }
}

fn decode_score(row: sqlx::sqlite::SqliteRow) -> Result<WorkerEffectivenessScore> {
    let provider_s: String = row.try_get("provider").map_err(error::from_sqlx)?;
    let task_class: String = row.try_get("task_class").map_err(error::from_sqlx)?;
    let successes_i: i64 = row.try_get("successes").map_err(error::from_sqlx)?;
    let failures_i: i64 = row.try_get("failures").map_err(error::from_sqlx)?;
    let avg_duration_ms: f64 = row.try_get("avg_duration_ms").map_err(error::from_sqlx)?;
    let last_seen_s: String = row.try_get("last_seen").map_err(error::from_sqlx)?;
    let last_seen = DateTime::parse_from_rfc3339(&last_seen_s)
        .map_err(|e| error::bad_row(format!("score last_seen `{last_seen_s}`: {e}")))?
        .with_timezone(&Utc);
    Ok(WorkerEffectivenessScore {
        provider: provider_from_str(&provider_s)?,
        task_class,
        successes: u64::try_from(successes_i).unwrap_or(0),
        failures: u64::try_from(failures_i).unwrap_or(0),
        avg_duration_ms,
        last_seen,
    })
}

/// Stable textual encoding of `ProviderKind` for the `provider` column.
/// Matches the serde-derived variant names so on-disk strings agree with
/// JSON-serialised values elsewhere.
fn provider_to_str(p: ProviderKind) -> &'static str {
    match p {
        ProviderKind::ClaudeCode => "ClaudeCode",
        ProviderKind::Codex => "Codex",
        ProviderKind::DeepSeek => "DeepSeek",
    }
}

fn provider_from_str(s: &str) -> Result<ProviderKind> {
    match s {
        "ClaudeCode" => Ok(ProviderKind::ClaudeCode),
        "Codex" => Ok(ProviderKind::Codex),
        "DeepSeek" => Ok(ProviderKind::DeepSeek),
        other => Err(error::bad_row(format!("unknown provider `{other}`"))),
    }
}

// TODO: Phase 4 — Bayesian / Thompson-sampling recommend(). Today's
//   pick-highest-success-rate logic is fine while sample counts are
//   small; once we have hundreds of recorded outcomes per task class,
//   shrinkage toward a prior will give better cold-start behaviour and
//   exploit/explore balance.
// TODO: Phase 4 — minimum-sample threshold before `recommend` is
//   willing to express a preference. The v4.0 implementation will
//   happily recommend after a single observation, which is too eager.
// TODO: Phase 4 — P50/P95 latency, not just mean. The mean hides tail
//   behaviour that matters for "is this provider rate-limited?"
//   detection.
// TODO: Phase 4 — rolling window / decay. Today every outcome counts
//   forever; we want recent outcomes to weigh more so providers can
//   improve out of a slump.

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn fresh_ledger() -> (tempfile::TempDir, ScoreLedger) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.db");
        let ledger = ScoreLedger::open(&path).await.expect("open");
        (dir, ledger)
    }

    #[tokio::test]
    async fn empty_ledger_returns_none() {
        let (_dir, ledger) = fresh_ledger().await;
        assert!(ledger
            .score(ProviderKind::ClaudeCode, "code_change")
            .await
            .unwrap()
            .is_none());
        assert!(ledger.recommend("code_change").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn record_creates_then_updates_row() {
        let (_dir, ledger) = fresh_ledger().await;
        ledger
            .record(
                ProviderKind::ClaudeCode,
                "code_change",
                WorkerStatus::Succeeded,
                1000,
            )
            .await
            .unwrap();
        ledger
            .record(
                ProviderKind::ClaudeCode,
                "code_change",
                WorkerStatus::Succeeded,
                3000,
            )
            .await
            .unwrap();
        let s = ledger
            .score(ProviderKind::ClaudeCode, "code_change")
            .await
            .unwrap()
            .expect("row exists");
        assert_eq!(s.successes, 2);
        assert_eq!(s.failures, 0);
        // Running mean of 1000 and 3000 is 2000.
        assert!((s.avg_duration_ms - 2000.0).abs() < 1e-6);
        assert_eq!(s.success_rate(), Some(1.0));
    }

    #[tokio::test]
    async fn failures_increment_separately() {
        let (_dir, ledger) = fresh_ledger().await;
        ledger
            .record(
                ProviderKind::Codex,
                "investigation",
                WorkerStatus::Failed("boom".into()),
                500,
            )
            .await
            .unwrap();
        ledger
            .record(
                ProviderKind::Codex,
                "investigation",
                WorkerStatus::Cancelled,
                500,
            )
            .await
            .unwrap();
        ledger
            .record(
                ProviderKind::Codex,
                "investigation",
                WorkerStatus::Succeeded,
                500,
            )
            .await
            .unwrap();
        let s = ledger
            .score(ProviderKind::Codex, "investigation")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(s.successes, 1);
        assert_eq!(s.failures, 2);
        assert_eq!(s.sample_count(), 3);
        assert!((s.success_rate().unwrap() - (1.0 / 3.0)).abs() < 1e-9);
    }

    #[tokio::test]
    async fn recommend_picks_higher_success_rate() {
        let (_dir, ledger) = fresh_ledger().await;
        // ClaudeCode: 5 failures.
        for _ in 0..5 {
            ledger
                .record(
                    ProviderKind::ClaudeCode,
                    "rust-refactor",
                    WorkerStatus::Failed("nope".into()),
                    2000,
                )
                .await
                .unwrap();
        }
        // Codex: 4 successes, 1 failure.
        for _ in 0..4 {
            ledger
                .record(
                    ProviderKind::Codex,
                    "rust-refactor",
                    WorkerStatus::Succeeded,
                    1500,
                )
                .await
                .unwrap();
        }
        ledger
            .record(
                ProviderKind::Codex,
                "rust-refactor",
                WorkerStatus::Failed("flake".into()),
                1500,
            )
            .await
            .unwrap();
        let rec = ledger.recommend("rust-refactor").await.unwrap();
        assert_eq!(rec, Some(ProviderKind::Codex));
    }

    #[tokio::test]
    async fn recommend_returns_only_provider_when_one_recorded() {
        let (_dir, ledger) = fresh_ledger().await;
        ledger
            .record(
                ProviderKind::DeepSeek,
                "documentation",
                WorkerStatus::Succeeded,
                700,
            )
            .await
            .unwrap();
        assert_eq!(
            ledger.recommend("documentation").await.unwrap(),
            Some(ProviderKind::DeepSeek)
        );
    }

    #[tokio::test]
    async fn unknown_task_class_yields_none() {
        let (_dir, ledger) = fresh_ledger().await;
        ledger
            .record(
                ProviderKind::ClaudeCode,
                "code_change",
                WorkerStatus::Succeeded,
                100,
            )
            .await
            .unwrap();
        assert!(ledger.recommend("never-recorded").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn scores_for_task_lists_all_providers() {
        let (_dir, ledger) = fresh_ledger().await;
        ledger
            .record(
                ProviderKind::ClaudeCode,
                "ts-fix",
                WorkerStatus::Succeeded,
                900,
            )
            .await
            .unwrap();
        ledger
            .record(ProviderKind::Codex, "ts-fix", WorkerStatus::Succeeded, 500)
            .await
            .unwrap();
        let list = ledger.scores_for_task("ts-fix").await.unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn provider_string_roundtrips() {
        for p in [
            ProviderKind::ClaudeCode,
            ProviderKind::Codex,
            ProviderKind::DeepSeek,
        ] {
            let s = provider_to_str(p);
            assert_eq!(provider_from_str(s).unwrap(), p);
        }
        assert!(provider_from_str("bogus").is_err());
    }
}
