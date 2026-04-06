-- migration 018: chunk_tenants junction for per-tenant FindMissingChunks.
--
-- WHY NOT A tenant_id COLUMN ON chunks: chunks are content-addressed.
-- Tenant A uploads glibc.so, tenant B uploads identical bytes -> same
-- blake3_hash. A single-column owner would either (a) overwrite on
-- conflict (B steals A's attribution) or (b) be DO-NOTHING (B told
-- "missing" forever, re-uploads on every build). Many-to-many junction
-- lets both tenants see the chunk as present.
--
-- FOLLOWS path_tenants PRECEDENT (012): composite PK, FK CASCADE both
-- sides, secondary index leading with tenant_id for the lookup shape.
--
-- FK to chunks(blake3_hash): unlike path_tenants which skipped the
-- narinfo FK (upsert-before-PUT race), chunk_tenants is populated
-- AFTER the chunks row exists (PutChunk does INSERT chunks -> then
-- INSERT chunk_tenants, same txn). No race; FK enforces referential
-- integrity. ON DELETE CASCADE is belt-and-suspenders: GC soft-deletes
-- chunks (UPDATE SET deleted=TRUE, never DELETE FROM), so the CASCADE
-- trigger doesn't fire in practice. Junction rows are explicitly
-- DELETEd by enqueue_chunk_deletes (gc/mod.rs) in the same tx as the
-- soft-delete. The FK + CASCADE guard against a future hard-delete path.

CREATE TABLE chunk_tenants (
    blake3_hash BYTEA NOT NULL REFERENCES chunks(blake3_hash) ON DELETE CASCADE,
    tenant_id   UUID  NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    PRIMARY KEY (blake3_hash, tenant_id)
);

-- Covers the FindMissingChunks shape: "for tenant X, which of hashes
-- [h1, h2, ...] are attributed?" -> equality on tenant_id, probe on
-- blake3_hash. (tenant_id, blake3_hash) order lets PG do an index-only
-- scan without touching the heap.
CREATE INDEX chunk_tenants_tenant_idx ON chunk_tenants (tenant_id, blake3_hash);
