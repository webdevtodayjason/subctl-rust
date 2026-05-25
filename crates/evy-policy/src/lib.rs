//! `evy-policy` — the policy gate for Evy v4. Classifies every action as
//! Trusted, Gated, or Sealed. Rust port of the v3 TypeScript reference at
//! `subctl/components/evy/tools/policy/*.ts`.
//!
//! # Public surface
//!
//! ```ignore
//! use evy_policy::{
//!     check_command, check_command_simple,
//!     load_policy, load_resolved_policy,
//!     write_snapshot, read_snapshot, get_snapshot_path,
//!     AuditWriter, AuditEntry,
//!     tokenize,
//!     Policy, GatedMode, AllowPattern, CheckOutcome, CheckRequest, Mode,
//! };
//! ```
//!
//! Each module has its own focused responsibility:
//!
//! - [`mod@tokenize`] — shell-aware tokenizer mirroring v3's
//!   `shell-quote`-based parser. Pure function, no I/O.
//! - [`check`] — hot-path policy decision; pack 06 §4 reference algorithm.
//! - [`load`] — TOML loader + four-source merger (project / user / preset /
//!   shipped defaults).
//! - [`snapshot`] — per-team snapshot writer + reader; format is
//!   structurally compatible with v3's `policy.snapshot.toml` (modulo a
//!   `port_origin = "rust"` line in the header).
//! - [`audit`] — JSONL append-only audit log per pack 09. Rotation is
//!   omitted in this Phase 1 port; entries match v3's reader format.
//! - [`types`] — shared shapes (Policy, GatedMode, AllowPattern, …).

#[cfg(test)]
pub(crate) mod testlock {
    //! Process-wide env-var serialization for tests. Every module that
    //! mutates `std::env` must lock this before changing anything and hold
    //! the guard for the full test body — otherwise tests across modules
    //! race on shared env state (cargo runs tests within a single binary
    //! on multiple threads by default).
    use std::sync::Mutex;
    pub static ENV_LOCK: Mutex<()> = Mutex::new(());
}

pub mod audit;
pub mod check;
pub mod load;
pub mod snapshot;
pub mod tokenize;
pub mod types;

// Public re-exports — the surface binary-wirer consumes.
pub use audit::{get_audit_log_path, AuditWriter};
pub use check::{check_command, check_command_simple};
pub use load::{
    compute_allowlist_sha, load_policy, load_preset, load_project_policy, load_resolved_policy,
    load_shipped_defaults, load_user_policy, merge_policies, resolve_subctl_install,
};
pub use snapshot::{
    get_snapshot_path, read_snapshot, write_snapshot, write_snapshot_with_local_mode,
    PolicySnapshot,
};
pub use tokenize::tokenize;
pub use types::{
    AllowPattern, AllowSection, AuditDecision, AuditEntry, AuditEventType, CheckOutcome,
    CheckRequest, DenyAlways, Escalation, GatedMode, JustTable, MakeTable, Mode, ModeTables,
    Policy, PolicyMeta, PythonModulesTable, RunTargetsTable, ScriptTable, SealedMode, TrustedMode,
};
