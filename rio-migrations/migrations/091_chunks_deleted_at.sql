-- Commentary: see rio-migrations/src/migrations.rs M_091
ALTER TABLE chunks ADD COLUMN deleted_at TIMESTAMPTZ;
UPDATE chunks SET deleted_at = now() WHERE deleted AND deleted_at IS NULL; -- backfill pre-091 tombstones (bug_354)
CREATE INDEX idx_chunks_reapable ON chunks (deleted_at) WHERE deleted;
