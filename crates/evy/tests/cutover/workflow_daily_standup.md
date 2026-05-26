# Workflow: Daily Standup via Telegram (Cutover Criterion #7)

**Test:** `crates/evy/tests/cutover/criterion_7_workflow.rs`
**Function:** `daily_standup_workflow_runs_end_to_end_on_v4`
**Wall clock:** ~2 seconds in CI
**v3 fallback used anywhere?** No.

## Story

Jason (the operator) wants the daemon to ask him a daily-standup
question every weekday morning so he can keep a running log of what
he shipped + what's blocked. The exchange happens over Telegram (Jason's
preferred async-attention channel), the question is sourced from an
operator-editable playbook on disk, and every step writes an
observation into the learning-loop substrate so the eventual Phase 3
distillation work has data to read.

This is the **end-to-end** cutover criterion: every component the
workflow touches is Rust v4, no v3 fallback anywhere.

## Mapping each step to a v4 component

| Step | Component | Phase 2 Slice | Verification in test |
|------|-----------|--------------|----------------------|
| 1. Operator authors `~/.evy/playbooks/daily-standup.md` | `evy-memory::PlaybookStore` | 2C | `playbooks.find("daily-standup")` returns the playbook with the expected triggers + body |
| 2. Operator registers a cron job (9am weekdays) | `evy-scheduler::Scheduler::register` | 1E + 2A | `scheduler.list()` returns the registered job; survives drop + reopen (covered in criterion_5_scheduler.rs) |
| 3. Daemon mints a per-session HMAC trust marker | `evy-providers::HmacKey::generate` | 2A | `HmacKey::generate()` constructs without panic — Slice 2A's `boot_components` does the same |
| 4. Daemon opens the dashboard HTTP/SSE server | `evy-comms::HttpServer::bind` | 2B1 | `bound.local_addr()` reports the kernel-assigned port; `/api/evy/scheduler/jobs` returns the registered job |
| 5. Operator console opens the SSE stream | `evy-comms::EventBroadcaster` + axum SSE | 2B1 | `reqwest` GET with `Accept: text/event-stream`, content-type asserted |
| 6. Scheduler fires the daily-standup job | `evy-scheduler` fire loop (simulated — see note below) | 1E + 2A | `broadcaster.emit(DaemonEvent::SchedulerFired{...})` — SSE client receives the JSON-encoded event |
| 7. Daemon writes a `SchedulerFiredJob` observation | `evy-memory::ObservationLog::append` | 2C | `obs_log.query_recent(20)` includes the row with the expected discriminator |
| 8. Daemon looks up the playbook by trigger | `evy-memory::PlaybookStore::matching_trigger` | 2C | `matching_trigger("daily-standup")` returns exactly one playbook |
| 9. Daemon posts the question via Telegram | `evy-comms::TelegramBridge::ask` | 2B2 | wiremock-mocked `POST /sendMessage` with the deterministic outbound `message_id` |
| 10. Operator replies on Telegram | `evy-comms::TelegramBridge::run` (long-poll) | 2B2 | wiremock-mocked `GET /getUpdates` returning a single reply with `reply_to_message.message_id` matching step 9 |
| 11. Bridge resolves the ask, returns the answer | `evy-comms::AskRegistry::resolve` (via `TelegramBridge::handle_update`) | 2B2 | `bridge.ask(...).await` returns the operator's reply text |
| 12. Daemon writes an `OperatorMessage` observation | `evy-memory::ObservationLog::append` | 2C | `obs_log.query_by_correlation(correlation_id)` returns both rows in the chain |
| 13. Operator console sees the `AskResolved` notification | `evy-comms::Notification::render_text` | 2B2 | rendered string contains the operator's answer |

## Why the scheduler fire is simulated rather than awaited

The fire loop is 5-field cron — minimum granularity is one minute.
The shortest natural fire window is therefore ~60 seconds (cron
expression `* * * * *`), which exceeds the ~5–30 second wall-clock
budget the brief asks for.

The fire loop's persistence + scheduling semantics are already
covered by:

- `crates/evy-scheduler/tests/integration.rs::live_fire_within_window`
  — registers `* * * * *`, waits up to 75 seconds, asserts a
  `Succeeded` run row.
- `crates/evy/tests/smoke.rs::phase1_smoke_succeeds_under_75_seconds`
  — the daemon library's `run_smoke_test` boots the full Phase 1
  stack and fires the heartbeat job end-to-end.

This workflow test deliberately owns the **bridging** behavior — a
fire event propagating through every Phase 2 component — and
simulates the fire so the assertion surface stays focused on that
bridging.

## Telegram safety

`TelegramConfig::base_url` is overridden to point at a `wiremock`
mock server. The real `api.telegram.org` is never contacted. The
test would fail the same way regardless of network availability.

## What this test does NOT do

- It does NOT call `evy::run_daemon` — that function blocks on
  `wait_for_shutdown_signal()` (SIGTERM / Ctrl-C), incompatible with
  a 30-second test budget. The brief explicitly permits "compose the
  same crates manually" — this is that path.
- It does NOT spawn a real Claude Code or Codex worker — there is no
  worker dispatch in this workflow. Criterion #1 (worker spawn
  parity) is covered by the `#[ignore]`'d integration tests in
  `crates/evy-providers/tests/real_claude_code_spawn.rs` and
  `real_codex_spawn.rs`.
- It does NOT exercise the `DaemonEvent::PolicyChecked` SSE variant —
  that's verified by `crates/evy-comms/tests/http_integration.rs`
  and the policy crate's own 134 vectors.

## Cutover gaps the workflow surfaces

The test composes components the **daemon binary** does not yet wire.
These are the top Phase 3 follow-ups (also listed in REPORT.md):

1. `run_daemon` constructs no `EventBroadcaster` — workers + scheduler
   + policy outcomes are not broadcast to the SSE stream.
2. `run_daemon` opens no `ObservationLog` — nothing is written to the
   learning-loop substrate at runtime.
3. `run_daemon` constructs no `TelegramBridge` — outbound
   notifications + inbound asks have no transport.
4. `run_daemon` loads no `PlaybookStore` — playbooks on disk are
   invisible to the daemon.
5. `run_daemon` constructs no `HttpServer` — the dashboard /
   `/api/master/*` aliases serve nothing in production. (The
   placeholder `http_port = 7654` log line is decorative.)
6. `AppState` for the dashboard is `StubAppState` (returns empty
   workers + empty jobs + default policy). Phase 3 needs a real
   `Arc<dyn AppState>` that reads through to the scheduler + provider
   layer.

Each of these is a one-or-two-file change to `crates/evy/src/lib.rs`
(`run_daemon`) — the libraries themselves are ready and tested.
