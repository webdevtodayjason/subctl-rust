//! End-to-end integration test for the Phase 3 learning loop.
//!
//! Walks the operator-side flow:
//!
//! 1. The orchestrator dispatches a worker (writes a `WorkerDispatched`
//!    observation and records a score for that provider × task_class).
//! 2. The worker reaches a terminal state (writes a `WorkerCompleted`
//!    observation and records the corresponding outcome to the ledger).
//! 3. The operator pushes back on the result (an `Approved` /
//!    `Rejected` / `OperatorPreference` feedback event lands).
//! 4. After enough cycles, `ScoreLedger::recommend` flips its
//!    recommendation toward the higher-success-rate provider.
//! 5. `OperatorPreference` feedback round-trips through the preference
//!    model.
//!
//! Every store opens against a single tempfile sqlite. Migrations are
//! idempotent so each `::open()` call is a no-op after the first.

use std::collections::HashMap;
use std::sync::Arc;

use evy_core::{MandateId, ProviderKind, WorkerId, WorkerStatus};
use evy_memory::feedback::{Feedback, FeedbackContext, FeedbackIngest, FeedbackKind};
use evy_memory::observation::ObservationKind;
use evy_memory::preferences::{OperatorPreferenceModel, PreferenceKey, PreferenceValue};
use evy_memory::scoring::ScoreLedger;
use evy_memory::{Observation, ObservationLog};
use tempfile::tempdir;
use uuid::Uuid;

/// All four stores opened against the same db file.
struct Stack {
    log: Arc<ObservationLog>,
    scoring: Arc<ScoreLedger>,
    prefs: Arc<OperatorPreferenceModel>,
    ingest: FeedbackIngest,
}

async fn fresh_stack() -> (tempfile::TempDir, Stack) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let log = Arc::new(ObservationLog::open(&path).await.unwrap());
    let scoring = Arc::new(ScoreLedger::open(&path).await.unwrap());
    let prefs = Arc::new(OperatorPreferenceModel::open(&path).await.unwrap());
    let ingest = FeedbackIngest::open(&path, log.clone(), scoring.clone(), prefs.clone())
        .await
        .unwrap();
    (
        dir,
        Stack {
            log,
            scoring,
            prefs,
            ingest,
        },
    )
}

/// One simulated dispatch → outcome cycle. Writes both observations
/// (dispatch and completion correlated by `correlation_id`), then
/// ingests an operator-feedback event mirroring the outcome. The
/// feedback's metadata carries provider / task_class / duration_ms, so
/// ingest itself drives the scoring update — exercising the layer 7 →
/// layer 3 wiring end-to-end. Direct `ScoreLedger::record` callers are
/// covered by `scoring.rs`'s unit tests.
async fn cycle(
    stack: &Stack,
    provider: ProviderKind,
    task_class: &str,
    outcome: WorkerStatus,
    duration_ms: u64,
) {
    let worker_id = WorkerId::new();
    let mandate_id = MandateId::new();
    let correlation = Uuid::new_v4();

    stack
        .log
        .append(
            Observation::new(ObservationKind::WorkerDispatched {
                worker_id,
                mandate_id,
                provider,
            })
            .with_correlation(correlation),
        )
        .await
        .unwrap();

    stack
        .log
        .append(
            Observation::new(ObservationKind::WorkerCompleted {
                worker_id,
                status: outcome.clone(),
            })
            .with_correlation(correlation),
        )
        .await
        .unwrap();

    let feedback_kind = match &outcome {
        WorkerStatus::Succeeded => FeedbackKind::Approved,
        WorkerStatus::Failed(reason) => FeedbackKind::Rejected {
            reason: reason.clone(),
        },
        // Cancelled / Pending / Running don't arrive as feedback in the
        // happy path; treat as rejection for the purposes of the test.
        _ => FeedbackKind::Rejected {
            reason: "non-success terminal state".into(),
        },
    };

    let mut meta = HashMap::new();
    meta.insert("provider".into(), provider_str(provider).into());
    meta.insert("task_class".into(), task_class.into());
    meta.insert("duration_ms".into(), duration_ms.to_string());
    let fb = Feedback::new(
        feedback_kind,
        FeedbackContext {
            related_worker: Some(worker_id),
            related_mandate: Some(mandate_id),
            action_proposed: format!("dispatch {:?} → {task_class}", provider),
            metadata: meta,
            ..FeedbackContext::default()
        },
    );
    stack.ingest.ingest(fb).await.unwrap();
}

fn provider_str(p: ProviderKind) -> &'static str {
    match p {
        ProviderKind::ClaudeCode => "ClaudeCode",
        ProviderKind::Codex => "Codex",
        ProviderKind::DeepSeek => "DeepSeek",
    }
}

