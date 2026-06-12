-- Commentary: see rio-migrations/src/migrations.rs M_072

CREATE TABLE drv_blobs (
    digest        BYTEA PRIMARY KEY,
    drv_path      TEXT NOT NULL,
    drv_path_hash BYTEA NOT NULL,
    body          BYTEA NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX drv_blobs_drv_path_hash_idx ON drv_blobs (drv_path_hash);

CREATE TABLE drv_blob_tenants (
    digest    BYTEA NOT NULL REFERENCES drv_blobs (digest) ON DELETE CASCADE,
    tenant_id UUID  NOT NULL REFERENCES tenants (tenant_id) ON DELETE CASCADE,
    PRIMARY KEY (digest, tenant_id)
);

CREATE TABLE chunk_tenants (
    blake3_hash BYTEA NOT NULL REFERENCES chunks (blake3_hash) ON DELETE CASCADE,
    tenant_id   UUID  NOT NULL REFERENCES tenants (tenant_id) ON DELETE CASCADE,
    PRIMARY KEY (blake3_hash, tenant_id)
);
