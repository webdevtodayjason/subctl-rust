#!/usr/bin/env bash
# scripts/preflight.sh — environment checks before any mutation.
#
# Bails on any HARD failure; emits WARN on soft issues that operator
# can decide about. Inherits DRY_RUN/REPLACE_V3 etc. from install.sh.
#
# Checks:
#   - OS is Darwin (macOS) — HARD
#   - macOS 14 (Sonoma) or newer — WARN (untested below)
#   - cargo on PATH — HARD (unless --skip-build)
#   - free disk space ≥ 500MB at $HOME — WARN if below
#   - port 8787 (Evy HTTP) — WARN; allowed if held by v3 evy
#   - port 8788 / 8789 (reserved + TTS) — INFO; informational only
#   - launchctl is callable — HARD on macOS

set -euo pipefail

# shellcheck disable=SC1091
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"

FAIL=0
WARN_COUNT=0

# --- OS ---
OS="$(uname -s)"
if [ "$OS" != "Darwin" ]; then
  subctl_err "OS is '$OS' — Evy v4 installer targets macOS (Darwin) only"
  subctl_err "Linux deployment is a separate distribution path"
  FAIL=$((FAIL + 1))
else
  subctl_ok "OS: Darwin"
fi

# --- macOS version ---
if [ "$OS" = "Darwin" ]; then
  MACOS_VER="$(sw_vers -productVersion 2>/dev/null || echo unknown)"
  case "$MACOS_VER" in
    14.*|15.*|16.*|17.*|18.*|19.*|2*)
      subctl_ok "macOS: $MACOS_VER"
      ;;
    *)
      subctl_warn "macOS: $MACOS_VER — Evy v4 has been smoked on Sonoma (14)+ only"
      WARN_COUNT=$((WARN_COUNT + 1))
      ;;
  esac
fi

# --- launchctl ---
if [ "$OS" = "Darwin" ]; then
  if command -v launchctl >/dev/null 2>&1; then
    subctl_ok "launchctl: available"
  else
    subctl_err "launchctl missing — cannot install LaunchAgent"
    FAIL=$((FAIL + 1))
  fi
fi

# --- cargo (skipped if --skip-build) ---
if [ "${SKIP_BUILD:-false}" = "true" ]; then
  subctl_info "skipping cargo check (--skip-build)"
else
  if command -v cargo >/dev/null 2>&1; then
    CARGO_VER="$(cargo --version 2>/dev/null | head -1 || echo unknown)"
    subctl_ok "cargo: $CARGO_VER"
  else
    subctl_err "cargo not on PATH — install rustup first:"
    subctl_err "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    FAIL=$((FAIL + 1))
  fi
fi

# --- disk space at $HOME ---
# df -k returns kilobytes; column 4 (Available) on macOS.
if [ -d "$HOME" ]; then
  DF_LINE="$(df -k "$HOME" | awk 'NR==2 {print $4}')"
  if [ -n "$DF_LINE" ] && [ "$DF_LINE" -lt 524288 ]; then
    subctl_warn "free disk at \$HOME: $((DF_LINE / 1024)) MB (recommended: 500 MB+)"
    WARN_COUNT=$((WARN_COUNT + 1))
  else
    subctl_ok "free disk at \$HOME: $((DF_LINE / 1024)) MB"
  fi
fi

# --- port 8787 (v3 Evy HTTP — informational) ---
PORT_HOLDER=""
if command -v lsof >/dev/null 2>&1; then
  PORT_HOLDER="$(lsof -nP -iTCP:8787 -sTCP:LISTEN 2>/dev/null | awk 'NR==2 {print $1, $2}' || true)"
fi
if [ -z "$PORT_HOLDER" ]; then
  subctl_info "port 8787: free (v3 not running)"
else
  V3_RUNNING="$(launchctl list 2>/dev/null | awk -v lbl="$SUBCTL_V3_LABEL" '$3 == lbl {print $1}' || true)"
  if [ -n "$V3_RUNNING" ]; then
    subctl_info "port 8787 held by v3 evy (PID $V3_RUNNING) — expected for side-by-side"
  else
    subctl_info "port 8787 held by an unrelated process: $PORT_HOLDER (informational; v4 binds 8797)"
  fi
fi

# --- port 8797 (v4 Evy HTTP — the port v4 WILL bind) ---
V4_PORT_HOLDER=""
if command -v lsof >/dev/null 2>&1; then
  V4_PORT_HOLDER="$(lsof -nP -iTCP:8797 -sTCP:LISTEN 2>/dev/null | awk 'NR==2 {print $1, $2}' || true)"
fi
if [ -z "$V4_PORT_HOLDER" ]; then
  subctl_ok "port 8797: free (v4 will bind here)"
else
  subctl_warn "port 8797 held by: $V4_PORT_HOLDER"
  subctl_warn "v4 will fail to bind. Free the port or edit config.toml after install."
  WARN_COUNT=$((WARN_COUNT + 1))
fi

# --- ports 8788 (reserved) and 8789 (TTS) — informational ---
if command -v lsof >/dev/null 2>&1; then
  for PORT in 8788 8789; do
    HOLDER="$(lsof -nP -iTCP:$PORT -sTCP:LISTEN 2>/dev/null | awk 'NR==2 {print $1, $2}' || true)"
    if [ -n "$HOLDER" ]; then
      subctl_info "port $PORT: in use by $HOLDER (informational)"
    fi
  done
fi

# --- summary ---
if [ "$FAIL" -gt 0 ]; then
  subctl_err "preflight: $FAIL hard failure(s), $WARN_COUNT warning(s)"
  exit 1
fi
subctl_ok "preflight: passed ($WARN_COUNT warning(s))"
exit 0
