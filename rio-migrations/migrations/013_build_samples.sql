-- migration 013: build_samples — raw per-build completion samples for
-- the CutoffRebalancer (P0229). EMAs destroy distribution shape; the
-- rebalancer needs raw (duration, peak_mem) pairs to partition into
-- equal-load size classes.
--
-- Retention: scheduler main.rs spawns a 1h-interval task that deletes
-- rows older than 30 days. Index on completed_at supports the range
-- delete.
--
-- NOTE: migrations/009_phase4.sql:8 has a stale "Part D (4c): build_samples
-- (appended later)" comment. That intent is overtaken — 010/011/012 landed
-- after 009, and sqlx migrate checksums prevent any 009 edit. 013 is correct.

CREATE TABLE build_samples (
    id                 BIGSERIAL PRIMARY KEY,
    pname              TEXT NOT NULL,
    system             TEXT NOT NULL,
    duration_secs      DOUBLE PRECISION NOT NULL,
    peak_memory_bytes  BIGINT NOT NULL,
    completed_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX build_samples_completed_at_idx ON build_samples (completed_at);
