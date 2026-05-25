//! Per-team policy snapshot writer + reader. Rust port of
//! `components/evy/tools/policy/snapshot.ts` (pack 02 §8 + HANDOFF_DIGEST D7).
//!
//! At spawn time the daemon resolves the project's policy chain, freezes
//! it to TOML, and writes:
//!
//! ```text
//! ~/.local/state/subctl/teams/<team_id>/policy.snapshot.toml
//! ```
//!
//! (honours `SUBCTL_STATE_DIR`). The file is two layers:
//!
//! 1. A header comment block carrying out-of-band metadata that wouldn't
//!    round-trip through TOML (team_id, project_root, spawned_at, mode,
//!    source_paths, allowlist_sha, port_origin).
//! 2. The serialized resolved policy body, with `__meta` stripped — the
//!    header captures everything `__meta` carried + more.
//!
//! `port_origin = "rust"` is appended to the header so bash-gate readers
//! can tell which writer produced this file. The v3 TS writer does not
//! emit this line; presence is the differentiator.
//!
//! NOTE on snapshot format: the team-lead's spec mentions JSON / jq, but
//! the v3 source ships TOML. We follow the source — the bash policy gate
//! at `providers/claude/policy.sh` reads `policy.snapshot.toml` and grep/
//! parses the `# allowlist_sha = "…"` header line directly, not via jq.

use std::path::{Path, PathBuf};

use evy_core::{Error, Result};
use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::load::{compute_allowlist_sha, iso_now_ms, load_resolved_policy};
use crate::types::{Mode, Policy};

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

fn resolve_state_dir() -> PathBuf {
    if let Ok(d) = std::env::var("SUBCTL_STATE_DIR") {
        return PathBuf::from(d);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/subctl");
    }
    PathBuf::from("/var/lib/subctl")
}

/// Deterministic path to a team's snapshot file. Constructable without I/O.
#[must_use]
pub fn get_snapshot_path(team_id: &str) -> PathBuf {
    resolve_state_dir()
        .join("teams")
        .join(team_id)
        .join("policy.snapshot.toml")
}

// ---------------------------------------------------------------------------
// Public metadata type
// ---------------------------------------------------------------------------

/// Snapshot header metadata. Mirrors v3's `SnapshotMetadata`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicySnapshot {
    pub team_id: String,
    pub project_root: String,
    pub mode: Mode,
    /// ISO 8601 UTC with millisecond precision.
    pub spawned_at: String,
    /// Source paths that contributed to the resolved policy.
    pub source_paths: Vec<String>,
    /// First 8 hex chars of sha256(canonical(policy)).
    pub allowlist_sha: String,
    /// Absolute path the snapshot was written to.
    pub snapshot_path: PathBuf,
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Resolve + freeze policy for a team. Steps mirror v3's
/// `writePolicySnapshot`:
///
/// 1. Resolve policy via [`load_resolved_policy`].
/// 2. Override `default_mode` with the spawn-time `mode`.
/// 3. Recompute allowlist_sha against the override.
/// 4. Serialize policy to TOML (drops `__meta`).
/// 5. Prepend the header comment block.
/// 6. If a prior snapshot exists, rename it to `.snapshot.toml.old`.
/// 7. Write the new file at mode 0644.
pub async fn write_snapshot(
    team_id: &str,
    project_root: &Path,
    mode: evy_core::PolicyMode,
) -> Result<PolicySnapshot> {
    write_snapshot_inner(team_id, project_root, Mode::from_core(mode)).await
}

/// Same as [`write_snapshot`] but takes the local lowercase [`Mode`] enum
/// (the on-disk shape). Internal callers and ports use this.
pub async fn write_snapshot_with_local_mode(
    team_id: &str,
    project_root: &Path,
    mode: Mode,
) -> Result<PolicySnapshot> {
    write_snapshot_inner(team_id, project_root, mode).await
}

