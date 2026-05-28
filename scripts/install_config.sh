#!/usr/bin/env bash
# scripts/install_config.sh — seed $SUBCTL_CONFIG_PATH if not present.
#
# Critically: NEVER overwrites an existing config. Operator may have
# hand-edited it, or v3's config may live at the same path. If the
# file exists, we leave it alone and log so.
#
# The seeded config differs from the dev-flavored config/evy.toml in
# the repo — that one uses /tmp/* DB paths (great for tests, awful
# for a daemon). We render production paths under $SUBCTL_CONFIG_DIR.
#
# Honors DRY_RUN.

set -euo pipefail

# shellcheck disable=SC1091
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"

run "mkdir -p '$SUBCTL_CONFIG_DIR'"
run "mkdir -p '$SUBCTL_CONFIG_DIR/playbooks'"
run "mkdir -p '$SUBCTL_LOG_DIR'"

# Refuse to overwrite an operator-edited file.
if [ -e "$SUBCTL_CONFIG_PATH" ] && [ "${DRY_RUN:-false}" != "true" ]; then
  subctl_info "config already exists at $SUBCTL_CONFIG_PATH — leaving untouched"
  subctl_info "  (delete it and re-run install if you want the v4 default re-seeded)"
  exit 0
fi

# Build the seeded TOML — production paths, all under $HOME.
# Heredoc keeps it readable. EOF is quoted so `$HOME` etc. are literal —
# we then write the rendered version with sed.
TMP_RENDER="$(mktemp -t subctl-evy-v4-config.XXXXXX)"
cat > "$TMP_RENDER" <<'TOML_EOF'
# Evy v4 daemon configuration — seeded by install.sh on first run.
# Layered: this file is overridden by env vars matching
# EVY_<SECTION>__<KEY> (double-underscore is section/key delimiter).
#
# Edit freely — the installer will not overwrite this file on re-run.

[scheduler]
db_path = "__SUBCTL_CONFIG_DIR__/evy-scheduler.db"

[policy]
# Operator-editable policy. The installer does NOT seed a policy.toml —
# Evy ships its own defaults in-binary. Set this path if you want to
# override.
# path = "__SUBCTL_CONFIG_DIR__/policy.toml"

# At least one provider is required for the daemon to do useful work.
# Uncomment + populate the provider blocks you have credentials for.

# [providers.claude_code]
# config_dir = "__HOME__/.claude"
# tmux_session = "evy-claude"
# working_dir = "__SUBCTL_CONFIG_DIR__"
# policy_mode = "Trusted"

# [providers.codex]
# codex_home = "__HOME__/.codex"
# tmux_session = "evy-codex"
# working_dir = "__SUBCTL_CONFIG_DIR__"
# policy_mode = "Trusted"

[comms.http]
host = "127.0.0.1"
# Parallel-test default: 8797 lets v4 run alongside v3 (still on 8787).
# When you're ready to fully cut over, change this to 8787 AND stop v3 first.
port = 8797
allow_origins = []

[memory]
observation_db = "__SUBCTL_CONFIG_DIR__/evy-observations.db"
score_db       = "__SUBCTL_CONFIG_DIR__/evy-scores.db"
preferences_db = "__SUBCTL_CONFIG_DIR__/evy-preferences.db"
playbook_dir   = "__SUBCTL_CONFIG_DIR__/playbooks"

# Optional: surface claude-mem priors via FTS5 retrieval.
# claude_mem_db = "__HOME__/.claude-mem/db.sqlite"

# ── Phase 6 — thinking-partner (operator chat surface) ───────────────
#
# Backend tradeoffs (recommended default: lm-studio):
#   lm-studio  — local OpenAI-compat model running in LM Studio on
#                127.0.0.1:1234. Free, fast, private, no API key. Best
#                for lightweight "what's running / what's broken" chat.
#   anthropic  — direct Anthropic Messages API. Highest quality;
#                requires a paid key in ANTHROPIC_API_KEY (or
#                whichever env var `api_key_env` names).
#   stub       — fixed-reply stub for smoke tests; never touches a
#                network.
#
# Without this block the daemon boots without a chat surface and
# POST /api/evy/chat returns 503.
#
# [thinking_partner]
# backend = "lm-studio"
# # Optional model override — required only when LM Studio has more
# # than one model loaded and you want to pin one.
# # model = "gemma-4-26b-a4b-it-mlx"
# # max_tokens = 2048
#
# [thinking_partner.lm_studio]
# endpoint = "http://127.0.0.1:1234"
# temperature = 0.7
TOML_EOF

# Substitute placeholders. Using sed with `|` delimiter to avoid escaping `/`.
sed -i.bak \
  -e "s|__HOME__|$HOME|g" \
  -e "s|__SUBCTL_CONFIG_DIR__|$SUBCTL_CONFIG_DIR|g" \
  "$TMP_RENDER"
rm -f "${TMP_RENDER}.bak"

if [ "${DRY_RUN:-false}" = "true" ]; then
  subctl_info "[dry-run] would write config → $SUBCTL_CONFIG_PATH"
  printf '\n%s──── rendered config ────%s\n' "$SUBCTL_C_DIM" "$SUBCTL_C_RST" >&2
  sed 's/^/    /' "$TMP_RENDER" >&2
  printf '%s────────────────────────%s\n\n' "$SUBCTL_C_DIM" "$SUBCTL_C_RST" >&2
  rm -f "$TMP_RENDER"
  exit 0
fi

install -m 0644 "$TMP_RENDER" "$SUBCTL_CONFIG_PATH"
rm -f "$TMP_RENDER"

subctl_ok "config seeded: $SUBCTL_CONFIG_PATH"
subctl_info "  edit it to enable providers (claude_code / codex) before cutover"
