-- Operator-defined cron jobs. One row per job; survives restart.
CREATE TABLE jobs (
  id          TEXT PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,
  cron_expr   TEXT NOT NULL,
  action      TEXT NOT NULL,    -- JSON-serialized JobAction
  enabled     INTEGER NOT NULL,
  created_at  TEXT NOT NULL,
  last_run    TEXT
);
