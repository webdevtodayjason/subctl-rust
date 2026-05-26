-- Operator-preference model — layer 5 of the learning loop.
-- ADR 0020 §"Layer 5 — Operator-preference model". Distinct from the
-- operator's own auto-memory at ~/.claude/projects/...: this is *Evy's
-- model of* operator preferences, updated implicitly from corrections.
--
-- One row per key. `value` is a JSON-serialised PreferenceValue;
-- `kind` carries the discriminator (`boolean` / `text` / `number` /
-- `list`) so cheap filtered queries don't need to parse JSON.

CREATE TABLE IF NOT EXISTS operator_preferences (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL,
    kind        TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS operator_preferences_kind_idx
    ON operator_preferences(kind);