async fn write_snapshot_inner(
    team_id: &str,
    project_root: &Path,
    mode: Mode,
) -> Result<PolicySnapshot> {
    if team_id.is_empty() {
        return Err(Error::InvalidMandate(
            "write_snapshot: team_id is required".into(),
        ));
    }
    if project_root.as_os_str().is_empty() {
        return Err(Error::InvalidMandate(
            "write_snapshot: project_root is required".into(),
        ));
    }

    // Resolve policy in a blocking task — load_resolved_policy is sync I/O.
    let project_root_owned = project_root.to_path_buf();
    let resolved = tokio::task::spawn_blocking(move || load_resolved_policy(&project_root_owned))
        .await
        .map_err(|e| Error::InvalidMandate(format!("write_snapshot: join error: {e}")))??;

    // Apply spawn-time mode override.
    let mut overridden = resolved.clone();
    overridden.default_mode = Some(mode);

    let allowlist_sha = compute_allowlist_sha(&overridden);
    let source_paths = resolved
        .meta
        .as_ref()
        .map(|m| m.source_paths.clone())
        .unwrap_or_default();
    let spawned_at = iso_now_ms();
    let snapshot_path = get_snapshot_path(team_id);

    // Strip __meta + empty option tables before serializing.
    let body_doc = strip_for_toml(&overridden);
    let body = toml::to_string(&body_doc)
        .map_err(|e| Error::InvalidMandate(format!("write_snapshot: serialize: {e}")))?;
    let header = build_header(
        team_id,
        project_root,
        &spawned_at,
        mode,
        &source_paths,
        &allowlist_sha,
    );
    let file_contents = format!("{header}\n{body}");

    // Ensure parent dir exists.
    if let Some(parent) = snapshot_path.parent() {
        fs::create_dir_all(parent).await?;
    }

    // Rotate prior snapshot to `.snapshot.toml.old` if present.
    if fs::try_exists(&snapshot_path).await.unwrap_or(false) {
        let mut old = snapshot_path.clone();
        let new_ext = match old.extension() {
            Some(_) => format!("{}.old", old.file_name().unwrap().to_string_lossy()),
            None => "policy.snapshot.toml.old".to_string(),
        };
        old.set_file_name(new_ext);
        fs::rename(&snapshot_path, &old).await?;
    }

    fs::write(&snapshot_path, file_contents).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o644);
        std::fs::set_permissions(&snapshot_path, perms)?;
    }

    Ok(PolicySnapshot {
        team_id: team_id.to_owned(),
        project_root: project_root.to_string_lossy().into_owned(),
        mode,
        spawned_at,
        source_paths,
        allowlist_sha,
        snapshot_path,
    })
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// Parse a snapshot file (both header + body). Returns `Ok(None)` if no
/// snapshot exists for the team; returns `Err` if the file exists but is
/// malformed.
pub async fn read_snapshot(team_id: &str) -> Result<Option<(Policy, PolicySnapshot)>> {
    let path = get_snapshot_path(team_id);
    if !fs::try_exists(&path).await.unwrap_or(false) {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).await?;
    let (header_lines, body_text) = split_header(&text);
    let header = parse_header(&header_lines, &path)?;

    let policy: Policy = toml::from_str(&body_text).map_err(|e| {
        Error::InvalidMandate(format!(
            "read_snapshot: failed to parse body of {}: {e}",
            path.display(),
        ))
    })?;

    let meta = PolicySnapshot {
        snapshot_path: path,
        ..header
    };
    Ok(Some((policy, meta)))
}

// ---------------------------------------------------------------------------
// Internals — TOML stripping + header build/parse
// ---------------------------------------------------------------------------

fn strip_for_toml(policy: &Policy) -> serde_json::Value {
    // Easier to clean as JSON first then TOML-serialize via a fresh
    // toml::Value tree — keeps the strip rules in one place.
    let mut value = serde_json::to_value(policy).expect("policy is always JSON-able");
    clean(&mut value);
    value
}

fn clean(value: &mut serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            map.remove("__meta");
            map.retain(|_, v| !v.is_null());
            for (_, v) in map.iter_mut() {
                clean(v);
            }
            // Drop empty inner objects so we don't emit `[empty.table]` headers.
            let empties: Vec<String> = map
                .iter()
                .filter_map(|(k, v)| match v {
                    Value::Object(m) if m.is_empty() => Some(k.clone()),
                    _ => None,
                })
                .collect();
            for k in empties {
                map.remove(&k);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                clean(v);
            }
        }
        _ => {}
    }
}

