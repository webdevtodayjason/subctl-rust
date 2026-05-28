#!/usr/bin/env bash
# scripts/build.sh — cargo build --release.
#
# Skipped at install.sh level when --skip-build is set. This script
# always attempts a release build when invoked.
#
# Honors DRY_RUN: prints the command without running it.

set -euo pipefail

# shellcheck disable=SC1091
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"

cd "$SUBCTL_RUST_ROOT"

# If the binary is fresh (newer than every Cargo.toml in workspace), skip.
if [ -x "$SUBCTL_RUST_ROOT/target/release/evy" ] && [ "${FORCE_BUILD:-false}" != "true" ]; then
  # The simplest staleness check that works across bash 3.2 — compare against
  # the workspace root Cargo.toml. Not perfect, but good enough for an
  # operator-driven re-run.
  if [ "$SUBCTL_RUST_ROOT/target/release/evy" -nt "$SUBCTL_RUST_ROOT/Cargo.toml" ]; then
    subctl_info "target/release/evy is newer than Cargo.toml — skipping build"
    subctl_info "  set FORCE_BUILD=true to override"
    exit 0
  fi
fi

subctl_info "cargo build --release -p evy"
run "cargo build --release -p evy --manifest-path '$SUBCTL_RUST_ROOT/Cargo.toml'"

if [ "${DRY_RUN:-false}" != "true" ]; then
  if [ ! -x "$SUBCTL_RUST_ROOT/target/release/evy" ]; then
    subctl_err "build completed but target/release/evy is missing"
    exit 1
  fi
  subctl_ok "build: target/release/evy"
fi
