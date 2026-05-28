#!/usr/bin/env bash
# scripts/install_ink_tui.sh — build + stage the v4 chat Ink TUI.
#
# Produces:
#   $SUBCTL_INK_BUNDLE_DIR/bundle.js     ← esbuild output (single-file, ~5MB)
#   $SUBCTL_USER_BIN_DIR/subctl          ← shell launcher in operator PATH
#   $SUBCTL_USER_BIN_DIR/evy → subctl    ← convenience alias symlink
#
# The installed launcher is a copy of bin/subctl with its dev-relative
# BUNDLE= path rewritten to the production install location. Source
# bin/subctl is left untouched so `cd subctl-rust && ./bin/subctl chat`
# keeps working from the repo.
#
# Honors DRY_RUN.

set -euo pipefail

# shellcheck disable=SC1091
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"

UI_DIR="$SUBCTL_RUST_ROOT/ui/chat-tui"
SRC_LAUNCHER="$SUBCTL_RUST_ROOT/bin/subctl"
SRC_BUNDLE="$UI_DIR/dist/bundle.js"

# ── preflight: Node 22+ ────────────────────────────────────────────────────
if ! command -v node >/dev/null 2>&1; then
  subctl_err "node is required (Node 22+). Install via 'brew install node@22'."
  exit 1
fi
NODE_VERSION="$(node --version | sed 's/^v//')"
NODE_MAJOR="${NODE_VERSION%%.*}"
if [ "$NODE_MAJOR" -lt 22 ]; then
  subctl_err "node v$NODE_VERSION found, but v22+ required (Ink 6 + React 19)."
  exit 1
fi
subctl_info "node v$NODE_VERSION ok"

# ── preflight: ui/chat-tui source present ──────────────────────────────────
if [ ! -d "$UI_DIR" ]; then
  subctl_err "ui/chat-tui not found at $UI_DIR"
  subctl_err "are you running install.sh from the subctl-rust repo root?"
  exit 1
fi
if [ ! -f "$SRC_LAUNCHER" ]; then
  subctl_err "bin/subctl launcher not found at $SRC_LAUNCHER"
  exit 1
fi

# ── build: npm install (prod-only) + esbuild bundle ────────────────────────
subctl_info "building chat-tui bundle (npm install + esbuild)…"
if [ "${DRY_RUN:-false}" = "true" ]; then
  subctl_info "[dry-run] (cd '$UI_DIR' && npm install && npm run build)"
else
  # npm install (not --omit=dev) because esbuild is a devDependency and
  # we need it for the build itself. After the build, only dist/bundle.js
  # is shipped — node_modules is left in the repo for next time.
  (cd "$UI_DIR" && npm install --silent --no-audit --no-fund && npm run build) \
    >/dev/null 2>&1 || {
    subctl_err "npm install or build failed — retrying with full output for diagnosis"
    (cd "$UI_DIR" && npm install --no-audit --no-fund && npm run build) || exit 1
  }
  if [ ! -f "$SRC_BUNDLE" ]; then
    subctl_err "build succeeded but $SRC_BUNDLE was not produced"
    exit 1
  fi
  subctl_ok "bundle built: $(du -h "$SRC_BUNDLE" | awk '{print $1}')"
fi

# ── stage: bundle → $SUBCTL_INK_BUNDLE_DIR/bundle.js ───────────────────────
run "mkdir -p '$SUBCTL_INK_BUNDLE_DIR'"
run "install -m 0644 '$SRC_BUNDLE' '$SUBCTL_INK_BUNDLE_PATH'"
subctl_ok "bundle staged: $SUBCTL_INK_BUNDLE_PATH"

# ── stage: launcher with rewritten BUNDLE path ─────────────────────────────
run "mkdir -p '$SUBCTL_USER_BIN_DIR'"

if [ "${DRY_RUN:-false}" = "true" ]; then
  subctl_info "[dry-run] would template bin/subctl → $SUBCTL_USER_BIN_SUBCTL"
  subctl_info "[dry-run]   BUNDLE path rewrite → $SUBCTL_INK_BUNDLE_PATH"
else
  # The dev launcher computes BUNDLE from $0's location. The installed
  # copy hardcodes the production bundle path so it doesn't depend on
  # any sibling directory layout. Sed-replace the BUNDLE= line, install,
  # chmod.
  TMP_LAUNCHER="$(mktemp -t subctl-launcher.XXXXXX)"
  # The dev line is:
  #   BUNDLE="${WORKSPACE_ROOT}/ui/chat-tui/dist/bundle.js"
  # Replace the entire BUNDLE= assignment (and the WORKSPACE_ROOT one
  # above it, which becomes meaningless once BUNDLE is hardcoded).
  sed \
    -e 's|^WORKSPACE_ROOT=.*|WORKSPACE_ROOT="" # unused in installed copy|' \
    -e "s|^BUNDLE=.*|BUNDLE=\"$SUBCTL_INK_BUNDLE_PATH\"|" \
    "$SRC_LAUNCHER" > "$TMP_LAUNCHER"
  install -m 0755 "$TMP_LAUNCHER" "$SUBCTL_USER_BIN_SUBCTL"
  rm -f "$TMP_LAUNCHER"
fi
subctl_ok "launcher installed: $SUBCTL_USER_BIN_SUBCTL"

# ── symlink: evy → subctl ──────────────────────────────────────────────────
# Use `ln -sf` (not -sFf) — macOS ln accepts -f for "remove existing"
# without the GNU -F flag. If $SUBCTL_USER_BIN_EVY is a regular file
# from a prior unrelated install, we leave it alone and warn.
if [ -e "$SUBCTL_USER_BIN_EVY" ] && [ ! -L "$SUBCTL_USER_BIN_EVY" ]; then
  subctl_warn "$SUBCTL_USER_BIN_EVY exists and is NOT a symlink — leaving untouched"
  subctl_warn "  remove it manually if you want 'evy' to alias 'subctl'"
else
  run "ln -sf '$SUBCTL_USER_BIN_SUBCTL' '$SUBCTL_USER_BIN_EVY'"
  subctl_ok "alias symlinked: $SUBCTL_USER_BIN_EVY → subctl"
fi

# ── PATH check (informational, doesn't fail) ───────────────────────────────
case ":$PATH:" in
  *":$SUBCTL_USER_BIN_DIR:"*)
    : # already on PATH
    ;;
  *)
    subctl_warn "$SUBCTL_USER_BIN_DIR is NOT on your PATH"
    subctl_warn "  add to ~/.zshrc:  export PATH=\"\$HOME/.local/bin:\$PATH\""
    ;;
esac

subctl_ok "ink TUI install complete"
