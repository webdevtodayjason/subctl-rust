//! TOML loader + four-source merger — the Rust port of
//! `components/evy/tools/policy/load.ts`.
//!
//! Priority chain (pack 02 §1):
//!
//! ```text
//! 1. (highest)  <project_root>/.subctl/policy.toml
//! 2.            <project_root>/.subctl/policy.local.toml
//! 3.            ~/.config/subctl/policy.toml         (SUBCTL_CONFIG_DIR)
//! 4. (lowest)   <install>/config/policy/defaults.toml (SUBCTL_INSTALL_ROOT)
//! ```
//!
//! Plus an optional named preset resolved from
//! `<install>/config/policy/presets/<name>.toml`. If a project sets
//! `preset = "none"`, the preset chain AND the shipped defaults are skipped.
//!
//! Merge semantics (pack 02 §6 + pack 03 §5) are hand-ported from the TS
//! reference because they mix ADDITIVE arrays (allow.commands,
//! allow_pattern, deny_always.*) with REPLACE arrays (every ecosystem
//! table) — no off-the-shelf TOML merger does that.
//!
//! Errors surface as `evy_core::Error::InvalidMandate("policy/load: ...")`
//! pending the addition of a dedicated `Error::Policy` variant in
//! `evy-core` (see report — request filed with team-lead).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use evy_core::{Error, Result};
use sha2::Digest;

use crate::types::{
    AllowSection, DenyAlways, GatedMode, JustTable, MakeTable, ModeTables, Policy, PolicyMeta,
    PythonModulesTable, RunTargetsTable, ScriptTable,
};

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Resolve the subctl install root. Honours `SUBCTL_INSTALL_ROOT` so tests
/// (and shipped-defaults overrides) can point at a fixture tree.
///
/// If neither the env var is set nor the bin's `..` chain yields a directory
/// with `config/policy/defaults.toml`, returns an [`Error::InvalidMandate`].
pub fn resolve_subctl_install() -> Result<PathBuf> {
    if let Ok(override_path) = std::env::var("SUBCTL_INSTALL_ROOT") {
        return Ok(PathBuf::from(override_path));
    }
    // Fall back to walking up from the current exe location until we find a
    // `config/policy/defaults.toml`. Most binaries will set the env var
    // explicitly; this branch is just a developer convenience.
    let exe = std::env::current_exe()
        .map_err(|e| invalid(format!("resolve_subctl_install: current_exe: {e}")))?;
    for candidate in exe.ancestors() {
        if candidate.join("config/policy/defaults.toml").exists() {
            return Ok(candidate.to_path_buf());
        }
    }
    Err(invalid(
        "resolve_subctl_install: no SUBCTL_INSTALL_ROOT and no defaults.toml found in any ancestor of current exe",
    ))
}

fn user_config_path() -> PathBuf {
    if let Ok(d) = std::env::var("SUBCTL_CONFIG_DIR") {
        return PathBuf::from(d).join("policy.toml");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config/subctl/policy.toml");
    }
    PathBuf::from("/etc/subctl/policy.toml")
}

// ---------------------------------------------------------------------------
// TOML I/O
// ---------------------------------------------------------------------------

fn read_toml_or_throw(path: &Path) -> Result<Policy> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        invalid(format!(
            "policy/load: failed to read {}: {e}",
            path.display()
        ))
    })?;
    toml::from_str::<Policy>(&text).map_err(|e| {
        invalid(format!(
            "policy/load: invalid TOML in {}: {e}",
            path.display()
        ))
    })
}

fn read_toml_if_exists(path: &Path) -> Result<Option<Policy>> {
    if !path.exists() {
        return Ok(None);
    }
    read_toml_or_throw(path).map(Some)
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::InvalidMandate(msg.into())
}

// ---------------------------------------------------------------------------
// Single-file load (the spec's required `load_policy`)
// ---------------------------------------------------------------------------

/// Load a single policy TOML file from `path`. Returns `Err` if the file is
/// missing or malformed. For full four-source resolution, use
/// [`load_resolved_policy`].
pub fn load_policy(path: &Path) -> Result<Policy> {
    read_toml_or_throw(path)
}

// ---------------------------------------------------------------------------
// Source-specific loaders (mirror the TS public API)
// ---------------------------------------------------------------------------

/// Read `<project_root>/.subctl/policy.toml` (priority 1) and
/// `policy.local.toml` (priority 2), merge so committed wins, return
/// `None` if neither exists.
pub fn load_project_policy(project_root: &Path) -> Result<Option<Policy>> {
    let committed = project_root.join(".subctl/policy.toml");
    let local = project_root.join(".subctl/policy.local.toml");
    let committed_doc = read_toml_if_exists(&committed)?;
    let local_doc = read_toml_if_exists(&local)?;
    match (committed_doc, local_doc) {
        (None, None) => Ok(None),
        (Some(c), None) => Ok(Some(c)),
        (None, Some(l)) => Ok(Some(l)),
        (Some(c), Some(l)) => Ok(Some(merge_policies(&[c, l]))),
    }
}

