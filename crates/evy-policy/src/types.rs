//! Type contracts for the policy engine — the Rust port of v3's
//! `lib/policy/types.ts`.
//!
//! Every field name is the snake_case of the TS field so that the same
//! `.subctl/policy.toml` files used by the v3 daemon parse here without
//! transformation. The doc comments paraphrase pack 06 §3 / pack 02 / pack 03
//! from the v3 handoff bundle.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The three policy modes a worker can spawn in.
///
/// Lower-cased on the wire to match `.subctl/policy.toml` and v3's JSONL
/// audit format. The workspace-level [`evy_core::PolicyMode`] uses Pascal
/// case in its `Serialize` impl; this type intentionally diverges so the
/// loader can deserialize v3 TOML/JSONL files verbatim. Conversion goes
/// through [`Mode::from_core`] and [`Mode::to_core`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Trusted,
    Gated,
    Sealed,
}

impl Mode {
    /// Convert from the workspace `PolicyMode` enum.
    #[must_use]
    pub fn from_core(m: evy_core::PolicyMode) -> Self {
        match m {
            evy_core::PolicyMode::Trusted => Self::Trusted,
            evy_core::PolicyMode::Gated => Self::Gated,
            evy_core::PolicyMode::Sealed => Self::Sealed,
        }
    }

    /// Convert to the workspace `PolicyMode` enum.
    #[must_use]
    pub fn to_core(self) -> evy_core::PolicyMode {
        match self {
            Self::Trusted => evy_core::PolicyMode::Trusted,
            Self::Gated => evy_core::PolicyMode::Gated,
            Self::Sealed => evy_core::PolicyMode::Sealed,
        }
    }

    /// Render as the lowercase string used in TOML / JSONL on disk.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Gated => "gated",
            Self::Sealed => "sealed",
        }
    }
}

/// The root policy document, as parsed from a `.subctl/policy.toml` file.
///
/// Mirrors `PolicyDocument` in `lib/policy/types.ts`. The optional `__meta`
/// field is populated by the loader after the four-source merge runs; on-disk
/// source files do NOT set it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Policy {
    /// Optional ecosystem preset to inherit from
    /// (`"node" | "python" | "generic" | "rust" | "go"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,

    /// Default mode when `subctl teams <provider>` is invoked without
    /// `--mode`. Defaults to `gated` when absent (per pack 06 §4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_mode: Option<Mode>,

    /// Per-mode configuration tables.
    #[serde(default)]
    pub mode: ModeTables,

    /// Loader-populated metadata. Source files on disk do NOT set this.
    #[serde(default, rename = "__meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<PolicyMeta>,
}

/// The `[mode.*]` tables.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModeTables {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted: Option<TrustedMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gated: Option<GatedMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed: Option<SealedMode>,
}

impl ModeTables {
    /// True iff every per-mode table is empty / unset.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.trusted.is_none() && self.gated.is_none() && self.sealed.is_none()
    }
}

/// Loader-populated metadata (pack 02 §8 / pack 09 §3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyMeta {
    /// Paths of every TOML file that contributed to the resolved policy,
    /// listed in priority order.
    pub source_paths: Vec<String>,
    /// First 8 hex chars of `sha256(canonical(policy))`.
    pub allowlist_sha: String,
    /// ISO 8601 timestamp the policy was resolved.
    pub resolved_at: String,
}

/// Trusted-mode configuration. Intentionally empty.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrustedMode {}

/// Gated-mode configuration — the bulk of the policy surface.
///
/// Resolution order at check time (pack 06 §4):
/// `deny_always` wins over everything → `deny_if_arg_contains` on a matched
/// `allow_pattern` → ecosystem-specific helpers → `allow_pattern` walk →
/// `allow.commands` exact match → default deny.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GatedMode {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<AllowSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_pattern: Option<Vec<AllowPattern>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny_always: Option<DenyAlways>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm: Option<ScriptTable>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pnpm: Option<ScriptTable>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bun: Option<ScriptTable>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yarn: Option<ScriptTable>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub make: Option<MakeTable>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub just: Option<JustTable>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python_modules: Option<PythonModulesTable>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uv: Option<RunTargetsTable>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poetry: Option<RunTargetsTable>,
}

