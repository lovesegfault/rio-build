-- Build log S3-blob metadata. One row per (build_id, drv_hash) pair.
--
-- A derivation builds exactly once even if N builds want it (DAG merging).
-- LogFlusher writes ONE S3 blob and N rows here (one per interested build,
-- all with the same s3_key). The dashboard's "logs for build X" query joins
-- on build_id; the dedup on s3_key is invisible to it.
--
-- `is_complete`: the 30s periodic flush writes rows with is_complete=false
-- (snapshot, derivation still running). The on-completion flush UPSERTs the
-- final row with is_complete=true. AdminService.GetBuildLogs checks the ring
-- buffer first (newer than any snapshot); only falls back to S3 for
-- is_complete=true rows (derivation has finished, ring buffer is gone).

CREATE TABLE build_logs (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    build_id        UUID NOT NULL REFERENCES builds (build_id),
    drv_hash        TEXT NOT NULL,
    s3_key          TEXT NOT NULL,      -- logs/{build_id}/{drv_hash}.log.gz
    line_count      BIGINT NOT NULL,
    byte_size       BIGINT NOT NULL,    -- gzipped size (the S3 object size)
    is_complete     BOOLEAN NOT NULL DEFAULT FALSE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- UPSERT target: on-completion flush replaces any periodic snapshot.
    CONSTRAINT build_logs_build_drv_uq UNIQUE (build_id, drv_hash)
);

-- Dashboard query: "all logs for build X, ordered by derivation".
CREATE INDEX build_logs_build_idx ON build_logs (build_id);
