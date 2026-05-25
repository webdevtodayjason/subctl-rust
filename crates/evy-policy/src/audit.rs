//! JSONL audit-log appender. Rust port of
//! `components/evy/tools/policy/audit.ts` (pack 09).
//!
//! Path: `~/.local/state/subctl/audit/<team_id>.jsonl` (honours
//! `SUBCTL_STATE_DIR`).
//!
//! Three event_type discriminants are produced:
//!   - [`AuditEventType::Header`] — emitted at spawn time by
//!     [`AuditWriter::write_header`].
//!   - [`AuditEventType::Check`] — emitted per policy check, allow or deny.
//!   - [`AuditEventType::VerifierCorrection`] — emitted when the master
//!     daemon's denial-cluster detector fires.
//!
//! Concurrency: per pack 09 §4, each line is appended in a single `write`
//! call against a file opened with `O_APPEND`. POSIX guarantees writes
//! shorter than `PIPE_BUF` (typically 4 KB) from concurrent processes
//! don't tear.
//!
//! Failure semantics (pack 09 §4): fail-open. An audit write failure must
//! not block the policy decision. The writer records into an internal
//! per-team failure counter; callers can inspect via
//! [`AuditWriter::failure_count`] for operator metrics.
//!
//! ROTATION NOTE: v3's audit.ts has ~150 lines of size-based rotation
//! (50 MB threshold, 3 generations). Per the Phase 1 spec, JSONL-on-disk
//! is sufficient and rotation lives in a future task; this port omits it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use evy_core::{Error, Result};
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::types::{AuditDecision, AuditEntry, AuditEventType, Mode};

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

fn resolve_audit_dir() -> PathBuf {
    resolve_state_dir().join("audit")
}

/// Deterministic path to a team's active audit log.
#[must_use]
pub fn get_audit_log_path(team_id: &str) -> PathBuf {
    resolve_audit_dir().join(format!("{team_id}.jsonl"))
}

// ---------------------------------------------------------------------------
// AuditWriter
// ---------------------------------------------------------------------------

/// Append-only JSONL writer for one team's audit log.
///
/// Open once with [`AuditWriter::open`], call [`AuditWriter::append`] for
/// every event. The writer holds a `Mutex<File>` so concurrent
/// `append` calls in the same process serialize, which is necessary
/// because the borrow checker won't let us issue overlapping `write_all`
/// against an `&mut File` from `&self`.
pub struct AuditWriter {
    team_id: String,
    path: PathBuf,
    file: Mutex<File>,
    failures: AtomicU64,
}

impl AuditWriter {
    /// Open (or create) the audit log at the conventional path for
    /// `team_id`. Honours `SUBCTL_STATE_DIR`.
    pub async fn open(team_id: &str) -> Result<Self> {
        if team_id.is_empty() {
            return Err(Error::InvalidMandate(
                "AuditWriter::open: team_id is required".into(),
            ));
        }
        let path = get_audit_log_path(team_id);
        Self::open_at(team_id, &path).await
    }

    /// Open at an explicit path. Used by tests and by callers that already
    /// know the path.
    pub async fn open_at(team_id: &str, path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        Ok(Self {
            team_id: team_id.to_owned(),
            path: path.to_path_buf(),
            file: Mutex::new(file),
            failures: AtomicU64::new(0),
        })
    }