/// Read `~/.config/subctl/policy.toml` (priority 3). `None` if missing.
pub fn load_user_policy() -> Result<Option<Policy>> {
    read_toml_if_exists(&user_config_path())
}

/// Read `<install>/config/policy/defaults.toml` (priority 4). Always
/// returns a [`Policy`]; missing or malformed is treated as a packaging
/// bug and surfaces as an error.
pub fn load_shipped_defaults() -> Result<Policy> {
    let root = resolve_subctl_install()?;
    let path = root.join("config/policy/defaults.toml");
    read_toml_or_throw(&path)
}

/// Read a named preset from `<install>/config/policy/presets/<name>.toml`.
/// Errors if `name == "none"` (caller is expected to skip the preset layer
/// upstream) or if the preset file is missing.
pub fn load_preset(name: &str) -> Result<Policy> {
    if name == "none" {
        return Err(invalid(
            "preset \"none\" means no inheritance — call sites should skip",
        ));
    }
    let root = resolve_subctl_install()?;
    let path = root
        .join("config/policy/presets")
        .join(format!("{name}.toml"));
    if !path.exists() {
        return Err(invalid(format!(
            "policy/load: preset \"{name}\" not found at {}",
            path.display(),
        )));
    }
    read_toml_or_throw(&path)
}

// ---------------------------------------------------------------------------
// Merge — pack 02 §6 + pack 03 §5, hand-ported from load.ts::mergePolicies
// ---------------------------------------------------------------------------

/// Merge a list of policy documents, ordered **highest-priority first**.
///
/// - Scalars (preset, default_mode) — the first defined (highest priority)
///   wins.
/// - Additive arrays (allow.commands, allow_pattern, deny_always.*) —
///   concatenated in priority-reverse so lowest-priority entries appear
///   first and the highest-priority entries land at the END.
/// - REPLACE arrays (every ecosystem-specific table) — the highest-priority
///   document that sets the table wins; its inner list replaces, not
///   extends, lower lists.
#[must_use]
pub fn merge_policies(docs: &[Policy]) -> Policy {
    let mut out = Policy::default();

    // Walk lowest-priority first so:
    //   - scalars get overwritten naturally by higher-priority writes
    //   - additive arrays concat in the right order (lower first, higher last)
    //   - REPLACE arrays naturally pick the last (= highest) writer
    for doc in docs.iter().rev() {
        if doc.preset.is_some() {
            out.preset = doc.preset.clone();
        }
        if doc.default_mode.is_some() {
            out.default_mode = doc.default_mode;
        }
        if doc.mode.trusted.is_some() {
            out.mode.trusted = doc.mode.trusted.clone();
        }
        if doc.mode.sealed.is_some() {
            out.mode.sealed = doc.mode.sealed.clone();
        }
        if let Some(g) = doc.mode.gated.as_ref() {
            out.mode.gated = Some(merge_gated_layer(out.mode.gated.as_ref(), g));
        }
    }

    out
}

