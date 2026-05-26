//! Phase 2 Slice 2D — cutover-readiness test harness.
//!
//! This file is the integration-test binary entry point. Each cutover
//! criterion that needs a fresh test lives in a sibling module under
//! `cutover/`. Pure documentation deliverables (REPORT.md,
//! workflow_daily_standup.md) sit beside the .rs files but are not
//! compiled.
//!
//! Layout:
//!
//! ```text
//! crates/evy/tests/cutover.rs                ← this file
//! crates/evy/tests/cutover/REPORT.md         ← human-readable verdict
//! crates/evy/tests/cutover/workflow_daily_standup.md
//! crates/evy/tests/cutover/criterion_4_dashboard.rs
//! crates/evy/tests/cutover/criterion_5_scheduler.rs
//! crates/evy/tests/cutover/criterion_7_workflow.rs
//! ```
//!
//! Why a single binary instead of one per criterion: Cargo compiles each
//! top-level `tests/*.rs` as its own binary, so consolidating shaves a
//! few seconds off `cargo test --workspace` and avoids three separate
//! `target/debug/deps/criterion_*` artifacts that share nothing useful.
//!
//! Criteria #1, #2, #3, #5 (cron-fire), and #6 are verified by
//! pre-existing tests in their respective crates — the REPORT cites them
//! rather than duplicating coverage. New behavior lives only here.

mod cutover {
    pub mod criterion_4_dashboard;
    pub mod criterion_5_scheduler;
    pub mod criterion_7_workflow;
}
