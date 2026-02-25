-- Store tables for rio-build Phase 2a.
--
-- Tables: narinfo, nar_blobs.
--
-- Phase 2a simplifications vs spec (docs/src/components/store.md):
--   - No chunking: nar_blobs replaces manifests + manifest_data + chunks
--   - No content_index (CA cutoff deferred)
--   - No realisations (Phase 5)
--   - No pending_s3_deletes (GC deferred)
--   - tenant_id is nullable (multi-tenancy unused in Phase 2a)
--   - signatures column present but will remain empty (signing deferred)

-- Store path metadata (narinfo equivalent)
CREATE TABLE narinfo (
    store_path_hash  BYTEA PRIMARY KEY,
    store_path       TEXT NOT NULL,
    deriver          TEXT,
    nar_hash         BYTEA NOT NULL,           -- SHA-256 digest
    nar_size         BIGINT NOT NULL,
    "references"     TEXT[] NOT NULL DEFAULT '{}',
    signatures       TEXT[] NOT NULL DEFAULT '{}',
    ca               TEXT,                     -- content address (empty for input-addressed)
    tenant_id        UUID,                     -- nullable: unused in Phase 2a
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- NAR blob storage reference (Phase 2a: full NARs, no chunking)
CREATE TABLE nar_blobs (
    store_path_hash  BYTEA PRIMARY KEY REFERENCES narinfo (store_path_hash),
    status           TEXT NOT NULL DEFAULT 'uploading'
                     CHECK (status IN ('uploading', 'complete')),
    blob_key         TEXT NOT NULL,            -- filesystem path or S3 key
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
