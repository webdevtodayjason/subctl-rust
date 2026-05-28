#!/usr/bin/env bash
# scripts/install_skills.sh — rsync skills/ → $SUBCTL_SKILLS_DIR.
#
# Phase 4 Slice G shipped the `evy-skills` crate (the routing logic)
# but NOT yet a `skills/` payload (the markdown skills themselves).
# This script:
#   - creates $SUBCTL_SKILLS_DIR if missing (always)
#   - if $SUBCTL_RUST_ROOT/skills/ exists, rsyncs it
#   - otherwise logs that there's no payload (graceful no-op)
#
# Idempotent. Honors DRY_RUN.

set -euo pipefail

# shellcheck disable=SC1091
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"

SRC_DIR="$SUBCTL_RUST_ROOT/skills"
DEST_DIR="$SUBCTL_SKILLS_DIR"

run "mkdir -p '$DEST_DIR'"

if [ ! -d "$SRC_DIR" ]; then
  subctl_info "no skills payload at $SRC_DIR (evy-skills crate is currently routing-only)"
  subctl_info "  destination $DEST_DIR ensured; skills can be dropped in later"
  exit 0
fi

# rsync if available (preserves perms, idempotent, deletes nothing).
if command -v rsync >/dev/null 2>&1; then
  subctl_info "rsync $SRC_DIR/ → $DEST_DIR/"
  run "rsync -a --no-owner --no-group '$SRC_DIR/' '$DEST_DIR/'"
else
  subctl_info "rsync missing — falling back to cp -R"
  run "cp -R '$SRC_DIR/.' '$DEST_DIR/'"
fi

if [ "${DRY_RUN:-false}" != "true" ]; then
  COUNT="$(find "$DEST_DIR" -name '*.md' 2>/dev/null | wc -l | tr -d ' ')"
  subctl_ok "skills installed: $COUNT markdown file(s) at $DEST_DIR"
fi
