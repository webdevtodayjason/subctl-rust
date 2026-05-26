//! Append-only event-sourced observation log.
//!
//! Backed by sqlite via sqlx (runtime-checked queries — no offline
//! metadata file required). Migrations live under
//! `crates/evy-memory/migrations/` and are applied on
//! [`ObservationLog::open`].
//!
//! Each [`Observation`] is serialised whole into the `payload` column;
//! the top-level discriminator is also written to an indexed `kind`
//! column so prefix queries stay cheap. The schema is deliberately wide
//! and weakly typed — strongly typed querying happens above the log.

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use evy_core::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::error;
use crate::observation::{Observation, ObservationKind};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Handle to the observation log database.
///
/// Cheap to clone — internally an `Arc`-shared `SqlitePool`.
#[derive(Debug, Clone)]
pub struct ObservationLog {
    pool: SqlitePool,
}

impl ObservationLog {
    /// Open (or create) the log at `db_path` and run migrations.
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

    /// Append an observation. The full `Observation` is serialised into
    /// `payload`; `kind`/`ts`/`correlation_id`/`metadata` are also
    /// projected to indexed columns for fast retrieval.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Serde`] if `payload` cannot be
    /// serialised, [`evy_core::Error::Io`] if the insert fails.
    pub async fn append(&self, obs: Observation) -> Result<()> {
        let payload = serde_json::to_string(&obs)?;
        let metadata = if obs.metadata.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&obs.metadata)?)
        };
        sqlx::query(
            "INSERT INTO observations (id, ts, kind, payload, correlation_id, metadata) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(obs.id.to_string())
        .bind(obs.ts.to_rfc3339())
        .bind(obs.kind.discriminator())
        .bind(payload)
        .bind(obs.correlation_id.map(|u| u.to_string()))
        .bind(metadata)
        .execute(&self.pool)
        .await
        .map_err(error::from_sqlx)?;
        Ok(())
    }

    /// Most-recent `limit` observations, newest first.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] on query failure or
    /// [`evy_core::Error::Serde`] if a row's payload cannot be parsed.
    pub async fn query_recent(&self, limit: usize) -> Result<Vec<Observation>> {
        let rows = sqlx::query(
            "SELECT payload FROM observations \
             ORDER BY ts DESC, id DESC LIMIT ?1",
        )
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(error::from_sqlx)?;
        decode_rows(rows)
    }

    /// All observations sharing a `correlation_id`, oldest first.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] on query failure or
    /// [`evy_core::Error::Serde`] if a row's payload cannot be parsed.
    pub async fn query_by_correlation(&self, correlation_id: Uuid) -> Result<Vec<Observation>> {
        let rows = sqlx::query(
            "SELECT payload FROM observations \
             WHERE correlation_id = ?1 \
             ORDER BY ts ASC, id ASC",
        )
        .bind(correlation_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(error::from_sqlx)?;
        decode_rows(rows)
    }

    /// Observations whose `kind` discriminator starts with `kind_prefix`.
    ///
    /// `kind_prefix` is matched with sqlite `LIKE` against the indexed
    /// `kind` column; passing the whole discriminator (e.g.
    /// `"worker_dispatched"`) acts as an exact match; passing `"worker_"`
    /// matches every worker_* variant.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] on query failure or
    /// [`evy_core::Error::Serde`] if a row's payload cannot be parsed.
    pub async fn query_by_kind(&self, kind_prefix: &str, limit: usize) -> Result<Vec<Observation>> {
        let pattern = format!("{kind_prefix}%");
        let rows = sqlx::query(
            "SELECT payload FROM observations \
             WHERE kind LIKE ?1 \
             ORDER BY ts DESC, id DESC LIMIT ?2",
        )
        .bind(pattern)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(error::from_sqlx)?;
        decode_rows(rows)
    }

    /// Total row count. Cheap (sqlite full-table scan, ~5MB/year).
    /// Used by tests and (eventually) the dashboard fitness panel.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] on query failure.
    pub async fn count(&self) -> Result<u64> {
        let row = sqlx::query("SELECT COUNT(*) AS c FROM observations")
            .fetch_one(&self.pool)
            .await
            .map_err(error::from_sqlx)?;
        let c: i64 = row.try_get("c").map_err(error::from_sqlx)?;
        Ok(u64::try_from(c).unwrap_or(0))
    }
}

fn decode_rows(rows: Vec<sqlx::sqlite::SqliteRow>) -> Result<Vec<Observation>> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let payload: String = row.try_get("payload").map_err(error::from_sqlx)?;
        let obs: Observation = serde_json::from_str(&payload)?;
        out.push(obs);
    }
    Ok(out)
}

