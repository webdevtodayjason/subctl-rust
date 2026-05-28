#!/usr/bin/env bash
# scripts/install_binary.sh — copy target/release/evy to $SUBCTL_BIN.
#
# Idempotent. Overwrites prior $SUBCTL_BIN with no backup — the bits
# are reproducible from `cargo build --release`. Skips if source ==
# dest and contents match.
#
# Honors DRY_RUN.

set -euo pipefail

# shellcheck disable=SC1091
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"

SRC="$SUBCTL_RUST_ROOT/target/release/evy"
DEST="$SUBCTL_BIN"

if [ "${DRY_RUN:-false}" != "true" ] && [ ! -x "$SRC" ]; then
  subctl_err "source binary missing: $SRC"
  subctl_err "  run scripts/build.sh first (or omit --skip-build from install.sh)"
  exit 1
fi

subctl_info "src  → $SRC"
subctl_info "dest → $DEST"

run "mkdir -p '$SUBCTL_PREFIX'"
run "install -m 0755 '$SRC' '$DEST'"

# Sanity check (skipped under dry-run).
if [ "${DRY_RUN:-false}" != "true" ]; then
  if [ ! -x "$DEST" ]; then
    subctl_err "post-install: $DEST is not executable"
    exit 1
  fi
  subctl_ok "binary installed: $DEST"
fi