fn merge_gated_layer(acc: Option<&GatedMode>, next: &GatedMode) -> GatedMode {
    let mut merged = acc.cloned().unwrap_or_default();

    // --- additive: allow.commands -----------------------------------------
    if let Some(next_allow) = next.allow.as_ref() {
        if let Some(next_cmds) = next_allow.commands.as_ref() {
            let mut combined = merged
                .allow
                .as_ref()
                .and_then(|a| a.commands.clone())
                .unwrap_or_default();
            combined.extend(next_cmds.iter().cloned());
            let mut base = merged.allow.clone().unwrap_or_default();
            base.commands = Some(combined);
            merged.allow = Some(base);
        } else {
            // No commands key — preserve other allow.* keys (forward-compat).
            let mut base = merged.allow.clone().unwrap_or_default();
            for (k, v) in &next_allow.extra {
                base.extra.insert(k.clone(), v.clone());
            }
            merged.allow = Some(base);
        }
    }

    // --- additive: allow_pattern -----------------------------------------
    if let Some(next_pat) = next.allow_pattern.as_ref() {
        let mut combined = merged.allow_pattern.clone().unwrap_or_default();
        combined.extend(next_pat.iter().cloned());
        merged.allow_pattern = Some(combined);
    }

    // --- additive: deny_always.substrings + deny_always.regex ------------
    if let Some(next_da) = next.deny_always.as_ref() {
        let mut acc_subs = merged
            .deny_always
            .as_ref()
            .and_then(|d| d.substrings.clone())
            .unwrap_or_default();
        let mut acc_regex = merged
            .deny_always
            .as_ref()
            .and_then(|d| d.regex.clone())
            .unwrap_or_default();

        if let Some(s) = next_da.substrings.as_ref() {
            acc_subs.extend(s.iter().cloned());
        }
        if let Some(r) = next_da.regex.as_ref() {
            acc_regex.extend(r.iter().cloned());
        }

        merged.deny_always = Some(DenyAlways {
            substrings: if acc_subs.is_empty() {
                None
            } else {
                Some(acc_subs)
            },
            regex: if acc_regex.is_empty() {
                None
            } else {
                Some(acc_regex)
            },
        });
    }

    // --- REPLACE: ecosystem-specific tables ------------------------------
    if let Some(t) = next.npm.as_ref() {
        merged.npm = Some(t.clone());
    }
    if let Some(t) = next.pnpm.as_ref() {
        merged.pnpm = Some(t.clone());
    }
    if let Some(t) = next.bun.as_ref() {
        merged.bun = Some(t.clone());
    }
    if let Some(t) = next.yarn.as_ref() {
        merged.yarn = Some(t.clone());
    }
    if let Some(t) = next.make.as_ref() {
        merged.make = Some(t.clone());
    }
    if let Some(t) = next.just.as_ref() {
        merged.just = Some(t.clone());
    }
    if let Some(t) = next.python_modules.as_ref() {
        merged.python_modules = Some(t.clone());
    }
    if let Some(t) = next.uv.as_ref() {
        merged.uv = Some(t.clone());
    }
    if let Some(t) = next.poetry.as_ref() {
        merged.poetry = Some(t.clone());
    }

    let _ = (
        ScriptTable::default(),
        MakeTable::default(),
        JustTable::default(),
        PythonModulesTable::default(),
        RunTargetsTable::default(),
        AllowSection::default(),
        ModeTables::default(),
    ); // suppress unused-warning for struct-name re-exports in some build modes

    merged
}

// ---------------------------------------------------------------------------
// Resolution (the public entry point)
// ---------------------------------------------------------------------------

/// Walk the four-source chain + preset, merge per the contract, attach
/// `__meta` (allowlist sha + source paths + resolvedAt), return the final
/// resolved document.
pub fn load_resolved_policy(project_root: &Path) -> Result<Policy> {
    let project_doc = load_project_policy(project_root)?;
    let user_doc = load_user_policy()?;
    let defaults_doc = load_shipped_defaults()?;

    let preset_name = project_doc
        .as_ref()
        .and_then(|d| d.preset.clone())
        .or_else(|| user_doc.as_ref().and_then(|d| d.preset.clone()))
        .or_else(|| defaults_doc.preset.clone());

    let skip_baselines = preset_name.as_deref() == Some("none");

    let mut layers: Vec<Policy> = Vec::new();
    if let Some(d) = project_doc.as_ref() {
        layers.push(d.clone());
    }
    if let Some(d) = user_doc.as_ref() {
        layers.push(d.clone());
    }
    if !skip_baselines {
        if let Some(name) = preset_name.as_deref() {
            if name != "none" {
                layers.push(load_preset(name)?);
            }
        }
        layers.push(defaults_doc.clone());
    }

    let mut merged = merge_policies(&layers);

    // Strip the sentinel "none" so the resolved doc doesn't leak it.
    if merged.preset.as_deref() == Some("none") {
        merged.preset = None;
    }

    let source_paths = collect_source_paths(
        project_root,
        project_doc.as_ref(),
        user_doc.as_ref(),
        preset_name.as_deref(),
        skip_baselines,
    )?;
    let allowlist_sha = compute_allowlist_sha(&merged);
    let resolved_at = iso_now_ms();

    merged.meta = Some(PolicyMeta {
        source_paths,
        allowlist_sha,
        resolved_at,
    });
    Ok(merged)
}

