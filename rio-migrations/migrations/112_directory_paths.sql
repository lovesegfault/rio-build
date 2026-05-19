-- Commentary: see rio-migrations/src/migrations.rs M_112
-- ADR-022 tenant-scope rework: replace the materialized
-- `directory_tenants`/`file_blob_tenants` snapshots with a per-path
-- linkage so DirectoryService resolves tenancy from `path_tenants` at
-- read time (single source of truth).

CREATE TABLE directory_paths (
    digest           BYTEA NOT NULL REFERENCES directories (digest) ON DELETE CASCADE,
    store_path_hash  BYTEA NOT NULL REFERENCES manifests (store_path_hash) ON DELETE CASCADE,
    PRIMARY KEY (digest, store_path_hash)
);
CREATE INDEX directory_paths_path_idx ON directory_paths (store_path_hash);

-- 065-067 ship in the same release; no backfill is needed.
DROP TABLE file_blob_tenants;
DROP TABLE directory_tenants;
