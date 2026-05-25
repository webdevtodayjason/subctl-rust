//! sqlx-backed persistence for `jobs` and `runs`.
//!
//! All public methods on this module return [`evy_core::Result`]. We use
//! the runtime query API (`sqlx::query` / `sqlx::query_as`) rather than the
//! compile-time `query!` macros so the crate builds without an offline
//! sqlx metadata file. The cost is hand-mapped rows; the benefit is that
//! `cargo build` works in any environment without `DATABASE_URL` set.

use std::path::Path;

use chrono::{DateTime, Utc};
use evy_core::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::error;
use crate::job::{Job, JobAction, JobId, Run, RunOutcome};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Open or create a sqlite database at `db_path`, run migrations, return
/// a connection pool ready for use.
pub async fn open_pool(db_path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await
        .map_err(error::from_sqlx)?;
    MIGRATOR.run(&pool).await.map_err(error::from_migrate)?;
    Ok(pool)
}

/// Insert a job into the `jobs` table. Returns the assigned `JobId`
/// (which is the same id present on the input `Job`).
pub async fn insert_job(pool: &SqlitePool, job: &Job) -> Result<JobId> {
    let action_json = serde_json::to_string(&job.action)?;
    sqlx::query(
        "INSERT INTO jobs (id, name, cron_expr, action, enabled, created_at, last_run) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(job.id.0.to_string())
    .bind(&job.name)
    .bind(&job.cron_expr)
    .bind(action_json)
    .bind(i64::from(job.enabled))
    .bind(job.created_at.to_rfc3339())
    .bind(job.last_run.map(|dt| dt.to_rfc3339()))
    .execute(pool)
    .await
    .map_err(error::from_sqlx)?;
    Ok(job.id)
}

/// Delete a job by id. Errors if the id is not present.
pub async fn delete_job(pool: &SqlitePool, id: JobId) -> Result<()> {
    let res = sqlx::query("DELETE FROM jobs WHERE id = ?1")
        .bind(id.0.to_string())
        .execute(pool)
        .await
        .map_err(error::from_sqlx)?;
    if res.rows_affected() == 0 {
        return Err(error::job_not_found(id));
    }
    Ok(())
}

/// List all jobs in the table, regardless of `enabled`.
pub async fn list_jobs(pool: &SqlitePool) -> Result<Vec<Job>> {
    let rows =
        sqlx::query("SELECT id, name, cron_expr, action, enabled, created_at, last_run FROM jobs")
            .fetch_all(pool)
            .await
            .map_err(error::from_sqlx)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(row_to_job(&row)?);
    }
    Ok(out)
}

/// List only enabled jobs.
pub async fn list_enabled_jobs(pool: &SqlitePool) -> Result<Vec<Job>> {
    let rows = sqlx::query(
        "SELECT id, name, cron_expr, action, enabled, created_at, last_run \
         FROM jobs WHERE enabled = 1",
    )
    .fetch_all(pool)
    .await
    .map_err(error::from_sqlx)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(row_to_job(&row)?);
    }
    Ok(out)
}

fn row_to_job(row: &sqlx::sqlite::SqliteRow) -> Result<Job> {
    let id_str: String = row.try_get("id").map_err(error::from_sqlx)?;
    let id_uuid = Uuid::parse_str(&id_str)
        .map_err(|e| evy_core::Error::InvalidMandate(format!("bad job uuid `{id_str}`: {e}")))?;
    let name: String = row.try_get("name").map_err(error::from_sqlx)?;
    let cron_expr: String = row.try_get("cron_expr").map_err(error::from_sqlx)?;
    let action_json: String = row.try_get("action").map_err(error::from_sqlx)?;
    let action: JobAction = serde_json::from_str(&action_json)?;
    let enabled_i: i64 = row.try_get("enabled").map_err(error::from_sqlx)?;
    let created_str: String = row.try_get("created_at").map_err(error::from_sqlx)?;
    let created_at = parse_rfc3339(&created_str)?;
    let last_run_str: Option<String> = row.try_get("last_run").map_err(error::from_sqlx)?;
    let last_run = match last_run_str {
        Some(s) => Some(parse_rfc3339(&s)?),
        None => None,
    };
    Ok(Job {
        id: JobId(id_uuid),
        name,
        cron_expr,
        action,
        enabled: enabled_i != 0,
        created_at,
        last_run,
    })
}

fn parse_rfc3339(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| evy_core::Error::InvalidMandate(format!("bad timestamp `{s}`: {e}")))
}

/// Insert a `Pending` run row, returning the new run id.
pub async fn mark_run_started(
    pool: &SqlitePool,
    job_id: JobId,
    started_at: DateTime<Utc>,
) -> Result<Uuid> {
    let id = Uuid::new_v4();
    let outcome_json = serde_json::to_string(&RunOutcome::Pending)?;
    sqlx::query(
        "INSERT INTO runs (id, job_id, started_at, finished_at, outcome) \
         VALUES (?1, ?2, ?3, NULL, ?4)",
    )
    .bind(id.to_string())
    .bind(job_id.0.to_string())
    .bind(started_at.to_rfc3339())
    .bind(outcome_json)
    .execute(pool)
    .await
    .map_err(error::from_sqlx)?;
    Ok(id)
}

