-- Observation log — append-only event-sourced substrate.
-- ADR 0020 §"Learning loop — layer 1". The schema is intentionally
-- wide and weakly typed: a top-level discriminator `kind` for cheap
-- prefix queries, a full JSON payload, and optional correlation /
-- metadata columns. Strongly typed querying happens above the log.

CREATE TABLE IF NOT EXISTS observations (
    id              TEXT PRIMARY KEY,
    ts              TEXT NOT NULL,
    kind            TEXT NOT NULL,
    payload         TEXT NOT NULL,
    correlation_id  TEXT,
    metadata        TEXT
);

CREATE INDEX IF NOT EXISTS observations_ts_idx
    ON observations(ts);

CREATE INDEX IF NOT EXISTS observations_kind_idx
    ON observations(kind);

CREATE INDEX IF NOT EXISTS observations_correlation_idx
    ON observations(correlation_id);
