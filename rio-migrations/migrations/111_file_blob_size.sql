-- Commentary: see rio-migrations/src/migrations.rs M_111

ALTER TABLE file_blobs ADD COLUMN size BIGINT NOT NULL DEFAULT 0;
