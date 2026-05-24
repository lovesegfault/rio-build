-- Commentary: see rio-migrations/src/migrations.rs M_066

-- One row per execution: lifecycle only. Writers: rio-scheduler (INSERT at
-- dispatch, UPDATE at terminal); rio-store's TTL sweep (DELETE by age).
CREATE TABLE drv_executions (
    exec_id          UUID        PRIMARY KEY,
    drv_hash         CHAR(32)    NOT NULL,
    executor_id      TEXT        NOT NULL,
    started_at       TIMESTAMPTZ NOT NULL,
    finished_at      TIMESTAMPTZ,
    status           TEXT,
    final_line_count BIGINT
);

-- Latest-exec lookup: ORDER BY exec_id DESC LIMIT 1 (UUIDv7 DESC = newest).
CREATE INDEX drv_executions_drv_latest ON drv_executions (drv_hash, exec_id DESC);

-- TTL sweep: WHERE started_at < $cutoff LIMIT $batch.
CREATE INDEX drv_executions_started_at ON drv_executions (started_at);

-- One row per durably committed log chunk. Writer: rio-store, INSERT-only.
CREATE TABLE drv_log_chunks (
    exec_id    UUID        NOT NULL,
    session_id UUID        NOT NULL,
    chunk_seq  INT         NOT NULL,
    first_line BIGINT      NOT NULL,
    line_count BIGINT      NOT NULL,
    byte_size  BIGINT      NOT NULL,
    s3_key     TEXT        NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (exec_id, session_id, chunk_seq)
);

-- Range reads: WHERE exec_id = $1 AND first_line + line_count > $since.
CREATE INDEX drv_log_chunks_range ON drv_log_chunks (exec_id, first_line);

-- The live-ingest routing registry: at most one live session per execution.
-- Writer: rio-store.
CREATE TABLE log_ingest_sessions (
    exec_id      UUID        PRIMARY KEY,
    session_id   UUID        NOT NULL,
    replica_pod  TEXT        NOT NULL,
    started_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    heartbeat_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