#[tokio::test]
async fn recommendation_flips_after_repeated_failures() {
    let (_dir, stack) = fresh_stack().await;

    // 5 ClaudeCode failures on rust-refactor.
    for _ in 0..5 {
        cycle(
            &stack,
            ProviderKind::ClaudeCode,
            "rust-refactor",
            WorkerStatus::Failed("compile error".into()),
            2_000,
        )
        .await;
    }

    // 5 Codex successes on the same task class. Recommendation should
    // now name Codex.
    for _ in 0..5 {
        cycle(
            &stack,
            ProviderKind::Codex,
            "rust-refactor",
            WorkerStatus::Succeeded,
            1_500,
        )
        .await;
    }

    // Each cycle:
    //   - dispatch observation + completion observation = 10 rows
    //   - feedback ingest mirrors a feedback_received observation = 1 row
    // 10 cycles × 3 observations + bookkeeping = 30 obs total.
    let total_obs = stack.log.count().await.unwrap();
    assert!(
        total_obs >= 30,
        "expected ≥30 observations, got {total_obs}"
    );

    let claude_score = stack
        .scoring
        .score(ProviderKind::ClaudeCode, "rust-refactor")
        .await
        .unwrap()
        .expect("claude row");
    assert_eq!(claude_score.failures, 5);
    assert_eq!(claude_score.successes, 0);

    let codex_score = stack
        .scoring
        .score(ProviderKind::Codex, "rust-refactor")
        .await
        .unwrap()
        .expect("codex row");
    assert_eq!(codex_score.successes, 5);
    assert_eq!(codex_score.failures, 0);

    let recommendation = stack.scoring.recommend("rust-refactor").await.unwrap();
    assert_eq!(
        recommendation,
        Some(ProviderKind::Codex),
        "after 5 failures and 5 successes, the recommendation must be Codex"
    );

    // The feedback table holds one row per cycle (10 total).
    let feedback_rows = stack.ingest.recent(50).await.unwrap();
    assert_eq!(feedback_rows.len(), 10);

    // Of which exactly 5 are approved and 5 rejected.
    let approved = stack.ingest.by_kind("approved", 50).await.unwrap();
    assert_eq!(approved.len(), 5);
    let rejected = stack.ingest.by_kind("rejected", 50).await.unwrap();
    assert_eq!(rejected.len(), 5);
}

#[tokio::test]
async fn operator_preference_round_trips_through_feedback() {
    let (_dir, stack) = fresh_stack().await;
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
    stack.ingest.ingest(fb).await.unwrap();
    let got = stack
        .prefs
        .get(&PreferenceKey::from("prefer_codex_for_rust"))
        .await
        .unwrap()
        .expect("preference exists");
    assert_eq!(got, PreferenceValue::Text("true".into()));
}

#[tokio::test]
async fn typed_preferences_round_trip_directly() {
    // Confirms the preference model also supports the typed variants
    // (not just the Text-wrapping path that feedback ingest uses).
    let (_dir, stack) = fresh_stack().await;
    let cases: Vec<(PreferenceKey, PreferenceValue)> = vec![
        (
            PreferenceKey::from("idle_banner_loud"),
            PreferenceValue::Boolean(true),
        ),
        (
            PreferenceKey::from("min_batch"),
            PreferenceValue::Number(3.0),
        ),
        (
            PreferenceKey::from("preferred_providers"),
            PreferenceValue::List(vec!["Codex".into(), "ClaudeCode".into()]),
        ),
    ];
    for (k, v) in &cases {
        stack.prefs.set(k.clone(), v.clone()).await.unwrap();
    }
    for (k, v) in &cases {
        assert_eq!(stack.prefs.get(k).await.unwrap().as_ref(), Some(v));
    }
    let listed = stack.prefs.list().await.unwrap();
    assert_eq!(listed.len(), cases.len());
}

#[tokio::test]
async fn feedback_mirror_observations_correlate_to_feedback_rows() {
    let (_dir, stack) = fresh_stack().await;
    cycle(
        &stack,
        ProviderKind::Codex,
        "rust-refactor",
        WorkerStatus::Succeeded,
        1_000,
    )
    .await;
    // The feedback mirror lives under the kind discriminator
    // `feedback_received`. There must be exactly one mirror row per
    // ingest.
    let mirrors = stack
        .log
        .query_by_kind("feedback_received", 10)
        .await
        .unwrap();
    assert_eq!(mirrors.len(), 1);
    let mirror_correlation = mirrors[0].correlation_id.expect("mirror has correlation");

    // The feedback row's id matches the mirror's correlation id.
    let chain = stack
        .log
        .query_by_correlation(mirror_correlation)
        .await
        .unwrap();
    assert_eq!(chain.len(), 1);
    let fb_lookup = stack.ingest.get(mirror_correlation).await.unwrap();
    assert!(
        fb_lookup.is_some(),
        "the mirror's correlation_id should match the feedback row's id"
    );
}
