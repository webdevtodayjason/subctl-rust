//! Voice layer configuration: schema + file-backed store + live watcher.
//!
//! Mirrors v3's `components/evy/voice-config.ts` semantics:
//!
//! - File lives at `~/.config/subctl/voice.json` (overridable per
//!   [`VoiceConfigStore::open`] for tests + non-default deployments).
//! - Seeds defaults on first read if the file is missing.
//! - Malformed JSON is **not** silently masked — callers see the parse
//!   error. (v3 falls back to defaults; v4 surfaces the error so the
//!   dashboard can show a useful message instead of pretending all is
//!   well.)
//! - `set()` writes atomically (tmp file + rename) so a partial write
//!   never leaves a half-empty JSON on disk.
//! - File-watcher reloads on external modification with a **200ms
//!   debounce** — same as v3, same reason: atomic-rename editors fire
//!   the watcher twice on macOS.
//!
//! The store owns:
//!   - the live [`VoiceConfig`] (broadcast over a `tokio::sync::watch`
//!     channel — last-value-wins semantics; lossless for subscribers
//!     who only care about the latest setting),
//!   - a background tokio task that hosts the [`notify`] watcher and
//!     the debounce timer,
//!   - a [`CancellationToken`] that the task watches; dropping the
//!     store cancels the token so the watcher unwinds cleanly.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::error::{Result, VoiceError};

/// On-disk schema for `voice.json`. Field names match v3 byte-for-byte
/// so an operator can swap between v3 and v4 daemons without rewriting
/// the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceConfig {
    /// Master switch. Defaults to **`false`** — operator must opt in to
    /// avoid surprise TTS spam. Renders refuse with
    /// [`VoiceError::Disabled`] when this is false.
    pub enabled: bool,
    /// Default voice id used when the caller doesn't pass one. Empty
    /// string is normalised to the v3 default `"evy-rachel-weisz"` on
    /// load.
    pub default_voice_id: String,
    /// TTS model the server should use. `voxcpm-0.5b` matches v3.
    pub model: String,
    /// Base URL of the local TTS server. `http://localhost:8789`
    /// matches the launchd-managed `com.subctl.tts` service.
    pub tts_server: String,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_voice_id: "evy-rachel-weisz".to_owned(),
            model: "voxcpm-0.5b".to_owned(),
            tts_server: "http://localhost:8789".to_owned(),
        }
    }
}

impl VoiceConfig {
    /// Apply v3's "empty string → default" normalisation. Idempotent.
    fn normalised(mut self) -> Self {
        let d = Self::default();
        if self.default_voice_id.is_empty() {
            self.default_voice_id = d.default_voice_id;
        }
        if self.model.is_empty() {
            self.model = d.model;
        }
        if self.tts_server.is_empty() {
            self.tts_server = d.tts_server;
        }
        self
    }
}

/// Read the config file at `path` (seeding defaults if missing). Pure
/// function — no in-memory cache, matching v3's "VERSION is the one
/// canonical source" discipline.
pub fn load_from_path(path: &Path) -> Result<VoiceConfig> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|source| VoiceError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
        }
        let defaults = VoiceConfig::default();
        write_atomic(path, &defaults)?;
        return Ok(defaults);
    }
    let bytes = std::fs::read(path).map_err(|source| VoiceError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let parsed: VoiceConfig = serde_json::from_slice(&bytes)?;
    Ok(parsed.normalised())
}

