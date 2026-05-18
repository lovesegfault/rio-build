-- migration 014: tenant_keys — per-tenant signing keys (P0256 consumes).
--
-- Cluster key stays as fallback when a tenant has none. ed25519 seed is
-- 32 raw bytes; key_name is the human-readable identifier that appears
-- in the signed narinfo (e.g. "tenant-foo-1"). revoked_at NULL = active.
--
-- BATCHED: derivations.is_ca (from P0250 T4) is appended below — single
-- migration number claimed instead of two. sqlx checksums make renumbering
-- painful; batching the ALTER here is cheaper than a 014b file.

-- r[store.tenant.sign-key] — impl lands in P0256
CREATE TABLE tenant_keys (
    key_id        SERIAL PRIMARY KEY,
    tenant_id     UUID NOT NULL REFERENCES tenants(tenant_id),
    key_name      TEXT NOT NULL,
    ed25519_seed  BYTEA NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at    TIMESTAMPTZ NULL
);

-- Active-key lookup is the hot path (signing every narinfo). Partial
-- index: revoked keys never read on the signing path.
CREATE INDEX tenant_keys_active_idx ON tenant_keys(tenant_id) WHERE revoked_at IS NULL;

-- ------------------------------------------------------------------------
-- Batched from P0250 T4: scheduler CA detection flag.
--
-- No IF NOT EXISTS (house style, see 004/009/010): migrations run once,
-- checksummed. A column-already-exists error is a schema-state bug to
-- surface, not silently skip.
-- ------------------------------------------------------------------------
ALTER TABLE derivations ADD COLUMN is_ca BOOLEAN NOT NULL DEFAULT FALSE;
