-- Feedback ingest — layer 7 of the learning loop.
-- ADR 0020 §"Layer 7 — Feedback ingest". Operator corrections flow back
-- and become priors. Each row is one operator reaction; `kind` stores
-- the discriminator (`approved`/`rejected`/`corrected`/
-- `operator_preference`) for cheap filtering; `context` carries the
-- full Feedback envelope (FeedbackKind variant data + FeedbackContext)
-- as JSON so rehydration is lossless.

CREATE TABLE IF NOT EXISTS feedback (
    id          TEXT PRIMARY KEY,
    ts          TEXT NOT NULL,
    kind        TEXT NOT NULL,
    context     TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS feedback_ts_idx
    ON feedback(ts);

CREATE INDEX IF NOT EXISTS feedback_kind_idx
    ON feedback(kind);
