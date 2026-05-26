-- Commentary: see rio-migrations/src/migrations.rs M_068
ALTER TABLE pending_s3_deletes
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'chunk'
    CHECK (kind IN ('chunk', 'blob'));
