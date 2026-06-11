//! Integration tests for the Phase 5 watchdogs.
//!
//! `AutoNudgeWatchdog` and `TeamStalenessWatchdog` are unit-tested inside
//! the crate against synthesised contexts; this file exercises them end-
//! to-end through the [`WatchdogRegistry`] tick loop so we catch any
//! integration regressions in the broadcaster / observation-log path.
//!
//! Wave 4 (orch events) adds the live-SSE suite at the bottom: the
//! cockpit vocabulary (`watchdog_fire` / `watchdog_ok` / `team_event`)
//! must reach a LIVE `/api/evy/events` subscription served by
//! evy-comms's `HttpServer` when the watchdog sources fire — ONE
//! `EventBroadcaster` shared between server and `WatchdogContext`,
//! exactly the daemon's wiring.
//!
//! All tests use real wall-clock time (not `tokio::time::pause`) because
//! the observation log writes via sqlx, which is incompatible with the
//! paused runtime — same constraint as the existing `registry_smoke`
//! test.

use std::sync::Arc;
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use eventsource_stream::{Event, Eventsource};
use evy_comms::{DaemonEvent, EventBroadcaster, HttpConfig, HttpServer};
use evy_memory::ObservationLog;
use evy_scheduler::Scheduler;
use evy_watchdog::{
    AutoNudgeConfig, AutoNudgeWatchdog, InMemoryTeamRegistry, MockDirectiveDispatcher,
    MockTmuxQuery, TeamGcWatchdog, TeamRecord, TeamStalenessConfig, TeamStalenessWatchdog,
    Watchdog, WatchdogContext, WatchdogRegistry,
};
use futures_util::StreamExt;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// Build a context around `events` (pass `EventBroadcaster::default()`
/// when the test doesn't share the bus with an HTTP server).
async fn build_ctx(events: EventBroadcaster) -> (WatchdogContext, TempDir, TempDir) {
    let scheduler_dir = tempfile::tempdir().unwrap();
    let scheduler = Arc::new(
        Scheduler::open(&scheduler_dir.path().join("s.db"))
            .await
            .unwrap(),
    );
    let obs_dir = tempfile::tempdir().unwrap();
    let obs_log = Arc::new(
        ObservationLog::open(&obs_dir.path().join("obs.db"))
            .await
            .unwrap(),
    );
    let ctx = WatchdogContext {
        providers: Vec::new(),
        scheduler,
        events,
        obs_log,
    };
    (ctx, scheduler_dir, obs_dir)
}

/// Spin up AutoNudge with aggressive thresholds (idle = 0, cooldown = 0,
/// escalation = 2) and a frozen pane — observe via the registry's
/// SSE broadcaster that the loop:
///   1. ticks the watchdog at least 3 times,
///   2. emits at least one nudge directive to the dispatcher,
///   3. eventually fires a `WorkerDead` finding (visible via the event
///      stream's `finding_count`).
#[tokio::test]
async fn auto_nudge_emits_findings_through_registry_loop() {
    let (ctx, _s_dir, _o_dir) = build_ctx(EventBroadcaster::default()).await;
    let mut rx = ctx.events.subscribe();

    let tmux = Arc::new(MockTmuxQuery::new());
    tmux.set_sessions(["claude-frozen"]);
    tmux.set_pane("claude-frozen:0", "$ never moves");
    let dispatcher = Arc::new(MockDirectiveDispatcher::new());

    let nudger = AutoNudgeWatchdog::new(tmux, dispatcher.clone())
        .with_tick_interval_secs(1)
        .with_config(AutoNudgeConfig {
            idle_threshold_secs: 0,
            nudge_cooldown_secs: 0,
            escalation_threshold: 2,
        });

    let mut reg = WatchdogRegistry::new();
    reg.tick_timeout = Duration::from_secs(2);
    reg.add(Arc::new(nudger));
    let reg = Arc::new(reg);
    let cancel = CancellationToken::new();
    reg.clone().start(ctx, cancel.clone()).await.unwrap();

    // Collect events for ~3 seconds — the 1s schedule yields 3-4 ticks.
    let event_handle = tokio::spawn(async move {
        let mut got = Vec::new();
        for _ in 0..8 {
            match tokio::time::timeout(Duration::from_millis(800), rx.recv()).await {
                Ok(Ok(ev)) => got.push(ev),
                _ => break,
            }
        }
        got
    });

    tokio::time::sleep(Duration::from_millis(3200)).await;
    cancel.cancel();
    reg.join().await;
    let events = event_handle.await.unwrap();

    // At least one event must be from auto-nudge and the dispatcher
    // must have received at least one directive.
    let nudge_events: Vec<_> = events
        .iter()
        .filter_map(|ev| match ev {
            DaemonEvent::WatchdogTick {
                name,
                finding_count,
                healthy,
            } if name == "auto-nudge" => Some((*finding_count, *healthy)),
            _ => None,
        })
        .collect();
    assert!(
        !nudge_events.is_empty(),
        "expected at least one auto-nudge WatchdogTick event; got {events:?}",
    );
    assert!(
        nudge_events.iter().all(|(_, healthy)| *healthy),
        "auto-nudge ticks should be healthy even when emitting findings",
    );
    assert!(
        dispatcher.count() >= 1,
        "expected at least one nudge dispatched; got {}",
        dispatcher.count(),
    );
    // At least one tick should have produced a finding (nudge or
    // escalation) — first sight is skipped, so this is the strongest
    // assertion the event payload alone supports without scraping the
    // observation log.
    assert!(
        nudge_events.iter().any(|(count, _)| *count >= 1),
        "expected at least one nudge tick to emit a finding; got {nudge_events:?}",
    );
}

