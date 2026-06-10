//! Small `tmux` control helper.
//!
//! Wraps the four `tmux` invocations the provider adapters need
//! (`new-window`, `send-keys`/`paste-buffer`, `kill-window`,
//! `list-windows`, `has-session`) in async fns that shell out via
//! `tokio::process::Command` and return [`evy_core::Error::Provider`] on
//! failure. The helper deliberately stays narrow: it knows nothing about
//! mandates, directives, or worker handles — it just speaks tmux.
//!
//! All entry points are crate-private; provider adapters call them from
//! the same crate. If a future caller outside the crate needs raw tmux
//! access, promote the function selectively rather than re-exporting the
//! whole module.

use std::path::Path;

use evy_core::{Error, ProviderKind, Result};
use tokio::process::Command;

/// Which provider triggered the tmux call. Used only to tag
/// [`Error::Provider`]; tmux itself doesn't care.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TmuxScope(pub(crate) ProviderKind);

/// Resolve an absolute `tmux` binary. The daemon runs under launchd's minimal
/// PATH (no Homebrew dirs), so bare `tmux` fails with ENOENT — same PATH
/// split-brain the absolute-`claude`-bin decision dodges. Honors `EVY_TMUX_BIN`,
/// then falls back to the common install locations, then bare `tmux`.
fn tmux_bin() -> std::borrow::Cow<'static, str> {
    if let Ok(p) = std::env::var("EVY_TMUX_BIN") {
        if !p.is_empty() {
            return std::borrow::Cow::Owned(p);
        }
    }
    for p in [
        "/opt/homebrew/bin/tmux",
        "/usr/local/bin/tmux",
        "/usr/bin/tmux",
    ] {
        if std::path::Path::new(p).exists() {
            return std::borrow::Cow::Borrowed(p);
        }
    }
    std::borrow::Cow::Borrowed("tmux")
}

