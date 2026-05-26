-- Worker-effectiveness scoring — layer 3 of the learning loop.
-- ADR 0020 §"Layer 3 — Worker-effectiveness scoring". One row per
-- (provider, task_class) pair; counters and a running average are
-- updated in-place by ScoreLedger::record via an upsert.
--
-- task_class is intentionally coarse (e.g. `code_change`, `code_review`,
-- `investigation`). The taxonomy will tighten in a future ADR once we
-- have enough recorded outcomes to know what cuts naturally.

CREATE TABLE IF NOT EXISTS worker_effectiveness_scores (
    provider          TEXT NOT NULL,
    task_class        TEXT NOT NULL,
    successes         INTEGER NOT NULL DEFAULT 0,
    failures          INTEGER NOT NULL DEFAULT 0,
    avg_duration_ms   REAL NOT NULL DEFAULT 0,
    last_seen         TEXT NOT NULL,
    PRIMARY KEY (provider, task_class)
);

CREATE INDEX IF NOT EXISTS worker_scores_task_class_idx
    ON worker_effectiveness_scores(task_class);