/// Run TeamStaleness through the registry against a registry that
/// contains one alive-but-quiet team. Verify the tick loop produces
/// `Finding::StaleTeam` via the event stream and that the registry
/// is left untouched (no GC).
#[tokio::test]
async fn team_staleness_emits_stale_finding_through_registry() {
    let (ctx, _s_dir, _o_dir) = build_ctx(EventBroadcaster::default()).await;

    let teams = Arc::new(InMemoryTeamRegistry::new());
    teams.insert(TeamRecord {
        team_id: "claude-quiet".into(),
        tmux_session: "claude-quiet".into(),
        last_activity: Some(Utc::now() - ChronoDuration::hours(2)),
    });
    let tmux = Arc::new(MockTmuxQuery::new());
    tmux.set_sessions(["claude-quiet"]);

    let stale = TeamStalenessWatchdog::new(teams.clone(), tmux)
        .with_tick_interval_secs(1)
        .with_config(TeamStalenessConfig {
            stale_threshold_secs: 60,
        });

    // Pop the obs_log handle so we can grep it post-cancel.
    let obs_log = ctx.obs_log.clone();
    let mut rx = ctx.events.subscribe();

    let mut reg = WatchdogRegistry::new();
    reg.tick_timeout = Duration::from_secs(2);
    reg.add(Arc::new(stale));
    let reg = Arc::new(reg);
    let cancel = CancellationToken::new();
    reg.clone().start(ctx, cancel.clone()).await.unwrap();

    // Wait for the tick's WatchdogTick event. The stale tick ALSO emits
    // the cockpit's `watchdog_fire` DashboardFrame onto the same bus
    // (Wave 4 orch events) — record it on the way past.
    let mut saw_fire_frame = false;
    let tick_ev = loop {
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("event arrival timed out")
            .expect("broadcaster closed");
        match ev {
            DaemonEvent::DashboardFrame { ref event, .. } if event == "watchdog_fire" => {
                saw_fire_frame = true;
            }
            DaemonEvent::WatchdogTick { .. } => break ev,
            other => panic!("unexpected event: {other:?}"),
        }
    };
    cancel.cancel();
    reg.join().await;

    match tick_ev {
        DaemonEvent::WatchdogTick {
            name,
            healthy,
            finding_count,
        } => {
            assert_eq!(name, "team-staleness");
            assert!(
                healthy,
                "stale-but-alive team must not flip watchdog health"
            );
            assert!(
                finding_count >= 1,
                "expected at least one StaleTeam finding; got {finding_count}",
            );
        }
        other => panic!("unexpected event: {other:?}"),
    }
    assert!(
        saw_fire_frame,
        "a stale tick must broadcast the cockpit's watchdog_fire frame"
    );

    // The registry must be untouched — TeamStaleness is observation-
    // only; deletion belongs to TeamGcWatchdog.
    assert_eq!(teams.len(), 1, "TeamStaleness must not mutate the registry");

    // The observation log should have at least one row for our
    // watchdog. We don't care about the schema beyond a smoke check
    // that nothing errored on append.
    let _ = obs_log
        .query_recent(10)
        .await
        .expect("observation log readable");
}

// ─── Wave 4 (orch events): cockpit frames on a LIVE /api/evy/events ──────
//
// The dashboard's Orchestration tab (`dashboard/public/tabs/orch.js`)
// opens an EventSource and listens for named frames:
//
// | frame | listener parses | v4 source under test |
// |---|---|---|
// | `watchdog_fire` | `{ts, prompt, stale:[…]}` | `TeamStalenessWatchdog` tick with a stale-but-alive team |
// | `watchdog_ok`   | `{ts, teams_tracked, stale}` | `TeamStalenessWatchdog` clean tick |
// | `team_event`    | `{team, type, text}` | `TeamGcWatchdog` removing a dead team |
//
// Sources fire through their internal APIs (`Watchdog::tick`) against an
// `InMemoryTeamRegistry` + `MockTmuxQuery` — no live tmux.

