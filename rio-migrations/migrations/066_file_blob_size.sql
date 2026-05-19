-- Commentary: see rio-store/src/migrations.rs M_066
-- ADR-022 P0577/P0570: denormalize file size onto file_blobs so the
-- ReadBlob/StatBlob hot path can skip the nar_index.entries decode.

ALTER TABLE file_blobs ADD COLUMN size BIGINT NOT NULL DEFAULT 0;