/// `[mode.gated.allow]`. Forward-compat: extra keys are preserved in `extra`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AllowSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commands: Option<Vec<String>>,
    /// Catch-all for fields the typed shape doesn't yet name (sorted for
    /// deterministic serialization). Empty by default.
    #[serde(flatten)]
    pub extra: BTreeMap<String, toml::Value>,
}

/// One row of `allow_pattern` (pack 02 §3.2).
///
/// - `command` — exact match on the first non-flag token.
/// - `args` — first non-flag argument must be in this list. Empty / `None`
///   means "any first non-flag arg is fine".
/// - `deny_if_arg_contains` — substring match against any token after the
///   pattern matches; converts allow to deny.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AllowPattern {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny_if_arg_contains: Option<Vec<String>>,
}

/// `[mode.gated.deny_always]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DenyAlways {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub substrings: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<Vec<String>>,
}

/// `[mode.gated.<runner>]` for `npm`, `pnpm`, `bun`, `yarn`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScriptTable {
    pub allowed_scripts: Vec<String>,
}

/// `[mode.gated.make]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MakeTable {
    pub allowed_targets: Vec<String>,
}

/// `[mode.gated.just]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JustTable {
    pub allowed_recipes: Vec<String>,
}

/// `[mode.gated.python_modules]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PythonModulesTable {
    pub allowed: Vec<String>,
}

/// `[mode.gated.<runner>]` for `uv`, `poetry`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunTargetsTable {
    pub allowed_run_targets: Vec<String>,
}

/// Sealed-mode configuration. Pack 06 §5 — the worker has no `Bash` tool,
/// only the allowlisted MCP tool surface.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SealedMode {
    pub mcp_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation: Option<Escalation>,
}

/// `[mode.sealed.escalation]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Escalation {
    pub target: String, // "master" | "operator"
    pub require_approval: bool,
    pub timeout_seconds: u64,
}

// ---------------------------------------------------------------------------
// Check request / outcome
// ---------------------------------------------------------------------------

/// Input to [`crate::check::check_command`] (pack 06 §3).
///
/// The check is stateless wrt the worker — only the raw command, the
/// worker's cwd (for ecosystem helpers that read `package.json` /
/// `Makefile`), and the team_id (for audit logging) flow in.
#[derive(Debug, Clone)]
pub struct CheckRequest<'a> {
    /// Raw command line as proposed by the agent. NOT shell-expanded.
    pub command: &'a str,
    /// Worker's current working directory.
    pub cwd: &'a std::path::Path,
    /// Team id (audit logging; not used by the check itself).
    pub team_id: &'a str,
    /// Provider session id, if available.
    pub agent_session_id: Option<&'a str>,
}

/// Result of a single policy check (pack 06 §3).
///
/// Each variant carries the same payload — `rule` is human-readable
/// ("`deny_always.substrings: \"rm -rf\"`"), `rule_path` is structured
/// ("`mode.gated.deny_always.substrings`"). Both are pinned by the v3 test
/// vectors.
///
/// `RequireAudit` is reserved for future use — v3 only emits `allow` and
/// `deny`. Binary-wirer needs the variant for the new audit gate path; no
/// current code path produces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    Allow { rule: String, rule_path: String },
    Deny { rule: String, rule_path: String },
    RequireAudit { rule: String, rule_path: String },
}

