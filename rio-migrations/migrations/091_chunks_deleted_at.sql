-- Commentary: see rio-migrations/src/migrations.rs M_091
ALTER TABLE chunks ADD COLUMN deleted_at TIMESTAMPTZ;
CREATE INDEX idx_chunks_reapable ON chunks (deleted_at) WHERE deleted;
