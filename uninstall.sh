#!/usr/bin/env bash
# uninstall.sh — reverse install.sh.
#
# Default (state-preserving):
#   - launchctl unload + remove $SUBCTL_PLIST_PATH
#   - remove $SUBCTL_PREFIX (binary)
#   - KEEP $SUBCTL_CONFIG_DIR (config, skills, observation log) intact
#
# --purge:
#   - everything above, PLUS
#   - remove $SUBCTL_CONFIG_DIR (config, skills, *.db files)
#   - remove $SUBCTL_LOG_DIR/evy-v4*.log
#
# v3 is NEVER touched. com.subctl.evy.plist stays as-is.
#
# Usage:
#   ./uninstall.sh                Default: remove v4 daemon, keep state
#   ./uninstall.sh --purge        Also wipe config + skills + state
#   ./uninstall.sh --dry-run      Show every command without executing
#   ./uninstall.sh --yes          Assume yes
#   ./uninstall.sh --help

set -euo pipefail

UNINSTALL_SH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export SUBCTL_RUST_ROOT="$UNINSTALL_SH_DIR"

# shellcheck disable=SC1091
. "$UNINSTALL_SH_DIR/scripts/_common.sh"

# ── flags ──────────────────────────────────────────────────────────────────
DRY_RUN=false
ASSUME_YES=false
PURGE=false
SHOW_HELP=false

for arg in "$@"; do
  case "$arg" in
    --dry-run)  DRY_RUN=true ;;
    --yes|-y)   ASSUME_YES=true ;;
    --purge)    PURGE=true ;;
    -h|--help)  SHOW_HELP=true ;;
    *) subctl_err "unknown arg: $arg"; exit 1 ;;
  esac
done
export DRY_RUN

if [ "$SHOW_HELP" = "true" ]; then
  sed -n '2,21p' "$0"
  exit 0
fi

confirm() {
  local prompt="$1"
  if [ "$ASSUME_YES" = "true" ] || [ "$DRY_RUN" = "true" ]; then
    subctl_info "auto-confirm: $prompt → yes"
    return 0
  fi
  printf '%s [y/N]: ' "$prompt" >&2
  local reply=""
  read -r reply || reply=""
  case "$reply" in
    y|Y|yes|YES) return 0 ;;
    *) return 1 ;;
  esac
}

# ── plan ───────────────────────────────────────────────────────────────────
subctl_step "Evy v4 uninstall plan"
cat >&2 <<EOF

  Plist file   → $SUBCTL_PLIST_PATH  (will REMOVE)
  Binary       → $SUBCTL_PREFIX       (will REMOVE — includes ui/ bundle)
  Launchers    → $SUBCTL_USER_BIN_SUBCTL  (will REMOVE)
               → $SUBCTL_USER_BIN_EVY     (will REMOVE — symlink)

EOF

if [ "$PURGE" = "true" ]; then
  cat >&2 <<EOF
  Config       → $SUBCTL_CONFIG_DIR  (will REMOVE — --purge)
  Logs         → $SUBCTL_LOG_DIR/evy-v4*.log  (will REMOVE — --purge)

  ${SUBCTL_C_YEL}--purge will delete your config, skills, observation log,
  scores DB, preferences DB, and playbooks directory.${SUBCTL_C_RST}

EOF
else
  cat >&2 <<EOF
  Config       → $SUBCTL_CONFIG_DIR  (PRESERVED)
  Logs         → $SUBCTL_LOG_DIR     (PRESERVED)

EOF
fi

cat >&2 <<EOF
  v3 ($SUBCTL_V3_LABEL) → UNTOUCHED

EOF

if ! confirm "Proceed with uninstall?"; then
  subctl_warn "aborted by operator"
  exit 0
fi

# ── 1. unload + remove plist ──────────────────────────────────────────────
subctl_step "remove plist"
if [ -f "$SUBCTL_PLIST_PATH" ]; then
  # Best-effort unload — may not be loaded.
  run "launchctl unload '$SUBCTL_PLIST_PATH' 2>/dev/null || true"
  run "rm -f '$SUBCTL_PLIST_PATH'"
  subctl_ok "plist removed: $SUBCTL_PLIST_PATH"
else
  subctl_info "no plist at $SUBCTL_PLIST_PATH — already gone"
fi

# ── 2. remove binary tree ──────────────────────────────────────────────────
subctl_step "remove binary"
if [ -d "$SUBCTL_PREFIX" ]; then
  # Safety: only allow removal under $HOME/.local/lib/subctl-rust.
  case "$SUBCTL_PREFIX" in
    "$HOME"/.local/lib/subctl-rust*)
      run "rm -rf '$SUBCTL_PREFIX'"
      subctl_ok "binary tree removed: $SUBCTL_PREFIX"
      ;;
    *)
      subctl_err "refusing to rm -rf '$SUBCTL_PREFIX' (path not under \$HOME/.local/lib/subctl-rust)"
      exit 1
      ;;
  esac
else
  subctl_info "no binary tree at $SUBCTL_PREFIX — already gone"
fi

# ── 3. remove user-PATH launchers (subctl + evy symlink) ──────────────────
subctl_step "remove launchers"
for launcher in "$SUBCTL_USER_BIN_SUBCTL" "$SUBCTL_USER_BIN_EVY"; do
  if [ -e "$launcher" ] || [ -L "$launcher" ]; then
    # Safety: only remove paths we control under $HOME/.local/bin/.
    case "$launcher" in
      "$HOME"/.local/bin/subctl|"$HOME"/.local/bin/evy)
        run "rm -f '$launcher'"
        subctl_ok "launcher removed: $launcher"
        ;;
      *)
        subctl_err "refusing to rm '$launcher' (path not under \$HOME/.local/bin/)"
        ;;
    esac
  else
    subctl_info "no launcher at $launcher — already gone"
  fi
done

# ── 4. purge (optional) ────────────────────────────────────────────────────
if [ "$PURGE" = "true" ]; then
  subctl_step "purge: config + logs"
  if ! confirm "Really delete $SUBCTL_CONFIG_DIR and v4 logs?"; then
    subctl_warn "purge aborted — state preserved"
    exit 0
  fi
  case "$SUBCTL_CONFIG_DIR" in
    "$HOME"/.config/subctl*)
      run "rm -rf '$SUBCTL_CONFIG_DIR'"
      subctl_ok "config removed: $SUBCTL_CONFIG_DIR"
      ;;
    *)
      subctl_err "refusing to rm -rf '$SUBCTL_CONFIG_DIR' (path not under \$HOME/.config/subctl)"
      ;;
  esac
  # Only v4-prefixed logs — leave v3's evy.log alone.
  run "rm -f '$SUBCTL_LOG_DIR'/evy-v4.log '$SUBCTL_LOG_DIR'/evy-v4.err.log"
  subctl_ok "v4 logs removed"
fi

subctl_ok "uninstall complete"
echo >&2
echo "  v3 is untouched. To resume v3:" >&2
echo "    launchctl load $SUBCTL_V3_PLIST" >&2
echo "    curl -s -m 3 $SUBCTL_HEALTH_URL" >&2
echo >&2
