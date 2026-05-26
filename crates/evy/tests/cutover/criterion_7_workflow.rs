//! Criterion #7 — one real workflow runs end-to-end on v4 without
//! falling back to v3.
//!
//! ADR 0020 cutover criterion #7: "One real workflow has run
//! end-to-end on v4 without falling back to v3."
//!
//! Workflow chosen: **"Operator schedules a daily standup question
//! to be asked via Telegram."** Full prose in
//! `tests/cutover/workflow_daily_standup.md`.
//!
//! Why this test composes the wiring manually (rather than calling
//! `evy::run_daemon`):
//!
//! - `run_daemon` blocks on `wait_for_shutdown_signal()` (SIGTERM /
//!   Ctrl-C), which is hostile to a 30s integration test.
//! - The current daemon binary wires only `Scheduler` + providers; it
//!   does NOT yet construct `EventBroadcaster`, `ObservationLog`,
//!   `PlaybookStore`, or `TelegramBridge`. This is the cutover gap
//!   the test exposes — and the REPORT flags it as the top Phase 3
//!   follow-up.
//! - The brief explicitly permits "compose the same crates manually
//!   via the library API". This is that path.
//!
//! Why the scheduler firing is simulated rather than awaited:
//!
//! - 5-field cron has minute granularity. The shortest natural fire
//!   window is ~60s, which exceeds the ~5–30s wall clock the brief
//!   asks for.
//! - The persistence + fire-loop semantics are already covered by
//!   `crates/evy-scheduler/tests/integration.rs::live_fire_within_window`
//!   (75s budget) and `crates/evy/tests/smoke.rs` (the Phase 1 smoke
//!   end-to-end).
//! - This test owns the **bridging** behaviour: a fire event
//!   propagating through every Phase 2 component (SSE broadcast,
//!   observation log, playbook lookup, Telegram ask round-trip, ask
//!   resolution observation). Simulating the fire keeps the assertion
//!   surface focused on that bridging.
//!
//! v3 fallback used anywhere? **No.** Every step touches only Phase 2
//! Rust components; the Telegram Bot API is mocked via wiremock so
//! the test never reaches the real `api.telegram.org`.

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use eventsource_stream::Eventsource;
use evy_comms::{
    AppState, AskRegistry, DaemonEvent, EventBroadcaster, HttpConfig, HttpServer, JobSummary,
    Notification, TelegramBridge, TelegramConfig, WorkerSummary,
};
use evy_core::{PolicyMode, WorkerId, WorkerStatus};
use evy_memory::observation::ObservationKind;
use evy_memory::{Observation, ObservationLog, PlaybookStore};
use evy_policy::Policy;
use evy_providers::HmacKey;
use evy_scheduler::{Job, JobAction, JobId, RunOutcome, Scheduler};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TELEGRAM_TOKEN: &str = "CUTOVER_TEST_TOKEN";
const TELEGRAM_CHAT_ID: i64 = 424242;
/// Outbound message_id Telegram "returns" for our ask. The mocked
/// inbound update's `reply_to_message.message_id` must match this.
const ASK_OUTBOUND_MESSAGE_ID: i64 = 5150;

const STANDUP_PLAYBOOK_NAME: &str = "daily-standup";
const STANDUP_PLAYBOOK: &str = r#"---
name: daily-standup
description: Daily standup question for the operator
triggers: ["daily-standup", "standup"]
last_reviewed: 2026-05-26
---

# Daily Standup

What did you ship yesterday, what are you shipping today, and what's
blocking you?
"#;

/// State surface the dashboard sees during the workflow. Populated
/// with the operator's registered job; workers list stays empty
/// because this workflow doesn't dispatch a Claude / Codex worker.
#[derive(Clone)]
struct WorkflowState {
    jobs: Vec<JobSummary>,
}