/// Update an existing run row with finish time + final outcome.
pub async fn mark_run_finished(
    pool: &SqlitePool,
    run_id: Uuid,
    finished_at: DateTime<Utc>,
    outcome: &RunOutcome,
) -> Result<()> {
    let outcome_json = serde_json::to_string(outcome)?;
    sqlx::query("UPDATE runs SET finished_at = ?1, outcome = ?2 WHERE id = ?3")
        .bind(finished_at.to_rfc3339())
        .bind(outcome_json)
        .bind(run_id.to_string())
        .execute(pool)
        .await
        .map_err(error::from_sqlx)?;
    Ok(())
}

/// Update a job's `last_run` column.
pub async fn update_last_run(
    pool: &SqlitePool,
    job_id: JobId,
    last_run: DateTime<Utc>,
) -> Result<()> {
    sqlx::query("UPDATE jobs SET last_run = ?1 WHERE id = ?2")
        .bind(last_run.to_rfc3339())
        .bind(job_id.0.to_string())
        .execute(pool)
        .await
        .map_err(error::from_sqlx)?;
    Ok(())
}

/// Fetch all runs for a job, oldest first. Used by tests and (eventually)
/// the operator-facing TUI.
pub async fn list_runs_for_job(pool: &SqlitePool, job_id: JobId) -> Result<Vec<Run>> {
    let rows = sqlx::query(
        "SELECT id, job_id, started_at, finished_at, outcome FROM runs \
         WHERE job_id = ?1 ORDER BY started_at ASC",
    )
    .bind(job_id.0.to_string())
    .fetch_all(pool)
    .await
    .map_err(error::from_sqlx)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id_str: String = row.try_get("id").map_err(error::from_sqlx)?;
        let run_id = Uuid::parse_str(&id_str).map_err(|e| {
            evy_core::Error::InvalidMandate(format!("bad run uuid `{id_str}`: {e}"))
        })?;
        let job_id_str: String = row.try_get("job_id").map_err(error::from_sqlx)?;
        let job_uuid = Uuid::parse_str(&job_id_str).map_err(|e| {
            evy_core::Error::InvalidMandate(format!("bad job uuid `{job_id_str}`: {e}"))
        })?;
        let started_str: String = row.try_get("started_at").map_err(error::from_sqlx)?;
        let started_at = parse_rfc3339(&started_str)?;
        let finished_str: Option<String> = row.try_get("finished_at").map_err(error::from_sqlx)?;
        let finished_at = match finished_str {
            Some(s) => Some(parse_rfc3339(&s)?),
            None => None,
        };
        let outcome_json: String = row.try_get("outcome").map_err(error::from_sqlx)?;
        let outcome: RunOutcome = serde_json::from_str(&outcome_json)?;
        out.push(Run {
            id: run_id,
            job_id: JobId(job_uuid),
            started_at,
            finished_at,
            outcome,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobAction;
    use tempfile::tempdir;

    fn sample_job() -> Job {
        Job {
            id: JobId::new(),
            name: "heartbeat".to_owned(),
            cron_expr: "* * * * *".to_owned(),
            action: JobAction::LogHeartbeat,
            enabled: true,
            created_at: Utc::now(),
            last_run: None,
        }
    }

    #[tokio::test]
    async fn open_pool_runs_migrations() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("scheduler.db");
        let pool = open_pool(&path).await.expect("open");
        let jobs = list_jobs(&pool).await.expect("list");
        assert!(jobs.is_empty());
    }

    #[tokio::test]
    async fn insert_then_list_returns_inserted_job() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("scheduler.db");
        let pool = open_pool(&path).await.unwrap();
        let job = sample_job();
        insert_job(&pool, &job).await.unwrap();
        let listed = list_jobs(&pool).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, job.id);
        assert_eq!(listed[0].name, "heartbeat");
        assert!(matches!(listed[0].action, JobAction::LogHeartbeat));
    }

    #[tokio::test]
    async fn delete_job_removes_row() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("scheduler.db");
        let pool = open_pool(&path).await.unwrap();
        let job = sample_job();
        insert_job(&pool, &job).await.unwrap();
        delete_job(&pool, job.id).await.unwrap();
        assert!(list_jobs(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_missing_job_errors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("scheduler.db");
        let pool = open_pool(&path).await.unwrap();
        let err = delete_job(&pool, JobId::new()).await.unwrap_err();
        assert!(err.to_string().contains("job not found"));
    }

    #[tokio::test]
    async fn run_lifecycle_records_outcome() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("scheduler.db");
        let pool = open_pool(&path).await.unwrap();
        let job = sample_job();
        insert_job(&pool, &job).await.unwrap();
        let started = Utc::now();
        let run_id = mark_run_started(&pool, job.id, started).await.unwrap();
        mark_run_finished(&pool, run_id, Utc::now(), &RunOutcome::Succeeded)
            .await
            .unwrap();
        let runs = list_runs_for_job(&pool, job.id).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, RunOutcome::Succeeded);
        assert!(runs[0].finished_at.is_some());
    }

    #[tokio::test]
    async fn update_last_run_persists() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("scheduler.db");
        let pool = open_pool(&path).await.unwrap();
        let job = sample_job();
        insert_job(&pool, &job).await.unwrap();
        let when = Utc::now();
        update_last_run(&pool, job.id, when).await.unwrap();
        let listed = list_jobs(&pool).await.unwrap();
        // RFC3339 round-trip is lossless at second granularity; just
        // assert "some last_run is now set".
        assert!(listed[0].last_run.is_some());
    }
}