fn collect_source_paths(
    project_root: &Path,
    _project_doc: Option<&Policy>,
    user_doc: Option<&Policy>,
    preset_name: Option<&str>,
    skip_baselines: bool,
) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    let committed = project_root.join(".subctl/policy.toml");
    let local = project_root.join(".subctl/policy.local.toml");
    if committed.exists() {
        paths.push(committed.to_string_lossy().into_owned());
    }
    if local.exists() {
        paths.push(local.to_string_lossy().into_owned());
    }
    if user_doc.is_some() {
        paths.push(user_config_path().to_string_lossy().into_owned());
    }
    if !skip_baselines {
        let root = resolve_subctl_install()?;
        if let Some(name) = preset_name {
            if name != "none" {
                paths.push(
                    root.join(format!("config/policy/presets/{name}.toml"))
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        paths.push(
            root.join("config/policy/defaults.toml")
                .to_string_lossy()
                .into_owned(),
        );
    }
    Ok(paths)
}

// ---------------------------------------------------------------------------
// Allowlist SHA (pack 09 §3 / pack 02 §8)
// ---------------------------------------------------------------------------

/// Compute a stable short hash of the resolved policy doc.
///
/// Algorithm: canonicalize (sorted keys, recursive; arrays NOT sorted) →
/// JSON-stringify → sha256 → first 8 hex chars. `__meta` is excluded so the
/// same policy resolved at two different times produces the same sha.
#[must_use]
pub fn compute_allowlist_sha(policy: &Policy) -> String {
    let value = serde_json::to_value(policy).expect("policy is always JSON-able");
    let canonical = canonicalize(&value);
    let bytes = serde_json::to_vec(&canonical).expect("canonical value is JSON-able");
    let digest = sha2::Sha256::digest(&bytes);
    let full = hex::encode(digest);
    full[..8].to_owned()
}

fn canonicalize(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::{Map, Value};
    match value {
        Value::Array(arr) => Value::Array(arr.iter().map(canonicalize).collect()),
        Value::Object(obj) => {
            let mut keys: Vec<&String> = obj.keys().collect();
            keys.sort();
            let mut out = Map::with_capacity(keys.len());
            for k in keys {
                if k == "__meta" {
                    continue;
                }
                out.insert(k.clone(), canonicalize(&obj[k]));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Timestamps
// ---------------------------------------------------------------------------

/// ISO 8601 with millisecond precision, UTC, ending in `Z`. Matches v3's
/// `new Date().toISOString()` output exactly.
#[must_use]
pub fn iso_now_ms() -> String {
    use chrono::SecondsFormat;
    chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

// Suppress "unused" for forward-compat helpers that other modules may pull in
// via re-exports.
#[allow(dead_code)]
fn _typecheck() -> Option<HashMap<String, String>> {
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testlock::ENV_LOCK;
    use crate::types::Mode;

    struct EnvGuard {
        keys: Vec<(String, Option<String>)>,
    }

    impl EnvGuard {
        fn new(keys: &[&str]) -> Self {
            let snapshot = keys
                .iter()
                .map(|k| (k.to_string(), std::env::var(k).ok()))
                .collect();
            Self { keys: snapshot }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.keys {
                match v {
                    Some(val) => unsafe { std::env::set_var(k, val) },
                    None => unsafe { std::env::remove_var(k) },
                }
            }
        }
    }

    fn install_root_fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/install")
    }

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    fn temp_dir(prefix: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .expect("tempdir")
    }

    fn write_file(root: &Path, rel: &str, body: &str) {
        let full = root.join(rel);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, body).unwrap();
    }

    // -- loadProjectPolicy --------------------------------------------------

    #[test]
    fn project_policy_missing_returns_none() {
        let _lock = ENV_LOCK.lock().unwrap();
        let p = temp_dir("loadProject");
        let doc = load_project_policy(p.path()).unwrap();
        assert!(doc.is_none());
    }

    #[test]
    fn project_policy_reads_committed_only() {
        let _lock = ENV_LOCK.lock().unwrap();
        let p = temp_dir("loadProject2");
        write_file(p.path(), ".subctl/policy.toml", "preset = \"node\"\n");
        let doc = load_project_policy(p.path()).unwrap().expect("doc");
        assert_eq!(doc.preset.as_deref(), Some("node"));
    }

    #[test]
    fn project_policy_committed_wins_over_local() {
        let _lock = ENV_LOCK.lock().unwrap();
        let p = temp_dir("loadProject3");
        write_file(
            p.path(),
            ".subctl/policy.toml",
            "default_mode = \"gated\"\n",
        );
        write_file(
            p.path(),
            ".subctl/policy.local.toml",
            "default_mode = \"trusted\"\n",
        );
        let doc = load_project_policy(p.path()).unwrap().expect("doc");
        assert_eq!(doc.default_mode, Some(Mode::Gated));
    }

    // -- loadUserPolicy -----------------------------------------------------

    #[test]
    fn user_policy_missing_returns_none() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&["SUBCTL_CONFIG_DIR"]);
        let p = temp_dir("userCfg");
        unsafe {
            std::env::set_var("SUBCTL_CONFIG_DIR", p.path());
        }
        let doc = load_user_policy().unwrap();
        assert!(doc.is_none());
    }

    #[test]
    fn user_policy_reads_file_when_present() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&["SUBCTL_CONFIG_DIR"]);
        let p = temp_dir("userCfg2");
        unsafe {
            std::env::set_var("SUBCTL_CONFIG_DIR", p.path());
        }
        let body =
            std::fs::read_to_string(fixtures_dir().join("user-config-example.toml")).unwrap();
        std::fs::write(p.path().join("policy.toml"), body).unwrap();
        let doc = load_user_policy().unwrap().expect("doc");
        assert_eq!(doc.default_mode, Some(Mode::Trusted));
    }

    // -- loadShippedDefaults + loadPreset -----------------------------------

    #[test]
    fn shipped_defaults_smoke() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&["SUBCTL_INSTALL_ROOT"]);
        unsafe {
            std::env::set_var("SUBCTL_INSTALL_ROOT", install_root_fixture());
        }
        let doc = load_shipped_defaults().unwrap();
        assert_eq!(doc.default_mode, Some(Mode::Gated));
        let regex = doc
            .mode
            .gated
            .as_ref()
            .and_then(|g| g.deny_always.as_ref())
            .and_then(|d| d.regex.as_ref())
            .expect("regex");
        assert!(!regex.is_empty());
    }

    #[test]
    fn load_preset_node_shape() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&["SUBCTL_INSTALL_ROOT"]);
        unsafe {
            std::env::set_var("SUBCTL_INSTALL_ROOT", install_root_fixture());
        }
        let doc = load_preset("node").unwrap();
        let g = doc.mode.gated.as_ref().expect("gated");
        let pats = g.allow_pattern.as_ref().expect("allow_pattern");
        let npm = pats
            .iter()
            .find(|p| p.command == "npm")
            .expect("npm pattern");
        assert!(npm.args.as_ref().unwrap().iter().any(|a| a == "install"));
        assert!(g
            .npm
            .as_ref()
            .unwrap()
            .allowed_scripts
            .iter()
            .any(|s| s == "test"));
    }

    #[test]
    fn load_preset_none_errors() {
        let _lock = ENV_LOCK.lock().unwrap();
        let err = load_preset("none").unwrap_err();
        assert!(err.to_string().contains("none"), "{err}");
    }

    #[test]
    fn load_preset_unknown_errors() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&["SUBCTL_INSTALL_ROOT"]);
        unsafe {
            std::env::set_var("SUBCTL_INSTALL_ROOT", install_root_fixture());
        }
        let err = load_preset("not-a-real-preset").unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    // -- malformed input ----------------------------------------------------

    #[test]
    fn project_policy_malformed_toml_surfaces_descriptive_error() {
        let _lock = ENV_LOCK.lock().unwrap();
        let p = temp_dir("malformed");
        write_file(
            p.path(),
            ".subctl/policy.toml",
            "this is = not = valid toml\n",
        );
        let err = load_project_policy(p.path()).unwrap_err();
        assert!(err.to_string().contains("invalid TOML"), "{err}");
    }

    // -- resolve_subctl_install --------------------------------------------

    #[test]
    fn resolve_install_respects_env_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&["SUBCTL_INSTALL_ROOT"]);
        unsafe {
            std::env::set_var("SUBCTL_INSTALL_ROOT", "/some/fake/path");
        }
        let root = resolve_subctl_install().unwrap();
        assert_eq!(root, PathBuf::from("/some/fake/path"));
    }

    // -- loadResolvedPolicy end-to-end -------------------------------------

    #[test]
    fn resolved_empty_project_falls_through_to_defaults() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&["SUBCTL_CONFIG_DIR", "SUBCTL_INSTALL_ROOT"]);
        let p = temp_dir("resolved");
        let cfg = temp_dir("resolvedCfg");
        unsafe {
            std::env::set_var("SUBCTL_CONFIG_DIR", cfg.path());
            std::env::set_var("SUBCTL_INSTALL_ROOT", install_root_fixture());
        }
        let body = std::fs::read_to_string(fixtures_dir().join("empty.toml")).unwrap();
        write_file(p.path(), ".subctl/policy.toml", &body);
        let resolved = load_resolved_policy(p.path()).unwrap();
        assert_eq!(resolved.default_mode, Some(Mode::Gated));
        let meta = resolved.meta.as_ref().expect("meta");
        assert_eq!(meta.allowlist_sha.len(), 8);
        assert!(!meta.source_paths.is_empty());
        let regex = resolved
            .mode
            .gated
            .as_ref()
            .and_then(|g| g.deny_always.as_ref())
            .and_then(|d| d.regex.as_ref())
            .expect("regex");
        assert!(regex.iter().any(|r| r == r"\bnode\s+-e\b"));
    }

    #[test]
    fn resolved_project_with_extra_allow_appends_last() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&["SUBCTL_CONFIG_DIR", "SUBCTL_INSTALL_ROOT"]);
        let p = temp_dir("resolved2");
        let cfg = temp_dir("resolved2Cfg");
        unsafe {
            std::env::set_var("SUBCTL_CONFIG_DIR", cfg.path());
            std::env::set_var("SUBCTL_INSTALL_ROOT", install_root_fixture());
        }
        let body =
            std::fs::read_to_string(fixtures_dir().join("project-with-extra-allow.toml")).unwrap();
        write_file(p.path(), ".subctl/policy.toml", &body);
        let resolved = load_resolved_policy(p.path()).unwrap();
        let patterns = resolved
            .mode
            .gated
            .as_ref()
            .and_then(|g| g.allow_pattern.as_ref())
            .expect("patterns");
        assert!(patterns.len() > 5);
        let last = patterns.last().unwrap();
        assert_eq!(last.command, "gh");
        assert_eq!(last.args.as_deref().unwrap(), &["issue".to_string()]);

        let meta = resolved.meta.as_ref().unwrap();
        assert!(meta
            .source_paths
            .iter()
            .any(|p| p.ends_with(".subctl/policy.toml")));
        assert!(meta
            .source_paths
            .iter()
            .any(|p| p.ends_with("presets/node.toml")));
        assert!(meta
            .source_paths
            .iter()
            .any(|p| p.ends_with("defaults.toml")));
    }

    #[test]
    fn resolved_preset_none_skips_baseline() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&["SUBCTL_CONFIG_DIR", "SUBCTL_INSTALL_ROOT"]);
        let p = temp_dir("resolved3");
        let cfg = temp_dir("resolved3Cfg");
        unsafe {
            std::env::set_var("SUBCTL_CONFIG_DIR", cfg.path());
            std::env::set_var("SUBCTL_INSTALL_ROOT", install_root_fixture());
        }
        let body =
            std::fs::read_to_string(fixtures_dir().join("project-preset-none.toml")).unwrap();
        write_file(p.path(), ".subctl/policy.toml", &body);
        let resolved = load_resolved_policy(p.path()).unwrap();

        let gated = resolved.mode.gated.as_ref().expect("gated");
        let patterns = gated.allow_pattern.as_ref().expect("patterns");
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].command, "git");
        assert_eq!(
            patterns[0].args.as_deref().unwrap(),
            &["status".to_string()]
        );

        let regex = gated.deny_always.as_ref().and_then(|d| d.regex.as_ref());
        assert!(regex.is_none() || regex.unwrap().is_empty());

        let subs = gated
            .deny_always
            .as_ref()
            .and_then(|d| d.substrings.as_ref());
        assert_eq!(subs.unwrap(), &vec!["sudo ".to_string()]);

        // Sentinel "none" stripped.
        assert!(resolved.preset.is_none());

        // sourcePaths references only the project file.
        let meta = resolved.meta.as_ref().unwrap();
        assert_eq!(meta.source_paths.len(), 1);
        assert!(meta.source_paths[0].ends_with("policy.toml"));
    }

    #[test]
    fn resolved_meta_resolved_at_is_iso_with_millis() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&["SUBCTL_CONFIG_DIR", "SUBCTL_INSTALL_ROOT"]);
        let p = temp_dir("resolved4");
        let cfg = temp_dir("resolved4Cfg");
        unsafe {
            std::env::set_var("SUBCTL_CONFIG_DIR", cfg.path());
            std::env::set_var("SUBCTL_INSTALL_ROOT", install_root_fixture());
        }
        write_file(p.path(), ".subctl/policy.toml", "preset = \"node\"\n");
        let resolved = load_resolved_policy(p.path()).unwrap();
        let ts = resolved.meta.as_ref().unwrap().resolved_at.clone();
        let re = regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$").unwrap();
        assert!(re.is_match(&ts), "got {ts}");
    }

    #[test]
    fn compute_allowlist_sha_deterministic_excluding_meta() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&["SUBCTL_CONFIG_DIR", "SUBCTL_INSTALL_ROOT"]);
        let p = temp_dir("resolved5");
        let cfg = temp_dir("resolved5Cfg");
        unsafe {
            std::env::set_var("SUBCTL_CONFIG_DIR", cfg.path());
            std::env::set_var("SUBCTL_INSTALL_ROOT", install_root_fixture());
        }
        write_file(p.path(), ".subctl/policy.toml", "preset = \"node\"\n");
        let a = load_resolved_policy(p.path()).unwrap();
        let b = load_resolved_policy(p.path()).unwrap();
        let sha_a = a.meta.as_ref().unwrap().allowlist_sha.clone();
        let sha_b = b.meta.as_ref().unwrap().allowlist_sha.clone();
        assert_eq!(sha_a, sha_b);
        // Recompute manually without meta — must agree.
        let mut stripped = a.clone();
        stripped.meta = None;
        assert_eq!(compute_allowlist_sha(&stripped), sha_a);
    }

    // -- mergePolicies pure tests (ports of merge.test.ts) -----------------

    fn parse(toml_src: &str) -> Policy {
        toml::from_str(toml_src).unwrap()
    }

    #[test]
    fn merge_pure_empty_project_plus_preset() {
        let project = parse("preset = \"node\"\n");
        let preset = parse(
            r#"[mode.gated.allow]
commands = ["git", "ls"]

[[mode.gated.allow_pattern]]
command = "npm"
args = ["install"]

[mode.gated.npm]
allowed_scripts = ["test", "build"]
"#,
        );
        let merged = merge_policies(&[project, preset]);
        assert_eq!(merged.preset.as_deref(), Some("node"));
        let g = merged.mode.gated.unwrap();
        assert_eq!(
            g.allow.unwrap().commands.unwrap(),
            vec!["git".to_string(), "ls".to_string()],
        );
        assert_eq!(g.allow_pattern.as_ref().unwrap().len(), 1);
        assert_eq!(
            g.npm.unwrap().allowed_scripts,
            vec!["test".to_string(), "build".to_string()],
        );
    }

    #[test]
    fn merge_pure_allow_pattern_appended_project_last() {
        let project = parse(
            r#"[[mode.gated.allow_pattern]]
command = "gh"
args = ["issue"]
"#,
        );
        let preset = parse(
            r#"[[mode.gated.allow_pattern]]
command = "npm"
args = ["install"]

[[mode.gated.allow_pattern]]
command = "git"
args = ["status"]
"#,
        );
        let merged = merge_policies(&[project, preset]);
        let patterns = merged.mode.gated.unwrap().allow_pattern.unwrap();
        let commands: Vec<&str> = patterns.iter().map(|p| p.command.as_str()).collect();
        assert_eq!(commands, vec!["npm", "git", "gh"]);
    }

    #[test]
    fn merge_pure_npm_allowed_scripts_replace() {
        let project = parse(
            r#"[mode.gated.npm]
allowed_scripts = ["deploy:staging", "migrate:up"]
"#,
        );
        let preset = parse(
            r#"[mode.gated.npm]
allowed_scripts = ["test", "build", "lint", "format", "dev"]
"#,
        );
        let merged = merge_policies(&[project, preset]);
        assert_eq!(
            merged
                .mode
                .gated
                .as_ref()
                .unwrap()
                .npm
                .as_ref()
                .unwrap()
                .allowed_scripts,
            vec!["deploy:staging".to_string(), "migrate:up".to_string()],
        );
    }

    #[test]
    fn merge_pure_default_mode_priority1_wins() {
        let project = parse("default_mode = \"gated\"\n");
        let user = parse("default_mode = \"trusted\"\n");
        let defaults = parse("default_mode = \"gated\"\n");
        let merged = merge_policies(&[project, user, defaults]);
        assert_eq!(merged.default_mode, Some(Mode::Gated));
    }

    #[test]
    fn merge_pure_deny_always_substrings_accumulate_low_first_high_last() {
        let project = parse(
            r#"[mode.gated.deny_always]
substrings = ["aws "]
"#,
        );
        let user = parse(
            r#"[mode.gated.deny_always]
substrings = ["kubectl delete "]
"#,
        );
        let preset = parse(
            r#"[mode.gated.deny_always]
substrings = ["rm -rf"]
"#,
        );
        let defaults = parse(
            r#"[mode.gated.deny_always]
substrings = [":(){:|:&};:"]
"#,
        );
        let merged = merge_policies(&[project, user, preset, defaults]);
        let subs = merged
            .mode
            .gated
            .as_ref()
            .unwrap()
            .deny_always
            .as_ref()
            .unwrap()
            .substrings
            .as_ref()
            .unwrap();
        assert_eq!(
            subs.as_slice(),
            &[
                ":(){:|:&};:".to_string(),
                "rm -rf".to_string(),
                "kubectl delete ".to_string(),
                "aws ".to_string(),
            ],
        );
    }

    #[test]
    fn merge_pure_replace_per_ecosystem_table() {
        let project = parse(
            r#"[mode.gated.pnpm]
allowed_scripts = ["custom"]
"#,
        );
        let preset = parse(
            r#"[mode.gated.npm]
allowed_scripts = ["test"]

[mode.gated.pnpm]
allowed_scripts = ["test", "build"]

[mode.gated.bun]
allowed_scripts = ["test"]

[mode.gated.make]
allowed_targets = ["test"]

[mode.gated.just]
allowed_recipes = ["test"]

[mode.gated.python_modules]
allowed = ["pytest"]

[mode.gated.uv]
allowed_run_targets = ["pytest"]

[mode.gated.poetry]
allowed_run_targets = ["pytest"]
"#,
        );
        let merged = merge_policies(&[project, preset]);
        let g = merged.mode.gated.unwrap();
        assert_eq!(g.pnpm.unwrap().allowed_scripts, vec!["custom".to_string()],);
        assert_eq!(g.npm.unwrap().allowed_scripts, vec!["test".to_string()]);
        assert_eq!(g.bun.unwrap().allowed_scripts, vec!["test".to_string()]);
        assert_eq!(g.make.unwrap().allowed_targets, vec!["test".to_string()]);
        assert_eq!(g.just.unwrap().allowed_recipes, vec!["test".to_string()]);
        assert_eq!(
            g.python_modules.unwrap().allowed,
            vec!["pytest".to_string()]
        );
        assert_eq!(
            g.uv.unwrap().allowed_run_targets,
            vec!["pytest".to_string()]
        );
        assert_eq!(
            g.poetry.unwrap().allowed_run_targets,
            vec!["pytest".to_string()]
        );
    }

    #[test]
    fn merge_pure_allow_commands_additive_order_preserved() {
        let project = parse(
            r#"[mode.gated.allow]
commands = ["tree"]
"#,
        );
        let preset = parse(
            r#"[mode.gated.allow]
commands = ["pwd", "ls"]
"#,
        );
        let merged = merge_policies(&[project, preset]);
        assert_eq!(
            merged.mode.gated.unwrap().allow.unwrap().commands.unwrap(),
            vec!["pwd".to_string(), "ls".to_string(), "tree".to_string()],
        );
    }

    #[test]
    fn merge_pure_sealed_replace_highest_priority_wins() {
        let project = parse(
            r#"[mode.sealed]
mcp_tools = ["fs_read"]
test_command = "npm test"
"#,
        );
        let preset = parse(
            r#"[mode.sealed]
mcp_tools = ["whatever"]
test_command = "pytest"
"#,
        );
        let merged = merge_policies(&[project, preset]);
        let sealed = merged.mode.sealed.unwrap();
        assert_eq!(sealed.mcp_tools, vec!["fs_read".to_string()]);
        assert_eq!(sealed.test_command.as_deref(), Some("npm test"));
    }

    // -- merge end-to-end via fixtures -------------------------------------

    #[test]
    fn merge_e2e_preset_none() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&["SUBCTL_CONFIG_DIR", "SUBCTL_INSTALL_ROOT"]);
        let p = temp_dir("noneFixture");
        let cfg = temp_dir("noneFixtureCfg");
        unsafe {
            std::env::set_var("SUBCTL_CONFIG_DIR", cfg.path());
            std::env::set_var("SUBCTL_INSTALL_ROOT", install_root_fixture());
        }
        let body =
            std::fs::read_to_string(fixtures_dir().join("project-preset-none.toml")).unwrap();
        write_file(p.path(), ".subctl/policy.toml", &body);
        let resolved = load_resolved_policy(p.path()).unwrap();
        let g = resolved.mode.gated.unwrap();
        assert_eq!(
            g.allow.unwrap().commands.unwrap(),
            vec!["pwd".to_string(), "echo".to_string()],
        );
        let patterns = g.allow_pattern.unwrap();
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].command, "git");
        let subs = g.deny_always.unwrap().substrings.unwrap();
        assert_eq!(subs, vec!["sudo ".to_string()]);
        assert!(g.npm.is_none());
        assert!(g.pnpm.is_none());
    }

    #[test]
    fn merge_e2e_replace_on_real_node_preset() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&["SUBCTL_CONFIG_DIR", "SUBCTL_INSTALL_ROOT"]);
        let p = temp_dir("realNode");
        let cfg = temp_dir("realNodeCfg");
        unsafe {
            std::env::set_var("SUBCTL_CONFIG_DIR", cfg.path());
            std::env::set_var("SUBCTL_INSTALL_ROOT", install_root_fixture());
        }
        let body =
            std::fs::read_to_string(fixtures_dir().join("project-overrides-npm-scripts.toml"))
                .unwrap();
        write_file(p.path(), ".subctl/policy.toml", &body);
        let resolved = load_resolved_policy(p.path()).unwrap();
        let g = resolved.mode.gated.unwrap();
        assert_eq!(
            g.npm.unwrap().allowed_scripts,
            vec!["deploy:staging".to_string(), "migrate:up".to_string()],
        );
        let patterns = g.allow_pattern.unwrap();
        assert!(patterns.iter().any(|p| p.command == "npm"));
        assert!(patterns.iter().any(|p| p.command == "git"));
    }

    // -- single-file load --------------------------------------------------

    #[test]
    fn load_policy_single_file() {
        let _lock = ENV_LOCK.lock().unwrap();
        let p = fixtures_dir().join("user-config-example.toml");
        let doc = load_policy(&p).unwrap();
        assert_eq!(doc.default_mode, Some(Mode::Trusted));
    }
}
