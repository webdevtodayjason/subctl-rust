-- One row per fire of a job. `outcome` is JSON-serialized RunOutcome.
CREATE TABLE runs (
  id           TEXT PRIMARY KEY,
  job_id       TEXT NOT NULL,
  started_at   TEXT NOT NULL,
  finished_at  TEXT,
  outcome      TEXT NOT NULL,
  FOREIGN KEY (job_id) REFERENCES jobs(id) ON DELETE CASCADE
);
