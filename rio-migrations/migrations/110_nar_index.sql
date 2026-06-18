-- Commentary: see rio-migrations/src/migrations.rs M_110

CREATE TABLE nar_index (
    store_path_hash  BYTEA PRIMARY KEY
                     REFERENCES manifests (store_path_hash) ON DELETE CASCADE,
    entries          BYTEA NOT NULL,
    root_node        BYTEA,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

ALTER TABLE manifests ADD COLUMN nar_indexed BOOLEAN NOT NULL DEFAULT FALSE;

CREATE INDEX manifests_nar_index_pending_idx
    ON manifests (updated_at)
    WHERE NOT nar_indexed AND status = 'complete';

CREATE TABLE directories (
    digest    BYTEA PRIMARY KEY,
    body      BYTEA NOT NULL,
    refcount  INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE directory_tenants (
    digest     BYTEA NOT NULL REFERENCES directories (digest) ON DELETE CASCADE,
    tenant_id  UUID NOT NULL REFERENCES tenants (tenant_id) ON DELETE CASCADE,
    PRIMARY KEY (digest, tenant_id)
);
CREATE INDEX directory_tenants_tenant_idx ON directory_tenants (tenant_id, digest);

CREATE TABLE file_blobs (
    digest           BYTEA NOT NULL,
    store_path_hash  BYTEA NOT NULL REFERENCES manifests (store_path_hash) ON DELETE CASCADE,
    nar_offset       BIGINT NOT NULL,
    PRIMARY KEY (digest, store_path_hash)
);
CREATE INDEX file_blobs_digest_idx ON file_blobs (digest);

CREATE TABLE file_blob_tenants (
    digest     BYTEA NOT NULL,
    tenant_id  UUID NOT NULL REFERENCES tenants (tenant_id) ON DELETE CASCADE,
    PRIMARY KEY (digest, tenant_id)
);
CREATE INDEX file_blob_tenants_tenant_idx ON file_blob_tenants (tenant_id, digest);

ALTER TABLE narinfo ADD COLUMN compat_file_hash BYTEA;

ALTER TABLE chunks ADD COLUMN durable BOOLEAN NOT NULL DEFAULT FALSE;
CREATE INDEX chunks_present_idx ON chunks (blake3_hash) WHERE durable AND NOT deleted;
