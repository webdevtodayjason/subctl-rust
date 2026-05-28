#!/usr/bin/env bash
# scripts/install_plist.sh — render launchd/com.subctl.evy-v4.plist
# and write to ~/Library/LaunchAgents/.
#
# Substitutes:
#   {{INSTALL_DIR}}  → $SUBCTL_PREFIX
#   {{CONFIG_PATH}}  → $SUBCTL_CONFIG_PATH
#   {{WORKING_DIR}}  → $SUBCTL_CONFIG_DIR
#   {{LOG_DIR}}      → $SUBCTL_LOG_DIR
#
# All values are absolute paths — launchd does NOT expand `~` or env
# vars in plist string values.
#
# Idempotent: if the existing plist on disk is byte-identical to what
# we'd write, the file is left alone (preserves mtime, avoids
# unnecessary launchctl churn). Otherwise the old file is backed up
# to .bak.<ts> before overwrite.
#
# Does NOT `launchctl load` — that's gated behind --replace-v3 in
# install.sh's main flow.
#
# Honors DRY_RUN: prints the rendered plist to stderr, writes nothing.

set -euo pipefail

# shellcheck disable=SC1091
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"

TEMPLATE="$SUBCTL_RUST_ROOT/launchd/com.subctl.evy-v4.plist"

if [ ! -f "$TEMPLATE" ]; then
  subctl_err "plist template missing: $TEMPLATE"
  exit 1
fi

# Render to a temp file using sed with `|` delimiter (paths contain `/`).
TMP_RENDER="$(mktemp -t subctl-evy-v4-plist.XXXXXX)"
sed \
  -e "s|{{INSTALL_DIR}}|$SUBCTL_PREFIX|g" \
  -e "s|{{CONFIG_PATH}}|$SUBCTL_CONFIG_PATH|g" \
  -e "s|{{WORKING_DIR}}|$SUBCTL_CONFIG_DIR|g" \
  -e "s|{{LOG_DIR}}|$SUBCTL_LOG_DIR|g" \
  "$TEMPLATE" > "$TMP_RENDER"

if [ "${DRY_RUN:-false}" = "true" ]; then
  subctl_info "[dry-run] would write plist → $SUBCTL_PLIST_PATH"
  printf '\n%s──── rendered plist ────%s\n' "$SUBCTL_C_DIM" "$SUBCTL_C_RST" >&2
  sed 's/^/    /' "$TMP_RENDER" >&2
  printf '%s───────────────────────%s\n\n' "$SUBCTL_C_DIM" "$SUBCTL_C_RST" >&2
  rm -f "$TMP_RENDER"
  exit 0
fi

run "mkdir -p '$HOME/Library/LaunchAgents'"
run "mkdir -p '$SUBCTL_LOG_DIR'"

# Idempotency: byte-compare against existing.
if [ -f "$SUBCTL_PLIST_PATH" ]; then
  if cmp -s "$TMP_RENDER" "$SUBCTL_PLIST_PATH"; then
    subctl_info "plist unchanged ($SUBCTL_PLIST_PATH) — no write"
    rm -f "$TMP_RENDER"
    exit 0
  fi
  BACKUP_TS="$(date -u +%Y%m%dT%H%M%SZ)"
  BACKUP_PATH="${SUBCTL_PLIST_PATH}.bak-${BACKUP_TS}"
  cp "$SUBCTL_PLIST_PATH" "$BACKUP_PATH"
  subctl_info "backed up existing plist → $BACKUP_PATH"
fi

install -m 0644 "$TMP_RENDER" "$SUBCTL_PLIST_PATH"
rm -f "$TMP_RENDER"

# Validate via plutil — Apple's official plist syntax check. Best-effort.
if command -v plutil >/dev/null 2>&1; then
  if ! plutil -lint "$SUBCTL_PLIST_PATH" >/dev/null 2>&1; then
    subctl_err "plutil rejected the rendered plist:"
    plutil -lint "$SUBCTL_PLIST_PATH" >&2 || true
    exit 1
  fi
fi

subctl_ok "plist written: $SUBCTL_PLIST_PATH"
subctl_info "  label: $SUBCTL_PLIST_LABEL"
subctl_info "  (NOT loaded yet — install.sh main flow gates load behind --replace-v3)"