impl CheckOutcome {
    /// True iff the outcome is `Allow`.
    #[must_use]
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow { .. })
    }

    /// True iff the outcome is `Deny`.
    #[must_use]
    pub fn is_deny(&self) -> bool {
        matches!(self, Self::Deny { .. })
    }

    /// The structured rule path, e.g. `mode.gated.allow_pattern[0]`.
    #[must_use]
    pub fn rule_path(&self) -> &str {
        match self {
            Self::Allow { rule_path, .. }
            | Self::Deny { rule_path, .. }
            | Self::RequireAudit { rule_path, .. } => rule_path,
        }
    }

    /// The human-readable rule that fired.
    #[must_use]
    pub fn rule(&self) -> &str {
        match self {
            Self::Allow { rule, .. }
            | Self::Deny { rule, .. }
            | Self::RequireAudit { rule, .. } => rule,
        }
    }
}

// ---------------------------------------------------------------------------
// Audit entry — JSONL row written by `audit::AuditWriter` (pack 09 §3)
// ---------------------------------------------------------------------------

/// One row of the audit JSONL log.
///
/// Field shape matches v3's `AuditEntry` so existing readers (the dashboard,
/// `subctl audit tail`) parse the Rust port's output without changes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditEntry {
    /// ISO 8601 with millisecond precision, UTC.
    pub ts: String,
    pub team_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowlist_sha: Option<String>,
    /// Untruncated raw command. Empty string on header / verifier rows.
    pub command: String,
    pub decision: AuditDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_path: Option<String>,
    pub event_type: AuditEventType,
}

/// Allow / deny tag carried by `AuditEntry`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditDecision {
    Allow,
    Deny,
}

/// Discriminator for the three audit event kinds (pack 09 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventType {
    Check,
    Header,
    VerifierCorrection,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_roundtrip_through_serde_lowercase() {
        for m in [Mode::Trusted, Mode::Gated, Mode::Sealed] {
            let json = serde_json::to_string(&m).expect("serialize");
            assert!(
                json == "\"trusted\"" || json == "\"gated\"" || json == "\"sealed\"",
                "expected lowercase, got {json}",
            );
            let back: Mode = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(m, back);
        }
    }

    #[test]
    fn mode_converts_to_and_from_core() {
        for m in [Mode::Trusted, Mode::Gated, Mode::Sealed] {
            assert_eq!(Mode::from_core(m.to_core()), m);
        }
    }

    #[test]
    fn policy_deserialises_lowercase_default_mode() {
        let src = r#"
default_mode = "gated"

[mode.gated.allow]
commands = ["ls", "pwd"]
"#;
        let p: Policy = toml::from_str(src).expect("parse");
        assert_eq!(p.default_mode, Some(Mode::Gated));
        let allow = p
            .mode
            .gated
            .as_ref()
            .and_then(|g| g.allow.as_ref())
            .and_then(|a| a.commands.as_ref())
            .expect("commands");
        assert_eq!(allow, &vec!["ls".to_owned(), "pwd".to_owned()]);
    }

    #[test]
    fn audit_entry_jsonl_shape_matches_v3() {
        // Sanity: the JSON shape v3 readers expect uses snake_case keys
        // plus the `event_type` discriminator.
        let entry = AuditEntry {
            ts: "2026-05-11T18:42:13.901Z".into(),
            team_id: "t".into(),
            agent_session_id: Some("sess".into()),
            mode: Mode::Gated,
            allowlist_sha: Some("deadbeef".into()),
            command: "ls".into(),
            decision: AuditDecision::Allow,
            rule: Some("allow.commands: ls".into()),
            rule_path: Some("mode.gated.allow.commands".into()),
            event_type: AuditEventType::Check,
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        assert!(json.contains("\"event_type\":\"check\""), "{json}");
        assert!(json.contains("\"decision\":\"allow\""), "{json}");
        assert!(json.contains("\"mode\":\"gated\""), "{json}");
        let back: AuditEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, entry);
    }

    #[test]
    fn check_outcome_accessors() {
        let o = CheckOutcome::Deny {
            rule: "deny_always.substrings: \"rm -rf\"".into(),
            rule_path: "mode.gated.deny_always.substrings".into(),
        };
        assert!(o.is_deny());
        assert!(!o.is_allow());
        assert_eq!(o.rule_path(), "mode.gated.deny_always.substrings");
        assert!(o.rule().contains("rm -rf"));
    }
}
