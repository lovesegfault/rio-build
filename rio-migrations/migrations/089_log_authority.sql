-- Commentary: see rio-migrations/src/migrations.rs M_089
ALTER TABLE drv_log_chunks
    ADD COLUMN accounted_bytes BIGINT NOT NULL DEFAULT 0;

CREATE VIEW latest_build_exec AS
    SELECT DISTINCT ON (drv_hash) drv_hash, exec_id
    FROM drv_executions
    WHERE attempt_kind = 'build'
    ORDER BY drv_hash, exec_id DESC;

CREATE INDEX drv_executions_build_latest_idx
    ON drv_executions (drv_hash, exec_id DESC)
    WHERE attempt_kind = 'build';
