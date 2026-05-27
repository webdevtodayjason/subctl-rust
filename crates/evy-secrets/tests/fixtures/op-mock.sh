#!/usr/bin/env bash
# Mock of the 1Password `op` CLI for evy-secrets integration tests.
#
# Behavior:
#   - Recognizes the `read <op://ref>` invocation that
#     OnePasswordBackend uses; ignores everything else.
#   - Verifies the OP_SERVICE_ACCOUNT_TOKEN env var is set (because
#     that is the whole interaction contract under test); exits 1 if
#     unset, mirroring `op`'s own behavior.
#   - For known refs, prints a deterministic value to stdout (with
#     the trailing newline `op read` always emits) and exits 0.
#   - For unknown refs, exits 1 with no output — the resolver should
#     interpret that as "not found, try the next backend".
#
# The fixture is copied into a per-test tempdir as `op`, chmod'd
# 0755, and prepended to PATH; tests should never run against the
# operator's real `op` binary.

set -u

if [[ "${1:-}" != "read" ]]; then
  echo "op-mock: unexpected subcommand: ${1:-<none>}" >&2
  exit 2
fi

REF="${2:-}"
if [[ -z "$REF" ]]; then
  echo "op-mock: read requires a reference" >&2
  exit 2
fi

if [[ -z "${OP_SERVICE_ACCOUNT_TOKEN:-}" ]]; then
  echo "op-mock: OP_SERVICE_ACCOUNT_TOKEN not set" >&2
  exit 1
fi

case "$REF" in
  op://Engineering/openai/api-key)
    # Standard happy path: known ref → known value.
    echo "sk-mock-engineering-openai"
    exit 0
    ;;
  op://Engineering/blank-item)
    # Empty stdout but exit 0 — backend should treat as Ok(None).
    exit 0
    ;;
  op://Engineering/echo-token)
    # Echoes the service account token (so a test can confirm the
    # env var was actually threaded through to the child process).
    # The token itself is a fake value supplied by the test.
    echo "$OP_SERVICE_ACCOUNT_TOKEN"
    exit 0
    ;;
  *)
    # Unknown ref → non-zero exit. Real `op` writes a message; we
    # keep stderr quiet to make test output readable.
    exit 1
    ;;
esac