fn build_header(
    team_id: &str,
    project_root: &Path,
    spawned_at: &str,
    mode: Mode,
    source_paths: &[String],
    allowlist_sha: &str,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push("# subctl policy snapshot".to_string());
    lines.push(format!("# team_id = {}", json_quote(team_id)));
    lines.push(format!(
        "# project_root = {}",
        json_quote(&project_root.to_string_lossy())
    ));
    lines.push(format!("# spawned_at = {}", json_quote(spawned_at)));
    lines.push(format!("# mode = {}", json_quote(mode.as_str())));
    if source_paths.is_empty() {
        lines.push("# source_paths = []".to_string());
    } else {
        lines.push("# source_paths = [".to_string());
        for p in source_paths {
            lines.push(format!("#   {},", json_quote(p)));
        }
        lines.push("# ]".to_string());
    }
    lines.push(format!("# allowlist_sha = {}", json_quote(allowlist_sha)));
    lines.push("# port_origin = \"rust\"".to_string());
    lines.join("\n")
}

fn json_quote(s: &str) -> String {
    // Use serde_json so escaping (quotes, backslashes, control chars) matches
    // v3's JSON.stringify exactly.
    serde_json::to_string(s).expect("string is always JSON-able")
}

fn split_header(text: &str) -> (Vec<String>, String) {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut header_lines: Vec<String> = Vec::new();
    let mut body_start = 0;
    for (i, ln) in lines.iter().enumerate() {
        let trimmed = ln.trim();
        if trimmed.starts_with('#') {
            header_lines.push((*ln).to_string());
            body_start = i + 1;
        } else if trimmed.is_empty() && !header_lines.is_empty() && header_lines.len() < 50 {
            // Allow one blank separator between header and body.
            body_start = i + 1;
        } else {
            break;
        }
    }
    let body = lines[body_start..].join("\n");
    (header_lines, body)
}

fn parse_header(header_lines: &[String], path: &Path) -> Result<PolicySnapshot> {
    // Strip leading `# ` from each line, drop the banner, then parse as TOML.
    let mut stripped = String::new();
    for line in header_lines {
        // remove one leading "#" + optional space
        let content = if let Some(rest) = line.trim_start().strip_prefix('#') {
            rest.strip_prefix(' ').unwrap_or(rest)
        } else {
            continue;
        };
        if content.trim() == "subctl policy snapshot" {
            continue;
        }
        stripped.push_str(content);
        stripped.push('\n');
    }

    let parsed: toml::Value = toml::from_str(&stripped).map_err(|e| {
        Error::InvalidMandate(format!(
            "read_snapshot: malformed header in {}: {e}",
            path.display(),
        ))
    })?;

    let team_id = expect_str(&parsed, "team_id", path)?.to_string();
    let spawned_at = expect_str(&parsed, "spawned_at", path)?.to_string();
    let mode_str = expect_str(&parsed, "mode", path)?;
    let mode = match mode_str {
        "trusted" => Mode::Trusted,
        "gated" => Mode::Gated,
        "sealed" => Mode::Sealed,
        other => {
            return Err(Error::InvalidMandate(format!(
                "read_snapshot: bad mode \"{other}\" in {}",
                path.display(),
            )));
        }
    };
    let allowlist_sha = expect_str(&parsed, "allowlist_sha", path)?.to_string();
    let source_paths = expect_str_array(&parsed, "source_paths", path)?;

    // v2.7.9: project_root added. Back-compat: missing → "".
    let project_root = match parsed.get("project_root") {
        Some(toml::Value::String(s)) => s.clone(),
        Some(_) => {
            return Err(Error::InvalidMandate(format!(
                "read_snapshot: header field \"project_root\" present but not a string in {}",
                path.display(),
            )));
        }
        None => {
            tracing::warn!(
                snapshot = %path.display(),
                "snapshot predates v2.7.9 (no project_root in header); falling back to empty string",
            );
            String::new()
        }
    };

    Ok(PolicySnapshot {
        team_id,
        project_root,
        mode,
        spawned_at,
        source_paths,
        allowlist_sha,
        snapshot_path: path.to_path_buf(),
    })
}

fn expect_str<'a>(v: &'a toml::Value, field: &str, path: &Path) -> Result<&'a str> {
    match v.get(field) {
        Some(toml::Value::String(s)) => Ok(s.as_str()),
        _ => Err(Error::InvalidMandate(format!(
            "read_snapshot: header field \"{field}\" missing or not a string in {}",
            path.display(),
        ))),
    }
}