// Suppress dead-code complaints when `chrono::{DateTime, Utc}` /
// `HashMap` / `ObservationKind` are imported only for doc-link resolution
// in the module-level docs. They genuinely are used in tests below.
#[allow(dead_code)]
fn _doc_link_anchors(_ts: DateTime<Utc>, _meta: HashMap<String, String>, _k: ObservationKind) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::ObservationKind;
    use evy_core::{MandateId, ProviderKind, WorkerId, WorkerStatus};
    use tempfile::tempdir;

    async fn fresh_log() -> (tempfile::TempDir, ObservationLog) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("obs.db");
        let log = ObservationLog::open(&path).await.expect("open");
        (dir, log)
    }

    #[tokio::test]
    async fn open_creates_empty_log() {
        let (_dir, log) = fresh_log().await;
        assert_eq!(log.count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn append_then_recent_roundtrip() {
        let (_dir, log) = fresh_log().await;
        let obs = Observation::new(ObservationKind::DaemonBooted {
            version: "0.1.0".into(),
        });
        log.append(obs.clone()).await.unwrap();
        let listed = log.query_recent(10).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], obs);
    }

    #[tokio::test]
    async fn recent_returns_newest_first() {
        let (_dir, log) = fresh_log().await;
        let first = Observation::new(ObservationKind::DaemonBooted {
            version: "1".into(),
        });
        log.append(first.clone()).await.unwrap();
        // Ensure a strictly later RFC3339 timestamp on the second row.
        // chrono::Utc::now() advances at nanosecond resolution, but
        // RFC3339 truncates fractional digits depending on locale; sleep
        // 10ms to be safe across platforms.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let second = Observation::new(ObservationKind::DaemonShutdown {
            reason: "sigterm".into(),
        });
        log.append(second.clone()).await.unwrap();
        let listed = log.query_recent(10).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, second.id, "second insert must come first");
        assert_eq!(listed[1].id, first.id);
    }

    #[tokio::test]
    async fn query_by_correlation_returns_only_matching_rows() {
        let (_dir, log) = fresh_log().await;
        let correlation = Uuid::new_v4();
        let a = Observation::new(ObservationKind::WorkerDispatched {
            worker_id: WorkerId::new(),
            mandate_id: MandateId::new(),
            provider: ProviderKind::ClaudeCode,
        })
        .with_correlation(correlation);
        let b = Observation::new(ObservationKind::WorkerCompleted {
            worker_id: WorkerId::new(),
            status: WorkerStatus::Succeeded,
        })
        .with_correlation(correlation);
        // Unrelated row, no correlation id.
        let c = Observation::new(ObservationKind::DaemonBooted {
            version: "v".into(),
        });
        log.append(a.clone()).await.unwrap();
        log.append(b.clone()).await.unwrap();
        log.append(c).await.unwrap();
        let related = log.query_by_correlation(correlation).await.unwrap();
        assert_eq!(related.len(), 2);
        // Oldest-first ordering for the correlation view.
        assert_eq!(related[0].id, a.id);
        assert_eq!(related[1].id, b.id);
    }

    #[tokio::test]
    async fn query_by_kind_prefix_filters() {
        let (_dir, log) = fresh_log().await;
        log.append(Observation::new(ObservationKind::WorkerDispatched {
            worker_id: WorkerId::new(),
            mandate_id: MandateId::new(),
            provider: ProviderKind::ClaudeCode,
        }))
        .await
        .unwrap();
        log.append(Observation::new(ObservationKind::WorkerCompleted {
            worker_id: WorkerId::new(),
            status: WorkerStatus::Succeeded,
        }))
        .await
        .unwrap();
        log.append(Observation::new(ObservationKind::DaemonBooted {
            version: "0.1.0".into(),
        }))
        .await
        .unwrap();
        let workers = log.query_by_kind("worker_", 10).await.unwrap();
        assert_eq!(workers.len(), 2);
        let exact = log.query_by_kind("daemon_booted", 10).await.unwrap();
        assert_eq!(exact.len(), 1);
        let none = log.query_by_kind("nonexistent_", 10).await.unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn metadata_roundtrips_through_payload() {
        let (_dir, log) = fresh_log().await;
        let obs = Observation::new(ObservationKind::OperatorMessage {
            channel: "telegram".into(),
            text: "ping".into(),
        })
        .with_metadata("from", "+15125551212")
        .with_metadata("chat_id", "42");
        log.append(obs.clone()).await.unwrap();
        let listed = log.query_recent(1).await.unwrap();
        assert_eq!(listed[0].metadata.get("chat_id"), Some(&"42".to_string()));
    }
}
