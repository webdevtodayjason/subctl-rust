//! Integration test for the append-only observation log.
//!
//! Exercises the full append + query surface against a tempfile-backed
//! sqlite database, including correlation grouping and ordering
//! invariants.

use evy_core::{MandateId, ProviderKind, WorkerId, WorkerStatus};
use evy_memory::observation::ObservationKind;
use evy_memory::{Observation, ObservationLog};
use tempfile::tempdir;
use uuid::Uuid;

#[tokio::test]
async fn append_and_query_recent_roundtrip() {
    let dir = tempdir().unwrap();
    let log = ObservationLog::open(&dir.path().join("obs.db"))
        .await
        .unwrap();

    let booted = Observation::new(ObservationKind::DaemonBooted {
        version: env!("CARGO_PKG_VERSION").to_owned(),
    });
    log.append(booted.clone()).await.unwrap();

    let listed = log.query_recent(10).await.unwrap();
    assert_eq!(listed.len(), 1, "exactly one row after one append");
    assert_eq!(listed[0], booted, "row must round-trip identically");
}

#[tokio::test]
async fn newest_first_ordering_holds_across_appends() {
    let dir = tempdir().unwrap();
    let log = ObservationLog::open(&dir.path().join("obs.db"))
        .await
        .unwrap();

    let first = Observation::new(ObservationKind::DaemonBooted {
        version: "0.1.0".into(),
    });
    log.append(first.clone()).await.unwrap();
    // Force a strictly later RFC3339 timestamp on the next insert.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let second = Observation::new(ObservationKind::DaemonShutdown {
        reason: "sigterm".into(),
    });
    log.append(second.clone()).await.unwrap();

    let listed = log.query_recent(10).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, second.id, "newest must come first");
    assert_eq!(listed[1].id, first.id);
}

#[tokio::test]
async fn correlation_groups_related_observations() {
    let dir = tempdir().unwrap();
    let log = ObservationLog::open(&dir.path().join("obs.db"))
        .await
        .unwrap();

    let correlation = Uuid::new_v4();
    let dispatch = Observation::new(ObservationKind::WorkerDispatched {
        worker_id: WorkerId::new(),
        mandate_id: MandateId::new(),
        provider: ProviderKind::ClaudeCode,
    })
    .with_correlation(correlation);
    let completion = Observation::new(ObservationKind::WorkerCompleted {
        worker_id: WorkerId::new(),
        status: WorkerStatus::Succeeded,
    })
    .with_correlation(correlation);
    let unrelated = Observation::new(ObservationKind::DaemonBooted {
        version: "0.1.0".into(),
    });

    log.append(dispatch.clone()).await.unwrap();
    log.append(completion.clone()).await.unwrap();
    log.append(unrelated).await.unwrap();

    let chain = log.query_by_correlation(correlation).await.unwrap();
    assert_eq!(chain.len(), 2);
    // Oldest-first within a correlation chain.
    assert_eq!(chain[0].id, dispatch.id);
    assert_eq!(chain[1].id, completion.id);
}

#[tokio::test]
async fn kind_prefix_filters_correctly() {
    let dir = tempdir().unwrap();
    let log = ObservationLog::open(&dir.path().join("obs.db"))
        .await
        .unwrap();

    log.append(Observation::new(ObservationKind::WorkerDispatched {
        worker_id: WorkerId::new(),
        mandate_id: MandateId::new(),
        provider: ProviderKind::Codex,
    }))
    .await
    .unwrap();
    log.append(Observation::new(ObservationKind::WorkerCompleted {
        worker_id: WorkerId::new(),
        status: WorkerStatus::Succeeded,
    }))
    .await
    .unwrap();
    log.append(Observation::new(ObservationKind::SchedulerFiredJob {
        job_name: "heartbeat".into(),
        outcome: "ok".into(),
    }))
    .await
    .unwrap();

    let workers = log.query_by_kind("worker_", 10).await.unwrap();
    assert_eq!(workers.len(), 2);
    let scheduler = log.query_by_kind("scheduler_", 10).await.unwrap();
    assert_eq!(scheduler.len(), 1);
    let exact_match = log.query_by_kind("worker_dispatched", 10).await.unwrap();
    assert_eq!(exact_match.len(), 1);
}