fn expect_str_array(v: &toml::Value, field: &str, path: &Path) -> Result<Vec<String>> {
    match v.get(field) {
        Some(toml::Value::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                match item {
                    toml::Value::String(s) => out.push(s.clone()),
                    _ => {
                        return Err(Error::InvalidMandate(format!(
                            "read_snapshot: header field \"{field}\" has non-string entry in {}",
                            path.display(),
                        )))
                    }
                }
            }
            Ok(out)
        }
        _ => Err(Error::InvalidMandate(format!(
            "read_snapshot: header field \"{field}\" missing or not an array in {}",
            path.display(),
        ))),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::await_holding_lock)] // env-var serialization lock is held
                                     // across awaits by design; tests run
                                     // on a current_thread runtime.
mod tests {
    use super::*;
    use crate::testlock::ENV_LOCK;
    use std::path::PathBuf;

    struct EnvGuard {
        keys: Vec<(String, Option<String>)>,
    }

    impl EnvGuard {
        fn new(keys: &[&str]) -> Self {
            Self {
                keys: keys
                    .iter()
                    .map(|k| (k.to_string(), std::env::var(k).ok()))
                    .collect(),
            }
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

    fn make_project() -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix("subctl-snap-proj-")
            .tempdir()
            .unwrap();
        std::fs::create_dir_all(dir.path().join(".subctl")).unwrap();
        std::fs::write(
            dir.path().join(".subctl/policy.toml"),
            r#"preset = "none"
default_mode = "gated"

[mode.gated]

[mode.gated.allow]
commands = ["ls", "pwd"]

[[mode.gated.allow_pattern]]
command = "git"
args = ["status", "diff", "log"]

[mode.gated.deny_always]
substrings = ["rm -rf"]
"#,
        )
        .unwrap();
        dir
    }

    fn setup() -> (tempfile::TempDir, tempfile::TempDir, tempfile::TempDir) {
        let project = make_project();
        let state = tempfile::Builder::new()
            .prefix("subctl-snap-state-")
            .tempdir()
            .unwrap();
        let cfg = tempfile::Builder::new()
            .prefix("subctl-snap-cfg-")
            .tempdir()
            .unwrap();
        unsafe {
            std::env::set_var("SUBCTL_STATE_DIR", state.path());
            std::env::set_var("SUBCTL_CONFIG_DIR", cfg.path());
            std::env::set_var("SUBCTL_INSTALL_ROOT", install_root_fixture());
        }
        (project, state, cfg)
    }

