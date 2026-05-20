-- Commentary: see rio-migrations/src/migrations.rs M_061

-- Greenfield: drop and recreate. No backfill (pre-prod).
DROP TABLE IF EXISTS build_logs;

-- One row per derivation execution. exec_id is UUIDv7 minted at dispatch.
-- drv_hash is drv_log_hash() of the .drv path (32-char nixbase32) — NOT
-- derivations.drv_hash, which is the polymorphic dedup identity.
CREATE TABLE drv_logs (
    exec_id     UUID        PRIMARY KEY,
    drv_hash    CHAR(32)    NOT NULL,
    s3_key      TEXT        NOT NULL,
    first_line  BIGINT      NOT NULL DEFAULT 0,
    line_count  BIGINT      NOT NULL DEFAULT 0,
    total_bytes BIGINT      NOT NULL DEFAULT 0,
    is_complete BOOLEAN     NOT NULL DEFAULT FALSE,
    status      TEXT,
    started_at  TIMESTAMPTZ NOT NULL,
    finished_at TIMESTAMPTZ
);

-- Latest-exec lookup: SELECT exec_id FROM drv_logs WHERE drv_hash = $1
-- ORDER BY exec_id DESC LIMIT 1. UUIDv7 time-sortability means DESC = newest.
CREATE INDEX drv_logs_drv_latest ON drv_logs (drv_hash, exec_id DESC);

-- TTL sweep: WHERE started_at < $cutoff LIMIT $batch (sweep_expired_logs).
-- Sub-LIMIT passes (incl. the terminal 0-row pass) seq-scan without it.
CREATE INDEX drv_logs_started_at ON drv_logs (started_at);

-- Recovery carrier: the new leader reloads exec_id for active assignments
-- so the flusher keys subsequent uploads correctly after failover.
ALTER TABLE assignments ADD COLUMN exec_id UUID;

-- build_id ↔ exec_id correlation. Set on terminal paths where an execution
-- ran (Completed, Poisoned, timeout-exhausted Cancelled); NULL for
-- Cached/DependencyFailed/Skipped/never-dispatched/non-terminal.
-- Spec: sched.merge.exec-correlation+2.
ALTER TABLE build_derivations ADD COLUMN exec_id UUID;