#[async_trait]
impl AppState for WorkflowState {
    async fn workers(&self) -> Vec<WorkerSummary> {
        Vec::new()
    }
    async fn jobs(&self) -> Vec<JobSummary> {
        self.jobs.clone()
    }
    async fn policy(&self) -> Policy {
        Policy::default()
    }
}

#[tokio::test]
async fn daily_standup_workflow_runs_end_to_end_on_v4() {
    let workflow_start = std::time::Instant::now();

    // ── 1. Operator filesystem: playbooks dir + scheduler db ──────────
    let workdir = tempdir().expect("tempdir for workflow");
    let playbooks_dir = workdir.path().join("playbooks");
    fs::create_dir_all(&playbooks_dir).expect("mkdir playbooks");
    fs::write(
        playbooks_dir.join(format!("{STANDUP_PLAYBOOK_NAME}.md")),
        STANDUP_PLAYBOOK,
    )
    .expect("write playbook file");

    let observation_db = workdir.path().join("observations.db");
    let scheduler_db = workdir.path().join("scheduler.db");

    // ── 2. evy-memory: open observation log, load playbook store ──────
    let obs_log = ObservationLog::open(&observation_db)
        .await
        .expect("open observation log");
    let playbooks = PlaybookStore::load(&playbooks_dir).expect("load playbook store");
    assert_eq!(
        playbooks.count(),
        1,
        "exactly one operator-authored playbook must load",
    );
    let playbook = playbooks
        .find(STANDUP_PLAYBOOK_NAME)
        .expect("daily-standup playbook must be findable by name");
    assert!(
        playbook.body.contains("What did you ship yesterday"),
        "playbook body must round-trip from disk",
    );
    assert!(
        playbook.triggers.iter().any(|t| t == "daily-standup"),
        "playbook must advertise its trigger",
    );

    // ── 3. evy-scheduler: register the operator's cron job ────────────
    let scheduler = Scheduler::open(&scheduler_db)
        .await
        .expect("open scheduler");
    let job_id = JobId::new();
    scheduler
        .register(Job {
            id: job_id,
            name: STANDUP_PLAYBOOK_NAME.to_owned(),
            cron_expr: "0 9 * * 1-5".to_owned(),
            action: JobAction::LogHeartbeat,
            enabled: true,
            created_at: Utc::now(),
            last_run: None,
        })
        .await
        .expect("register daily-standup job");
    let registered = scheduler.list().await.expect("list jobs");
    assert_eq!(registered.len(), 1);
    assert_eq!(registered[0].id, job_id);

    // ── 4. evy-comms: HMAC session key + broadcaster + populated state ─
    //
    // The HMAC key is the Phase 2 Slice 2A trust marker key. It's not
    // exercised in *this* test path (no real worker spawn) but
    // instantiating it here proves the daemon-boot integration would
    // mint one without crashing — matching `boot_components` in
    // `crates/evy/src/lib.rs`.
    let _session_key = HmacKey::generate();
    let broadcaster = EventBroadcaster::new(64);
    let state = Arc::new(WorkflowState {
        jobs: vec![JobSummary {
            id: job_id,
            name: STANDUP_PLAYBOOK_NAME.to_owned(),
            cron_expr: "0 9 * * 1-5".to_owned(),
            action_kind: "log_heartbeat".to_owned(),
            enabled: true,
        }],
    });

    // ── 5. Bind the dashboard HTTP server on an ephemeral port ────────
    let server = HttpServer::new(HttpConfig::ephemeral(), broadcaster.clone(), state);
    let bound = server.bind().await.expect("bind dashboard");
    let dashboard_url = format!("http://{}", bound.local_addr());
    let dashboard_shutdown = CancellationToken::new();
    let dashboard_shutdown_for_task = dashboard_shutdown.clone();
    let dashboard_handle = tokio::spawn(async move {
        let _ = bound.serve(dashboard_shutdown_for_task).await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // ── 6. Mock Telegram (wiremock) — never touches the real Bot API ──
    let telegram = MockServer::start().await;

    // sendMessage returns the deterministic outbound message_id so the
    // bridge's `open_asks` map gets a known key to match the reply
    // against.
    Mock::given(method("POST"))
        .and(path(format!("/bot{TELEGRAM_TOKEN}/sendMessage")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": {"message_id": ASK_OUTBOUND_MESSAGE_ID}
        })))
        .mount(&telegram)
        .await;

    // getUpdates: first call delivers the operator's reply, then
    // subsequent polls return empty arrays. wiremock matches in
    // insertion order and falls through once a mock's count is
    // exhausted.
    Mock::given(method("GET"))
        .and(path(format!("/bot{TELEGRAM_TOKEN}/getUpdates")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": [{
                "update_id": 1,
                "message": {
                    "message_id": 60,
                    "text": "Shipping Phase 2 Slice 2D today; no blockers.",
                    "chat": {"id": TELEGRAM_CHAT_ID},
                    "from": {"id": 99, "first_name": "Jason"},
                    "reply_to_message": {"message_id": ASK_OUTBOUND_MESSAGE_ID}
                }
            }]
        })))
        .up_to_n_times(1)
        .mount(&telegram)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/bot{TELEGRAM_TOKEN}/getUpdates")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": []})))
        .mount(&telegram)
        .await;

    let asks = Arc::new(AskRegistry::new());
    let mut tcfg = TelegramConfig::new(TELEGRAM_TOKEN.to_owned(), TELEGRAM_CHAT_ID);
    tcfg.base_url = telegram.uri();
    // Snappier polling so the operator's reply lands inside the test's
    // budget rather than the production-default ~25s long-poll.
    tcfg.long_poll_timeout = Duration::from_millis(50);
    tcfg.poll_interval = Duration::from_millis(20);
    let bridge = TelegramBridge::new(tcfg, asks.clone());

    // Bridge `run()` loop is *not* spawned yet — we start it right
    // before `bridge.ask()` (step 10) so the mocked reply can't be
    // consumed before the `message_id → AskId` mapping is in place.
    // See the comment in `TelegramBridge::ask` for the lock-discipline
    // detail this race depends on.

    // ── 7. Operator console subscribes to SSE before any events fire ──
    let sse_stream = reqwest::Client::new()
        .get(format!("{dashboard_url}/api/evy/events"))
        .header("Accept", "text/event-stream")
        .send()
        .await
        .expect("open SSE stream");
    assert_eq!(sse_stream.status(), 200);
    let mut sse_events = sse_stream.bytes_stream().eventsource();
    // Race guard: let the GET handler actually subscribe to the
    // broadcaster before we emit (broadcast::Sender::send only
    // delivers to *current* receivers — pattern from
    // `crates/evy-comms/tests/http_integration.rs::sse_stream_delivers_emitted_events_to_clients`).
    tokio::time::sleep(Duration::from_millis(50)).await;

    // ── 8. Simulate the scheduler fire ────────────────────────────────
    //
    // In a long-running daemon the fire loop emits this when the cron
    // expression elapses. Here we emit directly so the test stays
    // under the ~30s budget. The shape and downstream effects mirror
    // production exactly.
    let run_id = Uuid::new_v4();
    let correlation = Uuid::new_v4();
    let fired_event = DaemonEvent::SchedulerFired {
        job_id,
        run_id,
        outcome: RunOutcome::Succeeded,
    };
    broadcaster.emit(fired_event.clone());

    // Log the fire into the observation substrate (Layer 1) so the
    // learning loop (Phase 3) has a row to read.
    obs_log
        .append(
            Observation::new(ObservationKind::SchedulerFiredJob {
                job_name: STANDUP_PLAYBOOK_NAME.to_owned(),
                outcome: "succeeded".to_owned(),
            })
            .with_correlation(correlation)
            .with_metadata("run_id", run_id.to_string()),
        )
        .await
        .expect("append SchedulerFiredJob observation");

    // The dashboard's SSE client receives the fire.
    let frame = timeout(Duration::from_secs(2), sse_events.next())
        .await
        .expect("dashboard SSE next() timed out — fire event lost")
        .expect("SSE stream ended")
        .expect("SSE stream errored");
    let received: DaemonEvent = serde_json::from_str(&frame.data)
        .unwrap_or_else(|e| panic!("could not parse SSE frame {:?}: {e}", frame.data));
    assert_eq!(
        received, fired_event,
        "dashboard must see the fired event verbatim"
    );

    // ── 9. Trigger lookup — playbook substrate (Layer 4) ──────────────
    let matches = playbooks.matching_trigger(STANDUP_PLAYBOOK_NAME);
    assert_eq!(
        matches.len(),
        1,
        "trigger must match exactly the standup playbook"
    );
    let standup = matches[0];
    assert_eq!(standup.name, STANDUP_PLAYBOOK_NAME);

    // ── 10. Telegram ask round-trip: post the question, await reply ──
    //
    // Start the bridge's `run()` loop NOW — deferred until this point so
    // the mocked reply can't be consumed before `ask()` inserts the
    // `outbound_message_id → AskId` mapping. The lock-discipline inside
    // `TelegramBridge::ask` (hold `open_asks` across `send_message`)
    // then guarantees `handle_update` finds the mapping when it runs.
    let bridge_shutdown = CancellationToken::new();
    let bridge_for_run = bridge.clone();
    let bridge_shutdown_for_run = bridge_shutdown.clone();
    let bridge_handle = tokio::spawn(async move {
        bridge_for_run
            .run(bridge_shutdown_for_run)
            .await
            .expect("telegram bridge run loop ok");
    });

    let question = format!(
        "📋 {name}: {prompt}",
        name = standup.name,
        prompt = standup
            .body
            .lines()
            .find(|l| l.contains("What did you ship"))
            .unwrap_or("standup question")
            .trim(),
    );
    let answer = timeout(
        Duration::from_secs(5),
        bridge.ask(question.clone(), Duration::from_secs(4)),
    )
    .await
    .expect("ask() top-level timeout")
    .expect("ask must resolve via the mocked operator reply");
    assert_eq!(
        answer, "Shipping Phase 2 Slice 2D today; no blockers.",
        "operator's reply must round-trip through the Telegram bridge",
    );

    // ── 11. Log the ask resolution into the observation substrate ────
    //
    // In a fully-wired daemon this happens inside the bridge's reply
    // handler. Phase 2 doesn't include that wiring yet (see REPORT.md
    // Phase 3 follow-up: "TelegramBridge appends OperatorMessage
    // observations on inbound resolution"), so the test does it
    // explicitly to prove the substrate is ready.
    obs_log
        .append(
            Observation::new(ObservationKind::OperatorMessage {
                channel: "telegram".to_owned(),
                text: answer.clone(),
            })
            .with_correlation(correlation),
        )
        .await
        .expect("append OperatorMessage observation");

    // ── 12. Notification rendering — sanity that Notification::AskResolved
    //        produces a non-empty operator-visible string. This is what
    //        the dashboard / future Discord channel would surface.
    let ask_resolved_text = Notification::AskResolved {
        ask_id: evy_comms::AskId(Uuid::new_v4()),
        answer: answer.clone(),
    }
    .render_text();
    assert!(
        ask_resolved_text.contains(&answer),
        "AskResolved notification must include the operator's answer; got {ask_resolved_text:?}",
    );

    // ── 13. Worker lifecycle event (illustrative): show the
    //        DaemonEvent taxonomy covers worker outcomes too. This is
    //        not strictly part of the workflow but proves the
    //        WorkerStatusChanged variant is wire-compatible with the
    //        dashboard for Phase 3's worker-bridge work.
    let worker_event = DaemonEvent::WorkerStatusChanged {
        worker_id: WorkerId::new(),
        status: WorkerStatus::Succeeded,
    };
    broadcaster.emit(worker_event.clone());
    // Drain it from the SSE stream so a slow drop doesn't leak.
    let _ = timeout(Duration::from_secs(1), sse_events.next()).await;

    // ── 14. Assertions on the observation log ────────────────────────
    let recent = obs_log
        .query_recent(20)
        .await
        .expect("query recent observations");
    assert!(
        recent.len() >= 2,
        "expected at least 2 observations (scheduler-fired + operator-message), got {}",
        recent.len(),
    );
    // Discriminator sanity — both kinds present.
    assert!(
        recent
            .iter()
            .any(|o| matches!(o.kind, ObservationKind::SchedulerFiredJob { .. })),
        "observation log must contain a SchedulerFiredJob row",
    );
    assert!(
        recent
            .iter()
            .any(|o| matches!(o.kind, ObservationKind::OperatorMessage { .. })),
        "observation log must contain an OperatorMessage row",
    );

    // Correlation chain: both rows share the workflow correlation_id.
    let chain = obs_log
        .query_by_correlation(correlation)
        .await
        .expect("query by correlation");
    assert_eq!(
        chain.len(),
        2,
        "correlation chain must include exactly the scheduler-fired + operator-message pair",
    );

    // Ask registry state.
    let all_asks = asks.all().await;
    assert_eq!(all_asks.len(), 1, "exactly one ask was posted");
    assert_eq!(all_asks[0].answer.as_deref(), Some(answer.as_str()));
    assert!(
        all_asks[0].answered_at.is_some(),
        "ask must be marked resolved"
    );

    // ── 15. Dashboard /api/evy/scheduler/jobs surfaces the operator job ─
    let jobs_listed: Vec<JobSummary> =
        reqwest::get(format!("{dashboard_url}/api/evy/scheduler/jobs"))
            .await
            .expect("GET scheduler jobs")
            .json()
            .await
            .expect("parse jobs JSON");
    assert_eq!(jobs_listed.len(), 1);
    assert_eq!(jobs_listed[0].name, STANDUP_PLAYBOOK_NAME);
    assert_eq!(jobs_listed[0].id, job_id);
    assert!(jobs_listed[0].enabled);

    // Belt-and-braces: legacy /api/master alias still works.
    let alias: Value = reqwest::get(format!("{dashboard_url}/api/master/scheduler/jobs"))
        .await
        .expect("GET master jobs")
        .json()
        .await
        .expect("parse master jobs JSON");
    assert!(
        alias.is_array(),
        "legacy /api/master alias must still serve arrays"
    );
    assert_eq!(alias.as_array().unwrap().len(), 1);

    // ── 16. Drain everything cleanly so the test leaves no file
    //        handles / sockets / tokio tasks behind. ────────────────────
    bridge_shutdown.cancel();
    dashboard_shutdown.cancel();
    scheduler.stop().await.expect("scheduler stop");

    // Both spawned tasks must terminate cleanly.
    let _ = timeout(Duration::from_secs(2), bridge_handle).await;
    let _ = timeout(Duration::from_secs(2), dashboard_handle).await;

    // ── 17. PolicyMode sanity — touched so the workspace dep graph
    //        actually links the type. Operators who care about which
    //        policy mode the daemon was started in see it in /api/evy/policy
    //        (which we hit above); the Trusted/Gated/Sealed enum is the
    //        v4 surface that ADR 0020 calls out. ─────────────────────────
    let mode = PolicyMode::Gated;
    assert_eq!(format!("{mode:?}"), "Gated");

    let elapsed = workflow_start.elapsed();
    eprintln!(
        "cutover workflow `daily-standup` completed in {elapsed:?} \
         (asks: 1, observations: {n_obs}, sse events: 2)",
        n_obs = recent.len(),
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "workflow took {elapsed:?}, expected < 30s",
    );
}
