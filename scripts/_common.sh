#!/usr/bin/env bash
# scripts/_common.sh — shared helpers + canonical paths.
#
# Sourced by install.sh AND every script in scripts/. Has no side
# effects when sourced (no `main`, no top-level work).
#
# Bash 3.2 compatible. Sourcing twice is safe (guarded by SUBCTL_LOG_LOADED).

if [ -z "${SUBCTL_LOG_LOADED:-}" ]; then
  SUBCTL_LOG_LOADED=1

  if [ -t 2 ]; then
    SUBCTL_C_RED=$'\033[0;31m'
    SUBCTL_C_GRN=$'\033[0;32m'
    SUBCTL_C_YEL=$'\033[0;33m'
    SUBCTL_C_BLU=$'\033[0;34m'
    SUBCTL_C_DIM=$'\033[2m'
    SUBCTL_C_RST=$'\033[0m'
  else
    SUBCTL_C_RED=""; SUBCTL_C_GRN=""; SUBCTL_C_YEL=""
    SUBCTL_C_BLU=""; SUBCTL_C_DIM=""; SUBCTL_C_RST=""
  fi
  export SUBCTL_C_RED SUBCTL_C_GRN SUBCTL_C_YEL SUBCTL_C_BLU SUBCTL_C_DIM SUBCTL_C_RST

  subctl_info() { printf '%s[info]%s %s\n' "$SUBCTL_C_BLU" "$SUBCTL_C_RST" "$*" >&2; }
  subctl_ok()   { printf '%s[ ok ]%s %s\n' "$SUBCTL_C_GRN" "$SUBCTL_C_RST" "$*" >&2; }
  subctl_warn() { printf '%s[warn]%s %s\n' "$SUBCTL_C_YEL" "$SUBCTL_C_RST" "$*" >&2; }
  subctl_err()  { printf '%s[err ]%s %s\n' "$SUBCTL_C_RED" "$SUBCTL_C_RST" "$*" >&2; }
  subctl_step() { printf '\n%s── %s ──%s\n' "$SUBCTL_C_DIM" "$*" "$SUBCTL_C_RST" >&2; }

  # `run` honors DRY_RUN — prints what it would do without executing.
  # Use this for every state-mutating command in sub-scripts.
  run() {
    if [ "${DRY_RUN:-false}" = "true" ]; then
      printf '%s[dry-run]%s %s\n' "$SUBCTL_C_DIM" "$SUBCTL_C_RST" "$*" >&2
    else
      eval "$@"
    fi
  }
fi

# ── canonical install paths (single source of truth) ───────────────────────
# Sub-scripts read these directly. Operators can override SUBCTL_PREFIX
# etc. before invoking install.sh; we won't clobber.
: "${SUBCTL_PREFIX:=$HOME/.local/lib/subctl-rust}"
: "${SUBCTL_BIN:=$SUBCTL_PREFIX/evy}"
# Ink TUI bundle lives next to the daemon binary, under $SUBCTL_PREFIX.
: "${SUBCTL_INK_BUNDLE_DIR:=$SUBCTL_PREFIX/ui}"
: "${SUBCTL_INK_BUNDLE_PATH:=$SUBCTL_INK_BUNDLE_DIR/bundle.js}"
# User-facing launchers (subctl + evy alias) drop into the operator's PATH.
: "${SUBCTL_USER_BIN_DIR:=$HOME/.local/bin}"
: "${SUBCTL_USER_BIN_SUBCTL:=$SUBCTL_USER_BIN_DIR/subctl}"
: "${SUBCTL_USER_BIN_EVY:=$SUBCTL_USER_BIN_DIR/evy}"
# v4 lives in its OWN config subdir to avoid collisions with v3's state
# at $HOME/.config/subctl/ (config.toml, accounts.conf, cognee.json, evy/, …).
# Parallel-test mode: both v3 and v4 daemons can run simultaneously
# because their state is fully isolated.
: "${SUBCTL_CONFIG_DIR:=$HOME/.config/subctl/v4}"
: "${SUBCTL_CONFIG_PATH:=$SUBCTL_CONFIG_DIR/config.toml}"
: "${SUBCTL_SKILLS_DIR:=$SUBCTL_CONFIG_DIR/skills}"
: "${SUBCTL_LOG_DIR:=$HOME/Library/Logs/subctl}"
: "${SUBCTL_PLIST_LABEL:=com.subctl.evy-v4}"
: "${SUBCTL_PLIST_PATH:=$HOME/Library/LaunchAgents/${SUBCTL_PLIST_LABEL}.plist}"
: "${SUBCTL_V3_LABEL:=com.subctl.evy}"
: "${SUBCTL_V3_PLIST:=$HOME/Library/LaunchAgents/${SUBCTL_V3_LABEL}.plist}"
# v4 binds to 8797 by default so it can run side-by-side with v3 on 8787.
# Cutover step (later): edit the rendered config.toml to set port = 8787
# AND stop v3 first.
: "${SUBCTL_HEALTH_URL:=http://127.0.0.1:8797/health}"

# Set by the parent install.sh; default to repo root relative to this file.
: "${SUBCTL_RUST_ROOT:=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"

export SUBCTL_PREFIX SUBCTL_BIN SUBCTL_CONFIG_DIR SUBCTL_CONFIG_PATH \
       SUBCTL_SKILLS_DIR SUBCTL_LOG_DIR SUBCTL_PLIST_LABEL \
       SUBCTL_PLIST_PATH SUBCTL_V3_LABEL SUBCTL_V3_PLIST \
       SUBCTL_HEALTH_URL SUBCTL_RUST_ROOT \
       SUBCTL_INK_BUNDLE_DIR SUBCTL_INK_BUNDLE_PATH \
       SUBCTL_USER_BIN_DIR SUBCTL_USER_BIN_SUBCTL SUBCTL_USER_BIN_EVY