    /// Append one entry. Fail-open: never raises; failures bump the counter
    /// exposed by [`AuditWriter::failure_count`]. Returns `Ok(())` even on
    /// disk-full / permission-denied / serialization failure.
    pub async fn append(&self, entry: AuditEntry) -> Result<()> {
        let mut line = match serde_json::to_string(&entry) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    team_id = %self.team_id,
                    error = %e,
                    "audit serialization failed; dropping entry",
                );
                self.failures.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
        };
        line.push('\n');

        let mut file = self.file.lock().await;
        if let Err(e) = file.write_all(line.as_bytes()).await {
            tracing::warn!(
                team_id = %self.team_id,
                error = %e,
                "audit write failed",
            );
            self.failures.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        // `tokio::fs::File` buffers writes internally and only dispatches the
        // buffer when it fills or on flush/shutdown. Without an explicit
        // flush, a synchronous `std::fs::read_to_string` call right after the
        // `.await` returns may see an empty file. We pay the (small) cost of
        // a flush per append to guarantee readers see the line immediately.
        if let Err(e) = file.flush().await {
            tracing::warn!(
                team_id = %self.team_id,
                error = %e,
                "audit flush failed",
            );
            self.failures.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Convenience wrapper: write the spawn-time `header` event.
    pub async fn write_header(&self, mode: Mode, allowlist_sha: &str) -> Result<()> {
        let entry = AuditEntry {
            ts: now_iso(),
            team_id: self.team_id.clone(),
            agent_session_id: None,
            mode,
            allowlist_sha: Some(allowlist_sha.to_owned()),
            command: String::new(),
            decision: AuditDecision::Allow,
            rule: Some("spawn".to_owned()),
            rule_path: None,
            event_type: AuditEventType::Header,
        };
        self.append(entry).await
    }

    /// Convenience wrapper: write a `verifier_correction` event.
    pub async fn write_verifier_correction(
        &self,
        rule: &str,
        mode: Mode,
        allowlist_sha: &str,
    ) -> Result<()> {
        let entry = AuditEntry {
            ts: now_iso(),
            team_id: self.team_id.clone(),
            agent_session_id: None,
            mode,
            allowlist_sha: Some(allowlist_sha.to_owned()),
            command: String::new(),
            decision: AuditDecision::Deny,
            rule: Some(rule.to_owned()),
            rule_path: None,
            event_type: AuditEventType::VerifierCorrection,
        };
        self.append(entry).await
    }

    /// Total number of failed appends since this writer was opened.
    #[must_use]
    pub fn failure_count(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    /// Path the writer is appending to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn now_iso() -> String {
    use chrono::SecondsFormat;
    chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
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

    fn read_lines(path: &Path) -> Vec<String> {
        if !path.exists() {
            return Vec::new();
        }
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect()
    }

    fn make_check_entry(team_id: &str, command: &str, decision: AuditDecision) -> AuditEntry {
        AuditEntry {
            ts: now_iso(),
            team_id: team_id.into(),
            agent_session_id: None,
            mode: Mode::Gated,
            allowlist_sha: Some("deadbeef".into()),
            command: command.into(),
            decision,
            rule: None,
            rule_path: None,
            event_type: AuditEventType::Check,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn append_single_entry() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("single.jsonl");
        let w = AuditWriter::open_at("single-team", &path).await.unwrap();
        w.append(make_check_entry("single-team", "ls", AuditDecision::Allow))
            .await
            .unwrap();
        drop(w);
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1);
        let parsed: AuditEntry = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(parsed.team_id, "single-team");
        assert_eq!(parsed.command, "ls");
        assert_eq!(parsed.event_type, AuditEventType::Check);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn multiple_appends_preserve_order() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("order.jsonl");
        let w = AuditWriter::open_at("order-team", &path).await.unwrap();
        for cmd in ["alpha", "beta", "gamma"] {
            w.append(make_check_entry("order-team", cmd, AuditDecision::Allow))
                .await
                .unwrap();
        }
        drop(w);
        let cmds: Vec<String> = read_lines(&path)
            .iter()
            .map(|l| serde_json::from_str::<AuditEntry>(l).unwrap().command)
            .collect();
        assert_eq!(
            cmds,
            vec!["alpha".to_string(), "beta".into(), "gamma".into()]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn entry_roundtrips_through_json() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("json.jsonl");
        let w = AuditWriter::open_at("json-team", &path).await.unwrap();
        let entry = AuditEntry {
            ts: "2026-05-11T10:00:00.000Z".into(),
            team_id: "json-team".into(),
            agent_session_id: Some("sess_abc".into()),
            mode: Mode::Gated,
            allowlist_sha: Some("12345678".into()),
            command: "git commit -m 'fix: don\\'t panic'".into(),
            decision: AuditDecision::Allow,
            rule: Some("allow_pattern: git commit|status|diff".into()),
            rule_path: Some("mode.gated.allow_pattern[2]".into()),
            event_type: AuditEventType::Check,
        };
        w.append(entry.clone()).await.unwrap();
        drop(w);
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1);
        let parsed: AuditEntry = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(parsed, entry);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_header_emits_header_event() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hdr.jsonl");
        let w = AuditWriter::open_at("hdr-team", &path).await.unwrap();
        w.write_header(Mode::Gated, "a3f9c2e1").await.unwrap();
        drop(w);
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1);
        let parsed: AuditEntry = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(parsed.event_type, AuditEventType::Header);
        assert_eq!(parsed.decision, AuditDecision::Allow);
        assert_eq!(parsed.rule.as_deref(), Some("spawn"));
        assert_eq!(parsed.command, "");
        assert_eq!(parsed.mode, Mode::Gated);
        assert_eq!(parsed.allowlist_sha.as_deref(), Some("a3f9c2e1"));
        assert_eq!(parsed.team_id, "hdr-team");
        // ISO 8601 with millis + Z
        let re = regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$").unwrap();
        assert!(re.is_match(&parsed.ts), "got {}", parsed.ts);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_verifier_correction_emits_deny_event() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vc.jsonl");
        let w = AuditWriter::open_at("vc-team", &path).await.unwrap();
        let rule = "verifier: 5 denials in 60s, pattern 'mode.gated.deny_always.regex'";
        w.write_verifier_correction(rule, Mode::Gated, "deadbeef")
            .await
            .unwrap();
        drop(w);
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1);
        let parsed: AuditEntry = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(parsed.event_type, AuditEventType::VerifierCorrection);
        assert_eq!(parsed.decision, AuditDecision::Deny);
        assert_eq!(parsed.rule.as_deref(), Some(rule));
        assert_eq!(parsed.command, "");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_appends_do_not_tear() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("concurrent.jsonl");
        let w = std::sync::Arc::new(
            AuditWriter::open_at("concurrent-team", &path)
                .await
                .unwrap(),
        );
        let n = 100;
        let mut handles = Vec::with_capacity(n);
        for i in 0..n {
            let w2 = w.clone();
            handles.push(tokio::spawn(async move {
                w2.append(AuditEntry {
                    ts: now_iso(),
                    team_id: "concurrent-team".into(),
                    agent_session_id: None,
                    mode: Mode::Gated,
                    allowlist_sha: Some("deadbeef".into()),
                    command: format!("cmd-{i}"),
                    decision: AuditDecision::Allow,
                    rule: None,
                    rule_path: None,
                    event_type: AuditEventType::Check,
                })
                .await
                .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        drop(w);
        let lines = read_lines(&path);
        assert_eq!(lines.len(), n);
        let mut seen = std::collections::HashSet::new();
        for l in lines {
            let parsed: AuditEntry = serde_json::from_str(&l).expect("torn line");
            seen.insert(parsed.command);
        }
        assert_eq!(seen.len(), n);
        for i in 0..n {
            assert!(seen.contains(&format!("cmd-{i}")));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn state_dir_env_honored_e2e() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("SUBCTL_STATE_DIR").ok();
        unsafe {
            std::env::set_var("SUBCTL_STATE_DIR", dir.path());
        }
        let p = get_audit_log_path("env-team");
        assert_eq!(p, dir.path().join("audit/env-team.jsonl"));
        let w = AuditWriter::open("env-team").await.unwrap();
        w.write_header(Mode::Gated, "deadbeef").await.unwrap();
        drop(w);
        assert!(p.exists());
        // restore
        match prev {
            Some(v) => unsafe { std::env::set_var("SUBCTL_STATE_DIR", v) },
            None => unsafe { std::env::remove_var("SUBCTL_STATE_DIR") },
        }
    }
}