/// Persist `cfg` to `path` atomically (tmp file + rename in the same
/// directory, so the file watcher sees a single coherent change).
fn write_atomic(path: &Path, cfg: &VoiceConfig) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("voice.json")
    ));
    let bytes = serde_json::to_vec_pretty(cfg)?;
    std::fs::write(&tmp, &bytes).map_err(|source| VoiceError::Io {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, path).map_err(|source| VoiceError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Default config location. Honours `SUBCTL_VOICE_CONFIG_PATH` (mirrors
/// v3's `ENV_OVERRIDE`) before falling back to
/// `~/.config/subctl/voice.json`.
#[must_use]
pub fn default_path() -> PathBuf {
    if let Ok(p) = std::env::var("SUBCTL_VOICE_CONFIG_PATH") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    PathBuf::from(home)
        .join(".config")
        .join("subctl")
        .join("voice.json")
}

/// Debounce window for the file watcher. Matches v3 byte-for-byte; the
/// rationale (atomic-rename editors fire twice on macOS) is the same.
const DEBOUNCE: Duration = Duration::from_millis(200);

/// Live, file-backed voice config with a `tokio::sync::watch` broadcast
/// channel. Cheap to clone via [`Arc`] (which is how the renderer holds
/// it).
///
/// Dropping the store cancels the file-watcher task; the underlying
/// notify watcher is dropped along with it, releasing the OS handle.
pub struct VoiceConfigStore {
    path: PathBuf,
    tx: watch::Sender<VoiceConfig>,
    cancel: CancellationToken,
}

impl Drop for VoiceConfigStore {
    fn drop(&mut self) {
        // Signal the background watcher task to unwind. If nobody's
        // subscribed to the watch channel any longer, dropping `tx`
        // below also closes the channel, but the cancellation token is
        // the primary shutdown signal.
        self.cancel.cancel();
    }
}

impl VoiceConfigStore {
    /// Open (or initialise) the store at `path`. Spawns the file-watcher
    /// background task on the current tokio runtime.
    pub async fn open(path: &Path) -> Result<Self> {
        let path = path.to_path_buf();
        let initial = load_from_path(&path)?;
        let (tx, _rx) = watch::channel(initial);
        let cancel = CancellationToken::new();

        spawn_watcher(path.clone(), tx.clone(), cancel.clone())?;

        Ok(Self { path, tx, cancel })
    }

    /// Snapshot of the current config.
    #[must_use]
    pub fn current(&self) -> VoiceConfig {
        self.tx.borrow().clone()
    }

    /// `enabled` flag from the current snapshot. Convenience.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.tx.borrow().enabled
    }

    /// Subscribe to live updates. Subscribers receive the *current*
    /// value on first `borrow()` plus every subsequent change.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<VoiceConfig> {
        self.tx.subscribe()
    }

    /// Persist `cfg` to disk and immediately broadcast the new value to
    /// subscribers. The on-disk write will *also* fire the file watcher,
    /// which will publish the same value a second time after the
    /// debounce window — that's fine: `watch` channels are last-value
    /// wins, no queue to overflow.
    pub async fn set(&self, cfg: VoiceConfig) -> Result<()> {
        let normalised = cfg.normalised();
        // Publish first so in-process callers see the new value without
        // waiting for the watcher to round-trip through the fs.
        self.tx.send_replace(normalised.clone());
        write_atomic(&self.path, &normalised)?;
        Ok(())
    }

    /// Path the store is reading.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Spawn the file-watcher task. The notify watcher itself runs on its
/// own thread (FSEvents on macOS); a `std::sync::mpsc` bridge forwards
/// raw events into a `tokio::sync::mpsc` channel for the async side.
fn spawn_watcher(
    path: PathBuf,
    tx: watch::Sender<VoiceConfig>,
    cancel: CancellationToken,
) -> Result<()> {
    // We watch the *parent directory* (non-recursive), not the file
    // itself, because atomic-rename editors replace the inode — a
    // file-direct watcher would miss the subsequent writes. Filter for
    // the target filename inside the event handler.
    let parent = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let target_name = path.file_name().map(|n| n.to_os_string());

    let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<()>();

    let target_name_for_cb = target_name.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(evt) = res else { return };
        // Skip access-only events; we only care about content changes.
        match evt.kind {
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {}
            _ => return,
        }
        // Filter to our target file (parent-dir watch sees siblings too).
        let matches = evt.paths.iter().any(|p| {
            p.file_name()
                .zip(target_name_for_cb.as_deref())
                .is_some_and(|(a, b)| a == b)
        });
        if matches {
            // Best-effort: receiver dropped means store is being torn
            // down; let the cancellation token finish the job.
            let _ = notify_tx.send(());
        }
    })
    .map_err(|e| VoiceError::Watcher(format!("notify init: {e}")))?;

    // Parent dir must exist before we can watch it. `load_from_path`
    // already created it when seeding defaults, so this is normally a
    // no-op; guard anyway in case someone passes a bare filename.
    if !parent.as_os_str().is_empty() && !parent.exists() {
        std::fs::create_dir_all(&parent).map_err(|source| VoiceError::Io {
            path: parent.clone(),
            source,
        })?;
    }

    watcher
        .watch(&parent, RecursiveMode::NonRecursive)
        .map_err(|e| VoiceError::Watcher(format!("watch {}: {e}", parent.display())))?;

    let task_path = path.clone();
    let task_cancel = cancel.clone();
    tokio::spawn(async move {
        // Keep the notify watcher alive for the lifetime of the task.
        let _watcher_keepalive = watcher;
        loop {
            tokio::select! {
                biased;
                () = task_cancel.cancelled() => break,
                evt = notify_rx.recv() => {
                    if evt.is_none() { break; }
                    // Debounce: sleep, then drain any events that piled
                    // up during the sleep. This collapses the
                    // atomic-rename double-fire (and any subsequent
                    // burst) into a single reload.
                    tokio::time::sleep(DEBOUNCE).await;
                    while notify_rx.try_recv().is_ok() {}
                    match load_from_path(&task_path) {
                        Ok(cfg) => {
                            // send_replace ignores no-subscribers; we
                            // still update the in-Sender value so a
                            // late subscriber sees the latest config.
                            tx.send_replace(cfg);
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                path = %task_path.display(),
                                "voice config reload failed; keeping previous value",
                            );
                        }
                    }
                }
            }
        }
        tracing::debug!("voice config watcher task exiting");
    });

    Ok(())
}