/// Boot the evy-comms HTTP server on an ephemeral port sharing
/// `broadcaster`, and return `(base_url, shutdown)`.
async fn spawn_http(broadcaster: EventBroadcaster) -> (String, CancellationToken) {
    let server = HttpServer::with_stub_state(HttpConfig::ephemeral(), broadcaster);
    let bound = server.bind().await.expect("bind");
    let addr = bound.local_addr();
    let shutdown = CancellationToken::new();
    let st = shutdown.clone();
    tokio::spawn(async move {
        let _ = bound.serve(st).await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    (format!("http://{addr}"), shutdown)
}

/// Open a live `/api/evy/events` subscription (the cockpit's EventSource).
async fn subscribe(
    base: &str,
) -> impl futures_util::Stream<
    Item = Result<Event, eventsource_stream::EventStreamError<reqwest::Error>>,
> + Unpin {
    let res = reqwest::Client::new()
        .get(format!("{base}/api/evy/events"))
        .header("Accept", "text/event-stream")
        .send()
        .await
        .expect("events GET");
    assert_eq!(res.status(), 200);
    let stream = res.bytes_stream().eventsource();
    // Give the subscription a beat to register — broadcast only reaches
    // current subscribers.
    tokio::time::sleep(Duration::from_millis(50)).await;
    stream
}

/// Drain the stream until the first frame named `event`, with a timeout.
async fn next_named_frame(
    events: &mut (impl futures_util::Stream<
        Item = Result<Event, eventsource_stream::EventStreamError<reqwest::Error>>,
    > + Unpin),
    event: &str,
) -> serde_json::Value {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(3), events.next())
            .await
            .unwrap_or_else(|_| panic!("`{event}` frame timed out"))
            .expect("event stream ended")
            .expect("event stream errored");
        if frame.event == event {
            return serde_json::from_str(&frame.data).expect("frame data is JSON");
        }
    }
}

#[tokio::test]
async fn watchdog_fire_reaches_live_events_subscription() {
    let broadcaster = EventBroadcaster::new(64);
    let (base, shutdown) = spawn_http(broadcaster.clone()).await;
    let mut events = subscribe(&base).await;
    let (ctx, _s_dir, _o_dir) = build_ctx(broadcaster).await;

    // A stale-but-alive team trips the staleness canary.
    let teams = Arc::new(InMemoryTeamRegistry::new());
    teams.insert(TeamRecord {
        team_id: "quiet".into(),
        tmux_session: "claude-quiet".into(),
        last_activity: Some(Utc::now() - ChronoDuration::hours(2)),
    });
    let tmux = Arc::new(MockTmuxQuery::new());
    tmux.set_sessions(["claude-quiet"]);
    let w = TeamStalenessWatchdog::new(teams, tmux);
    w.tick(&ctx).await.expect("tick");

    // orch.js:451 — prompt string + stale array.
    let data = next_named_frame(&mut events, "watchdog_fire").await;
    assert_eq!(data["stale"], serde_json::json!(["quiet"]));
    let prompt = data["prompt"].as_str().expect("prompt is a string");
    assert!(prompt.contains("quiet"), "prompt names the team: {prompt}");
    assert!(data["ts"].as_str().expect("ts").ends_with('Z'));

    shutdown.cancel();
}

#[tokio::test]
async fn watchdog_ok_reaches_live_events_subscription() {
    let broadcaster = EventBroadcaster::new(64);
    let (base, shutdown) = spawn_http(broadcaster.clone()).await;
    let mut events = subscribe(&base).await;
    let (ctx, _s_dir, _o_dir) = build_ctx(broadcaster).await;

    // A fresh, alive team → clean pass.
    let teams = Arc::new(InMemoryTeamRegistry::new());
    teams.insert(TeamRecord {
        team_id: "fresh".into(),
        tmux_session: "claude-fresh".into(),
        last_activity: Some(Utc::now()),
    });
    let tmux = Arc::new(MockTmuxQuery::new());
    tmux.set_sessions(["claude-fresh"]);
    let w = TeamStalenessWatchdog::new(teams, tmux);
    w.tick(&ctx).await.expect("tick");

    // orch.js:458 — numeric counts.
    let data = next_named_frame(&mut events, "watchdog_ok").await;
    assert_eq!(data["teams_tracked"], 1);
    assert_eq!(data["stale"], 0);
    assert!(data["ts"].as_str().expect("ts").ends_with('Z'));

    shutdown.cancel();
}

#[tokio::test]
async fn team_event_reaches_live_events_subscription_on_gc_removal() {
    let broadcaster = EventBroadcaster::new(64);
    let (base, shutdown) = spawn_http(broadcaster.clone()).await;
    let mut events = subscribe(&base).await;
    let (ctx, _s_dir, _o_dir) = build_ctx(broadcaster).await;

    // A dead-session team with no activity → GC removes it from the
    // registry — an orchestration-registry state-change.
    let teams = Arc::new(InMemoryTeamRegistry::new());
    teams.insert(TeamRecord {
        team_id: "ghost".into(),
        tmux_session: "claude-ghost".into(),
        last_activity: None,
    });
    let tmux = Arc::new(MockTmuxQuery::new()); // no live sessions
    let w = TeamGcWatchdog::new(teams.clone(), tmux);
    w.tick(&ctx).await.expect("tick");
    assert!(teams.is_empty(), "GC removed the team");

    // orch.js:445 — {team, type, text}.
    let data = next_named_frame(&mut events, "team_event").await;
    assert_eq!(data["team"], "ghost");
    assert_eq!(data["type"], "dead");
    assert_eq!(data["text"], "tmux_session_gone:no_activity");

    shutdown.cancel();
}
