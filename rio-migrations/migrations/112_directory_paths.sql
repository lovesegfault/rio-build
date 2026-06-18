-- Commentary: see rio-migrations/src/migrations.rs M_112

CREATE TABLE directory_paths (
    digest           BYTEA NOT NULL REFERENCES directories (digest) ON DELETE CASCADE,
    store_path_hash  BYTEA NOT NULL REFERENCES manifests (store_path_hash) ON DELETE CASCADE,
    PRIMARY KEY (digest, store_path_hash)
);
CREATE INDEX directory_paths_path_idx ON directory_paths (store_path_hash);

DROP TABLE file_blob_tenants;
DROP TABLE directory_tenants;