/// Run `tmux <args>`, returning the captured stdout on success.
///
/// On failure (non-zero exit, command not found, …) returns
/// [`Error::Provider`] tagged with `scope`. The error reason includes
/// stderr to make debugging the adapter chain easier.
async fn run_tmux(scope: TmuxScope, args: &[&str]) -> Result<String> {
    let output = Command::new(tmux_bin().as_ref())
        .args(args)
        .output()
        .await
        .map_err(|err| Error::Provider {
            kind: scope.0,
            reason: format!("failed to spawn tmux: {err}"),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let argv = args.join(" ");
        return Err(Error::Provider {
            kind: scope.0,
            reason: format!("tmux {argv} exited {}: {stderr}", output.status),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Public pane-capture for the Orch panel / criterion-#7 observation: returns
/// the last `lines` rows of the session's active pane, or `None` if capture
/// fails (session gone, tmux error). Trailing blank lines are trimmed.
pub async fn tmux_capture(session: &str, lines: usize) -> Option<String> {
    let scope = TmuxScope(ProviderKind::ClaudeCode);
    let out = run_tmux(scope, &["capture-pane", "-p", "-t", session])
        .await
        .ok()?;
    let trimmed: Vec<&str> = out.lines().collect();
    let end = trimmed
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map_or(0, |i| i + 1);
    let start = end.saturating_sub(lines);
    Some(trimmed[start..end].join("\n"))
}

/// `tmux has-session -t <session>`. Cheap liveness probe.
pub(crate) async fn session_exists(scope: TmuxScope, session: &str) -> Result<bool> {
    let output = Command::new(tmux_bin().as_ref())
        .args(["has-session", "-t", session])
        .output()
        .await
        .map_err(|err| Error::Provider {
            kind: scope.0,
            reason: format!("failed to spawn tmux: {err}"),
        })?;
    Ok(output.status.success())
}

/// Public liveness probe for the Orch panel: is the named tmux session alive?
/// Swallows tmux errors as "not alive" — callers want a bool, not a `Result`.
pub async fn tmux_session_alive(session: &str) -> bool {
    session_exists(TmuxScope(ProviderKind::ClaudeCode), session)
        .await
        .unwrap_or(false)
}

/// Public, idempotent `tmux kill-session -t <session>` for the Orch panel's kill
/// action. A missing session is treated as success (already gone).
///
/// # Errors
/// [`Error::Provider`] only if the session exists but tmux fails to kill it.
pub async fn tmux_kill_session(session: &str) -> Result<()> {
    let scope = TmuxScope(ProviderKind::ClaudeCode);
    if !session_exists(scope, session).await? {
        return Ok(());
    }
    run_tmux(scope, &["kill-session", "-t", session]).await?;
    Ok(())
}

/// `tmux new-window -t <session> -n <name> -c <cwd>`.
///
/// Opens a new window in the named session. The session must already
/// exist — Phase 1 assumes the operator (or the smoke-test in Slice E)
/// has spawned a session before dispatching.
pub(crate) async fn new_window(
    scope: TmuxScope,
    session: &str,
    name: &str,
    cwd: &Path,
) -> Result<()> {
    let cwd_str = cwd.to_str().ok_or_else(|| Error::Provider {
        kind: scope.0,
        reason: format!("cwd is not valid UTF-8: {}", cwd.display()),
    })?;
    run_tmux(
        scope,
        &["new-window", "-t", session, "-n", name, "-c", cwd_str],
    )
    .await?;
    Ok(())
}

/// `tmux new-session -d -s <session> -c <cwd> -x <w> -y <h> -e KEY=VAL …`.
///
/// Creates a detached session (Phase 2 slice 2e). `env` pairs become `-e KEY=VAL`
/// flags so the spawned worker inherits `CLAUDE_CONFIG_DIR` + the team/role
/// markers. Ports the v3 `teams.sh:576-581` create line (mouse/wheel ergonomics
/// are deferred — cosmetic, not needed for dispatch).
pub(crate) async fn new_session(
    scope: TmuxScope,
    session: &str,
    cwd: &Path,
    env: &[(&str, &str)],
    width: u16,
    height: u16,
) -> Result<()> {
    let cwd_str = cwd.to_str().ok_or_else(|| Error::Provider {
        kind: scope.0,
        reason: format!("cwd is not valid UTF-8: {}", cwd.display()),
    })?;
    let w = width.to_string();
    let h = height.to_string();
    let mut args: Vec<String> = vec![
        "new-session".into(),
        "-d".into(),
        "-s".into(),
        session.into(),
        "-c".into(),
        cwd_str.into(),
        "-x".into(),
        w,
        "-y".into(),
        h,
    ];
    for (k, v) in env {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    run_tmux(scope, &argv).await?;
    Ok(())
}

/// `tmux send-keys -t <session>:<window> <keys>`.
///
/// Use this for short key sequences ("Enter", "C-c", a short command).
/// For multi-line directive text use [`paste_text`] — `send-keys` is
/// fussy with newlines and shell-argv length, and the v3 bash launchers
/// use `paste-buffer` for exactly that reason.
pub(crate) async fn send_keys(
    scope: TmuxScope,
    session: &str,
    window: &str,
    keys: &[&str],
) -> Result<()> {
    let target = format!("{session}:{window}");
    let mut argv = vec!["send-keys", "-t", target.as_str()];
    argv.extend_from_slice(keys);
    run_tmux(scope, &argv).await?;
    Ok(())
}

/// `tmux send-keys -t <session>:<window> Enter`. Convenience helper.
pub(crate) async fn press_enter(scope: TmuxScope, session: &str, window: &str) -> Result<()> {
    send_keys(scope, session, window, &["Enter"]).await
}

/// Paste `text` into the target window via a named tmux buffer.
///
/// Mirrors the v3 bash launchers:
/// ```text
/// tmux set-buffer -b <buffer_name> "$text"
/// tmux paste-buffer -t <session>:<window> -b <buffer_name>
/// ```
/// Handles multi-line content and special characters cleanly. The
/// buffer is named per worker (caller's responsibility to pick a unique
/// name) and is left in place after paste — tmux GCs buffers under its
/// own scheme; deleting it eagerly is unnecessary.
pub(crate) async fn paste_text(
    scope: TmuxScope,
    session: &str,
    window: &str,
    buffer_name: &str,
    text: &str,
) -> Result<()> {
    run_tmux(scope, &["set-buffer", "-b", buffer_name, text]).await?;
    let target = format!("{session}:{window}");
    run_tmux(
        scope,
        &["paste-buffer", "-t", target.as_str(), "-b", buffer_name],
    )
    .await?;
    Ok(())
}

/// `tmux kill-window -t <session>:<window>`. Idempotent: a missing
/// window returns Ok (tmux's non-zero exit is folded into "already
/// gone" — we re-check with [`window_exists`] before erroring).
pub(crate) async fn kill_window(scope: TmuxScope, session: &str, window: &str) -> Result<()> {
    let target = format!("{session}:{window}");
    let output = Command::new(tmux_bin().as_ref())
        .args(["kill-window", "-t", target.as_str()])
        .output()
        .await
        .map_err(|err| Error::Provider {
            kind: scope.0,
            reason: format!("failed to spawn tmux: {err}"),
        })?;
    if output.status.success() {
        return Ok(());
    }
    // Non-zero — but if the window is already gone the caller's intent
    // (cancel / stop) is already satisfied. Re-probe; only error if it
    // really still exists.
    if !window_exists(scope, session, window).await? {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(Error::Provider {
        kind: scope.0,
        reason: format!("tmux kill-window {target} failed: {stderr}"),
    })
}

/// `tmux list-windows -t <session> -F '#{window_name}'` → membership
/// check for `window`.
pub(crate) async fn window_exists(scope: TmuxScope, session: &str, window: &str) -> Result<bool> {
    // If the session is gone, no windows exist by definition. tmux would
    // exit non-zero in that case; surfacing it as a provider error is
    // noisy — callers usually want "no, it's not running."
    if !session_exists(scope, session).await? {
        return Ok(false);
    }
    let stdout = run_tmux(
        scope,
        &["list-windows", "-t", session, "-F", "#{window_name}"],
    )
    .await?;
    Ok(stdout.lines().any(|line| line.trim() == window))
}

#[cfg(test)]
mod tests {
    //! These tests do NOT shell out to real tmux. They exercise the
    //! pure-logic parts (target formatting, scope tagging). The actual
    //! tmux interaction is verified end-to-end by the Slice E smoke
    //! test, which runs against a real tmux session.

    use super::*;

    #[test]
    fn scope_carries_provider_kind() {
        let s = TmuxScope(ProviderKind::ClaudeCode);
        assert!(matches!(s.0, ProviderKind::ClaudeCode));
    }

    #[tokio::test]
    #[ignore = "requires real tmux + manual setup; exercised by Slice E smoke test"]
    async fn real_tmux_roundtrip() {
        // Smoke shape for the operator to run manually:
        //   tmux new-session -d -s evy-providers-test
        //   cargo test -p evy-providers -- --ignored real_tmux_roundtrip
        //   tmux kill-session -t evy-providers-test
        let scope = TmuxScope(ProviderKind::ClaudeCode);
        let session = "evy-providers-test";
        assert!(session_exists(scope, session).await.unwrap());
        new_window(scope, session, "wtest", Path::new("/tmp"))
            .await
            .unwrap();
        assert!(window_exists(scope, session, "wtest").await.unwrap());
        paste_text(scope, session, "wtest", "evy-test-buf", "echo hi\n")
            .await
            .unwrap();
        kill_window(scope, session, "wtest").await.unwrap();
        assert!(!window_exists(scope, session, "wtest").await.unwrap());
    }
}
