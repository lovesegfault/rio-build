-- Commentary: see rio-store/src/migrations.rs M_024
CREATE UNIQUE INDEX idx_pending_s3_deletes_blake3_unique
    ON pending_s3_deletes (blake3_hash)
    WHERE blake3_hash IS NOT NULL;