    #[test]
    fn snapshot_path_default_layout() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&["SUBCTL_STATE_DIR"]);
        unsafe {
            std::env::remove_var("SUBCTL_STATE_DIR");
        }
        let p = get_snapshot_path("foo-team");
        let s = p.to_string_lossy();
        assert!(s.ends_with("/.local/state/subctl/teams/foo-team/policy.snapshot.toml"));
    }

    #[test]
    fn snapshot_path_honors_env() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&["SUBCTL_STATE_DIR"]);
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("SUBCTL_STATE_DIR", tmp.path());
        }
        let p = get_snapshot_path("foo-team");
        assert_eq!(p, tmp.path().join("teams/foo-team/policy.snapshot.toml"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_then_read_roundtrip() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&[
            "SUBCTL_STATE_DIR",
            "SUBCTL_CONFIG_DIR",
            "SUBCTL_INSTALL_ROOT",
        ]);
        let (project, _state, _cfg) = setup();

        let meta = write_snapshot_with_local_mode("rt-team", project.path(), Mode::Gated)
            .await
            .unwrap();
        let (_policy, read_meta) = read_snapshot("rt-team").await.unwrap().expect("present");
        assert_eq!(read_meta.team_id, meta.team_id);
        assert_eq!(read_meta.project_root, meta.project_root);
        assert_eq!(read_meta.mode, meta.mode);
        assert_eq!(read_meta.spawned_at, meta.spawned_at);
        assert_eq!(read_meta.allowlist_sha, meta.allowlist_sha);
        assert_eq!(read_meta.source_paths, meta.source_paths);
        assert_eq!(read_meta.snapshot_path, meta.snapshot_path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn header_starts_with_banner_and_expected_lines() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&[
            "SUBCTL_STATE_DIR",
            "SUBCTL_CONFIG_DIR",
            "SUBCTL_INSTALL_ROOT",
        ]);
        let (project, _state, _cfg) = setup();
        write_snapshot_with_local_mode("hdr-team", project.path(), Mode::Gated)
            .await
            .unwrap();
        let raw = std::fs::read_to_string(get_snapshot_path("hdr-team")).unwrap();
        let lines: Vec<&str> = raw.split('\n').collect();
        assert_eq!(lines[0], "# subctl policy snapshot");
        assert!(lines
            .iter()
            .any(|l| l.starts_with("# team_id = \"hdr-team\"")));
        assert!(lines.iter().any(|l| l.starts_with("# mode = \"gated\"")));
        assert!(lines.iter().any(|l| l.starts_with("# spawned_at = ")));
        assert!(lines.iter().any(|l| l.starts_with("# allowlist_sha = ")));
        assert!(lines.iter().any(|l| l.trim() == "# source_paths = ["));
        assert!(lines.iter().any(|l| l.trim() == "# port_origin = \"rust\""));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn body_parses_via_toml() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&[
            "SUBCTL_STATE_DIR",
            "SUBCTL_CONFIG_DIR",
            "SUBCTL_INSTALL_ROOT",
        ]);
        let (project, _state, _cfg) = setup();
        write_snapshot_with_local_mode("body-team", project.path(), Mode::Gated)
            .await
            .unwrap();
        let raw = std::fs::read_to_string(get_snapshot_path("body-team")).unwrap();
        let mut body_start = 0;
        for (i, line) in raw.split('\n').enumerate() {
            let t = line.trim();
            if t.starts_with('#') || t.is_empty() {
                body_start = i + 1;
                continue;
            }
            break;
        }
        let body: Vec<&str> = raw.split('\n').collect();
        let body = body[body_start..].join("\n");
        let parsed: toml::Value = toml::from_str(&body).expect("parse body");
        assert_eq!(
            parsed.get("default_mode").and_then(|v| v.as_str()),
            Some("gated"),
        );
        assert!(parsed.get("__meta").is_none());
        assert!(parsed.get("mode").and_then(|m| m.get("gated")).is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn spawn_mode_override_wins() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&[
            "SUBCTL_STATE_DIR",
            "SUBCTL_CONFIG_DIR",
            "SUBCTL_INSTALL_ROOT",
        ]);
        let (project, _state, _cfg) = setup();
        write_snapshot_with_local_mode("override-team", project.path(), Mode::Trusted)
            .await
            .unwrap();
        let (policy, meta) = read_snapshot("override-team")
            .await
            .unwrap()
            .expect("present");
        assert_eq!(meta.mode, Mode::Trusted);
        assert_eq!(policy.default_mode, Some(Mode::Trusted));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn allowlist_sha_deterministic_for_same_input() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&[
            "SUBCTL_STATE_DIR",
            "SUBCTL_CONFIG_DIR",
            "SUBCTL_INSTALL_ROOT",
        ]);
        let (project, _state, _cfg) = setup();
        let a = write_snapshot_with_local_mode("det-a", project.path(), Mode::Gated)
            .await
            .unwrap();
        let b = write_snapshot_with_local_mode("det-b", project.path(), Mode::Gated)
            .await
            .unwrap();
        assert_eq!(a.allowlist_sha, b.allowlist_sha);
        assert_eq!(a.allowlist_sha.len(), 8);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn allowlist_sha_changes_when_policy_changes() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&[
            "SUBCTL_STATE_DIR",
            "SUBCTL_CONFIG_DIR",
            "SUBCTL_INSTALL_ROOT",
        ]);
        let (project, _state, _cfg) = setup();
        let baseline = write_snapshot_with_local_mode("sha-baseline", project.path(), Mode::Gated)
            .await
            .unwrap();

        std::fs::write(
            project.path().join(".subctl/policy.toml"),
            r#"preset = "none"
default_mode = "gated"

[mode.gated]

[mode.gated.allow]
commands = ["ls", "pwd"]

[[mode.gated.allow_pattern]]
command = "git"
args = ["status", "diff", "log"]

[mode.gated.deny_always]
substrings = ["rm -rf", "dd if="]
"#,
        )
        .unwrap();

        let mutated = write_snapshot_with_local_mode("sha-mutated", project.path(), Mode::Gated)
            .await
            .unwrap();
        assert_ne!(mutated.allowlist_sha, baseline.allowlist_sha);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn respawn_rotates_old_snapshot() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&[
            "SUBCTL_STATE_DIR",
            "SUBCTL_CONFIG_DIR",
            "SUBCTL_INSTALL_ROOT",
        ]);
        let (project, _state, _cfg) = setup();
        write_snapshot_with_local_mode("respawn-team", project.path(), Mode::Gated)
            .await
            .unwrap();
        let path = get_snapshot_path("respawn-team");
        assert!(path.exists());
        let old_path = path.with_file_name(format!(
            "{}.old",
            path.file_name().unwrap().to_string_lossy()
        ));
        assert!(!old_path.exists());

        let first = std::fs::read_to_string(&path).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        write_snapshot_with_local_mode("respawn-team", project.path(), Mode::Gated)
            .await
            .unwrap();

        assert!(path.exists());
        assert!(old_path.exists());
        let rotated = std::fs::read_to_string(&old_path).unwrap();
        assert_eq!(rotated, first);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn state_dir_env_honored_e2e() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&[
            "SUBCTL_STATE_DIR",
            "SUBCTL_CONFIG_DIR",
            "SUBCTL_INSTALL_ROOT",
        ]);
        let (project, state, _cfg) = setup();
        let meta = write_snapshot_with_local_mode("env-team", project.path(), Mode::Gated)
            .await
            .unwrap();
        assert!(meta
            .snapshot_path
            .to_string_lossy()
            .contains(state.path().to_string_lossy().as_ref()));
        assert!(meta.snapshot_path.exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_snapshot_missing_returns_none() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&["SUBCTL_STATE_DIR"]);
        let state = tempfile::Builder::new()
            .prefix("subctl-snap-never-")
            .tempdir()
            .unwrap();
        unsafe {
            std::env::set_var("SUBCTL_STATE_DIR", state.path());
        }
        let r = read_snapshot("never-written").await.unwrap();
        assert!(r.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn project_root_roundtrips_through_header() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&[
            "SUBCTL_STATE_DIR",
            "SUBCTL_CONFIG_DIR",
            "SUBCTL_INSTALL_ROOT",
        ]);
        let (project, _state, _cfg) = setup();
        let meta = write_snapshot_with_local_mode("pr-team", project.path(), Mode::Gated)
            .await
            .unwrap();
        assert_eq!(meta.project_root, project.path().to_string_lossy());
        let raw = std::fs::read_to_string(get_snapshot_path("pr-team")).unwrap();
        let expected = format!(
            "# project_root = {}",
            json_quote(&project.path().to_string_lossy())
        );
        assert!(
            raw.contains(&expected),
            "header missing project_root line: {raw}"
        );
        let (_policy, read_meta) = read_snapshot("pr-team").await.unwrap().expect("present");
        assert_eq!(read_meta.project_root, project.path().to_string_lossy());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn back_compat_legacy_header_without_project_root() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&["SUBCTL_STATE_DIR"]);
        let state = tempfile::Builder::new()
            .prefix("subctl-snap-legacy-")
            .tempdir()
            .unwrap();
        unsafe {
            std::env::set_var("SUBCTL_STATE_DIR", state.path());
        }
        let path = get_snapshot_path("legacy-team");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"# subctl policy snapshot
