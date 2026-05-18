-- Commentary: see rio-store/src/migrations.rs M_018
--
-- FROZEN: do not edit. sqlx checksums by content; comment edits
-- brick persistent-DB deploys. Commentary/rationale/history lives
-- in the Rust doc-module above.

CREATE TABLE chunk_tenants (
    blake3_hash BYTEA NOT NULL REFERENCES chunks(blake3_hash) ON DELETE CASCADE,
    tenant_id   UUID  NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    PRIMARY KEY (blake3_hash, tenant_id)
);

CREATE INDEX chunk_tenants_tenant_idx ON chunk_tenants (tenant_id, blake3_hash);
