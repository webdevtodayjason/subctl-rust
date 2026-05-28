#!/usr/bin/env bash
# tests/install_dryrun.sh — verify install.sh --dry-run is safe.
#
# Asserts:
#   1. install.sh --dry-run exits 0
#   2. install.sh --help exits 0
#   3. uninstall.sh --dry-run exits 0
#   4. Plan output mentions the v4 plist label
#   5. No actual file was created at $SUBCTL_PLIST_PATH
#   6. In DEFAULT mode (not --replace-v3), the dry-run does NOT plan to
#      `launchctl unload` v3 — the v3 daemon is untouched
#   7. In --replace-v3 mode, the dry-run DOES plan the v3 unload
#
# Run manually:
#   bash tests/install_dryrun.sh
#
# This is NOT a cargo test — it's a bash smoke harness invoked by the
# operator to validate the installer before committing the real run.

set -euo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$TEST_DIR/.." && pwd)"

# Use a sandbox HOME so the test never touches the operator's real
# LaunchAgents / config. This is critical — without it, a buggy
# dry-run that accidentally executed would clobber v3.
SANDBOX="$(mktemp -d -t subctl-installer-test.XXXXXX)"
trap 'rm -rf "$SANDBOX"' EXIT
export HOME="$SANDBOX"
mkdir -p "$SANDBOX/Library/LaunchAgents"
mkdir -p "$SANDBOX/Library/Logs"
mkdir -p "$SANDBOX/.config"

PASS=0
FAIL=0

green() { printf '\033[0;32m%s\033[0m\n' "$*"; }
red()   { printf '\033[0;31m%s\033[0m\n' "$*"; }

assert_ok() {
  local label="$1"
  if [ "$2" = "true" ]; then
    green "  PASS  $label"
    PASS=$((PASS + 1))
  else
    red "  FAIL  $label"
    FAIL=$((FAIL + 1))
  fi
}

echo "== tests/install_dryrun.sh =="
echo "  ROOT     = $ROOT"
echo "  SANDBOX  = $SANDBOX  (sandbox HOME for the test)"
echo

# ── 1. install.sh --dry-run exits 0 ────────────────────────────────────────
echo "[1] install.sh --dry-run --yes --skip-build"
DRY_OUT="$(bash "$ROOT/install.sh" --dry-run --yes --skip-build 2>&1)"
RC=$?
assert_ok "exit 0 from --dry-run" "$( [ $RC -eq 0 ] && echo true || echo false )"

# ── 4. plan mentions v4 label ──────────────────────────────────────────────
echo "$DRY_OUT" | grep -q 'com.subctl.evy-v4'
assert_ok "plan mentions com.subctl.evy-v4" "$( [ $? -eq 0 ] && echo true || echo false )"

# ── 5. no real plist was written ───────────────────────────────────────────
if [ ! -f "$SANDBOX/Library/LaunchAgents/com.subctl.evy-v4.plist" ]; then
  assert_ok "no plist written to sandbox" "true"
else
  assert_ok "no plist written to sandbox" "false"
fi

# ── 6. default mode does NOT plan to unload v3 ─────────────────────────────
# Look for active `launchctl unload com.subctl.evy.plist` lines (dry-run
# prints them with [dry-run] prefix). Side-by-side default should NOT
# emit one.
if echo "$DRY_OUT" | grep -E '\[dry-run\].*launchctl unload.*com\.subctl\.evy\.plist' >/dev/null 2>&1; then
  assert_ok "default mode does NOT unload v3" "false"
else
  assert_ok "default mode does NOT unload v3" "true"
fi

# ── 7. --replace-v3 mode DOES plan to unload v3 ────────────────────────────
echo
echo "[2] install.sh --dry-run --yes --skip-build --replace-v3"
# Seed a fake v3 plist in the sandbox so the unload branch fires.
cat > "$SANDBOX/Library/LaunchAgents/com.subctl.evy.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>Label</key><string>com.subctl.evy</string></dict></plist>
EOF
REPLACE_OUT="$(bash "$ROOT/install.sh" --dry-run --yes --skip-build --replace-v3 2>&1)"
RC=$?
assert_ok "exit 0 from --replace-v3 --dry-run" "$( [ $RC -eq 0 ] && echo true || echo false )"

if echo "$REPLACE_OUT" | grep -E '\[dry-run\].*launchctl unload.*com\.subctl\.evy\.plist' >/dev/null 2>&1; then
  assert_ok "--replace-v3 mode plans v3 unload" "true"
else
  assert_ok "--replace-v3 mode plans v3 unload" "false"
fi

# ── 8. install.sh --help exits 0 ───────────────────────────────────────────
echo
echo "[3] install.sh --help"
bash "$ROOT/install.sh" --help >/dev/null 2>&1
assert_ok "exit 0 from --help" "$( [ $? -eq 0 ] && echo true || echo false )"

# ── 9. uninstall.sh --dry-run exits 0 ──────────────────────────────────────
echo
echo "[4] uninstall.sh --dry-run --yes"
bash "$ROOT/uninstall.sh" --dry-run --yes >/dev/null 2>&1
assert_ok "exit 0 from uninstall --dry-run" "$( [ $? -eq 0 ] && echo true || echo false )"

# ── 10. uninstall.sh --help ────────────────────────────────────────────────
bash "$ROOT/uninstall.sh" --help >/dev/null 2>&1
assert_ok "exit 0 from uninstall --help" "$( [ $? -eq 0 ] && echo true || echo false )"

echo
echo "── summary ──"
echo "  PASS: $PASS"
echo "  FAIL: $FAIL"
if [ $FAIL -eq 0 ]; then
  green "OK"
  exit 0
else
  red "FAILED"
  exit 1
fi
