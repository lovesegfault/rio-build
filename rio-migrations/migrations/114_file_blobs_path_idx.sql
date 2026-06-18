-- no-transaction
-- Commentary: see rio-migrations/src/migrations.rs M_114
CREATE INDEX CONCURRENTLY IF NOT EXISTS file_blobs_path_idx
    ON file_blobs (store_path_hash);
