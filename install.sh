#!/usr/bin/env bash
# install.sh — Evy v4 (Rust) installer.
#
# Distributes the v4 daemon binary + skills payload + config + launchd
# plist to the operator's machine. Designed to coexist with v3 Evy
# (label `com.subctl.evy`, currently running on :8787) — v4 ships
# under a distinct label `com.subctl.evy-v4` so both can live on disk.
#
# Default flow:
#   preflight → build → confirm → install files → write plist
#   (DOES NOT launchctl-load — operator does the cutover manually)
#
# `--replace-v3` flow:
#   preflight → build → confirm → install files → unload v3 →
#   write plist → load v4 → health check on :8787
#
# Usage:
#   ./install.sh                  Side-by-side install; v4 written but NOT started
#   ./install.sh --replace-v3     Stop v3, start v4 (irreversible during run)
#   ./install.sh --dry-run        Show every command without executing
#   ./install.sh --yes            Assume yes; non-interactive
#   ./install.sh --help           Print this header
#
# Bash 3.2 compatible (macOS default). No declare -A, no mapfile, no ${var,,}.

set -euo pipefail

# ── locate self ────────────────────────────────────────────────────────────
INSTALL_SH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export SUBCTL_RUST_ROOT="$INSTALL_SH_DIR"

# ── shared helpers + canonical paths ───────────────────────────────────────
# shellcheck disable=SC1091
. "$INSTALL_SH_DIR/scripts/_common.sh"

# ── flags ──────────────────────────────────────────────────────────────────
DRY_RUN=false
ASSUME_YES=false
REPLACE_V3=false
SHOW_HELP=false
SKIP_BUILD=false

for arg in "$@"; do
  case "$arg" in
    --dry-run)      DRY_RUN=true ;;
    --yes|-y)       ASSUME_YES=true ;;
    --replace-v3)   REPLACE_V3=true ;;
    --skip-build)   SKIP_BUILD=true ;;
    -h|--help)      SHOW_HELP=true ;;
    *) subctl_err "unknown arg: $arg"; subctl_err "run --help for usage"; exit 1 ;;
  esac
done
export DRY_RUN ASSUME_YES REPLACE_V3 SKIP_BUILD

if [ "$SHOW_HELP" = "true" ]; then
  # Print the header comment as usage.
  sed -n '2,24p' "$0"
  exit 0
fi

# ── confirm prompt (skipped under --yes / --dry-run) ───────────────────────
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

# ── plan summary (always printed) ──────────────────────────────────────────
print_plan() {
  cat >&2 <<EOF

${SUBCTL_C_BLU}═══ Evy v4 install plan ═══${SUBCTL_C_RST}

  Binary       → $SUBCTL_BIN
  Ink bundle   → $SUBCTL_INK_BUNDLE_PATH
  User launchers → $SUBCTL_USER_BIN_SUBCTL  (+ evy symlink)
  Config       → $SUBCTL_CONFIG_PATH
  Skills dir   → $SUBCTL_SKILLS_DIR
  Log dir      → $SUBCTL_LOG_DIR
  Plist label  → $SUBCTL_PLIST_LABEL
  Plist file   → $SUBCTL_PLIST_PATH
  Health probe → $SUBCTL_HEALTH_URL

  Mode         → $([ "$REPLACE_V3" = "true" ] && echo 'REPLACE v3 (unload com.subctl.evy, load v4)' || echo 'side-by-side (v4 staged but NOT started)')
  Dry-run      → $DRY_RUN
  Assume yes   → $ASSUME_YES

EOF
}

# ── main ───────────────────────────────────────────────────────────────────
main() {
  subctl_step "Evy v4 installer"
  print_plan

  # 1) preflight
  subctl_step "preflight"
  bash "$SUBCTL_RUST_ROOT/scripts/preflight.sh" \
    || { subctl_err "preflight failed — aborting"; exit 1; }

  # 2) build (cargo --release)
  if [ "$SKIP_BUILD" = "true" ]; then
    subctl_info "--skip-build set — assuming target/release/evy already exists"
  else
    subctl_step "build"
    bash "$SUBCTL_RUST_ROOT/scripts/build.sh" \
      || { subctl_err "build failed — aborting"; exit 1; }
  fi

  # 3) confirm gate
  if ! confirm "Proceed with install?"; then
    subctl_warn "aborted by operator"
    exit 0
  fi

  # 4) install steps
  subctl_step "install: binary"
  bash "$SUBCTL_RUST_ROOT/scripts/install_binary.sh" || exit 1

  subctl_step "install: skills"
  bash "$SUBCTL_RUST_ROOT/scripts/install_skills.sh" || exit 1

  subctl_step "install: config"
  bash "$SUBCTL_RUST_ROOT/scripts/install_config.sh" || exit 1

  subctl_step "install: ink TUI"
  bash "$SUBCTL_RUST_ROOT/scripts/install_ink_tui.sh" || exit 1

  subctl_step "install: plist"
  bash "$SUBCTL_RUST_ROOT/scripts/install_plist.sh" || exit 1

  # 5) v3 cutover (gated behind --replace-v3) — health check too
  if [ "$REPLACE_V3" = "true" ]; then
    subctl_step "cutover: unload v3"
    if [ -f "$SUBCTL_V3_PLIST" ]; then
      run "launchctl unload '$SUBCTL_V3_PLIST' 2>/dev/null || true"
      subctl_info "v3 plist unloaded ($SUBCTL_V3_LABEL)"
    else
      subctl_info "no v3 plist on disk ($SUBCTL_V3_PLIST) — nothing to unload"
    fi

    subctl_step "cutover: load v4"
    run "launchctl load '$SUBCTL_PLIST_PATH'"

    subctl_step "cutover: health check"
    if [ "$DRY_RUN" = "true" ]; then
      subctl_info "[dry-run] would curl -s -m 3 $SUBCTL_HEALTH_URL"
    else
      # Give launchd a moment to spawn the process and bind the port.
      sleep 2
      if curl -s -m 3 "$SUBCTL_HEALTH_URL" >/dev/null 2>&1; then
        subctl_ok "v4 health check PASSED ($SUBCTL_HEALTH_URL)"
      else
        subctl_err "v4 health check FAILED — daemon did not respond on :8787"
        subctl_err ""
        subctl_err "Rollback recipe:"
        subctl_err "  launchctl unload $SUBCTL_PLIST_PATH"
        subctl_err "  launchctl load   $SUBCTL_V3_PLIST"
        subctl_err "  tail -n 50 $SUBCTL_LOG_DIR/evy-v4.err.log"
        exit 1
      fi
    fi
  else
    subctl_step "next steps (side-by-side mode)"
    cat >&2 <<EOF

  v4 files are staged but the v4 daemon is NOT running yet.
  v3 ($SUBCTL_V3_LABEL) is UNTOUCHED.

  To cut over manually:
    launchctl unload $SUBCTL_V3_PLIST
    launchctl load   $SUBCTL_PLIST_PATH
    curl -s -m 3 $SUBCTL_HEALTH_URL

  Or re-run with --replace-v3 to perform the swap automatically.

  Rollback at any time:
    launchctl unload $SUBCTL_PLIST_PATH
    launchctl load   $SUBCTL_V3_PLIST

EOF
  fi

  subctl_ok "install complete"
}

main "$@"