/// Convenience wrapper so the renderer can hold an `Arc<VoiceConfigStore>`
/// without spelling out the import everywhere.
pub type SharedVoiceConfigStore = Arc<VoiceConfigStore>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_v3() {
        let c = VoiceConfig::default();
        assert!(!c.enabled);
        assert_eq!(c.default_voice_id, "evy-rachel-weisz");
        assert_eq!(c.model, "voxcpm-0.5b");
        assert_eq!(c.tts_server, "http://localhost:8789");
    }

    #[test]
    fn serde_roundtrip_preserves_fields() {
        let c = VoiceConfig {
            enabled: true,
            default_voice_id: "evy-x".to_owned(),
            model: "vox-2".to_owned(),
            tts_server: "http://localhost:9000".to_owned(),
        };
        let s = serde_json::to_string(&c).unwrap();
        let back: VoiceConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn normalised_substitutes_empties() {
        let c = VoiceConfig {
            enabled: true,
            default_voice_id: "".to_owned(),
            model: "".to_owned(),
            tts_server: "".to_owned(),
        }
        .normalised();
        assert!(c.enabled);
        assert_eq!(c.default_voice_id, "evy-rachel-weisz");
        assert_eq!(c.model, "voxcpm-0.5b");
        assert_eq!(c.tts_server, "http://localhost:8789");
    }

    #[test]
    fn json_compat_with_v3_shape() {
        // Byte-for-byte v3 voice.json sample.
        let v3 = r#"{
  "enabled": false,
  "default_voice_id": "evy-rachel-weisz",
  "model": "voxcpm-0.5b",
  "tts_server": "http://localhost:8789"
}"#;
        let c: VoiceConfig = serde_json::from_str(v3).unwrap();
        assert_eq!(c, VoiceConfig::default());
    }
}