# team_id = "legacy-team"
# spawned_at = "2026-05-11T00:00:00.000Z"
# mode = "gated"
# source_paths = []
# allowlist_sha = "deadbeef"

default_mode = "gated"

[mode]
[mode.gated]
[mode.gated.allow]
commands = ["ls"]
"#,
        )
        .unwrap();
        let (_policy, meta) = read_snapshot("legacy-team")
            .await
            .unwrap()
            .expect("present");
        assert_eq!(meta.project_root, "");
        assert_eq!(meta.team_id, "legacy-team");
        assert_eq!(meta.mode, Mode::Gated);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_snapshot_throws_on_malformed_body() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::new(&["SUBCTL_STATE_DIR"]);
        let state = tempfile::Builder::new()
            .prefix("subctl-snap-corrupt-")
            .tempdir()
            .unwrap();
        unsafe {
            std::env::set_var("SUBCTL_STATE_DIR", state.path());
        }
        let path = get_snapshot_path("corrupt-team");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"# subctl policy snapshot
# team_id = "corrupt-team"
# spawned_at = "2026-05-11T00:00:00.000Z"
# mode = "gated"
# source_paths = []
# allowlist_sha = "deadbeef"

this is not [valid toml
"#,
        )
        .unwrap();
        let err = read_snapshot("corrupt-team").await.unwrap_err();
        assert!(err.to_string().contains("read_snapshot"), "{err}");
    }
}
