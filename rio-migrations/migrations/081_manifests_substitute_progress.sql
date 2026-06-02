-- Commentary: see rio-migrations/src/migrations.rs M_081
ALTER TABLE manifests
    ADD COLUMN fetched_bytes BIGINT,
    ADD COLUMN last_progress_at TIMESTAMPTZ,
    ADD COLUMN stall_count SMALLINT NOT NULL DEFAULT 0,
    ADD COLUMN claimed_by TEXT;
