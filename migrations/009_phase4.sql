-- ═══════════════════════════════════════════════════════════════════
-- Migration 009 — Phase 4: multi-tenancy + poison persistence +
-- per-tenant GC + SITA-E samples. Parts appended across sub-phases.
--
-- Part A (4a): tenants table + FK backfill
-- Part B (4a): derivations.poisoned_at
-- Part C (4b): path_tenants junction (appended later)
-- Part D (4c): build_samples (appended later)
-- ═══════════════════════════════════════════════════════════════════

-- ── Part A: tenants table + FK backfill (phase 4a) ─────────────────

CREATE TABLE tenants (
    tenant_id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_name         TEXT NOT NULL UNIQUE,
    gc_retention_hours  INTEGER NOT NULL DEFAULT 168,  -- 7 days
    gc_max_store_bytes  BIGINT,                        -- NULL = no limit
    cache_token         TEXT UNIQUE,                   -- Bearer token for binary cache; NULL = no cache access for this tenant
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Pre-FK cleanup: existing builds/derivations.tenant_id values are
-- valid-UUID-format (pre-009 code parsed with uuid::Uuid) but
-- reference nothing. NULL them out before adding FKs — otherwise the
-- FK constraint would fail. In practice these columns have never been
-- populated (gateway sends empty string → scheduler maps to NULL),
-- but defense-in-depth for dev databases that may have test data.
UPDATE builds SET tenant_id = NULL WHERE tenant_id IS NOT NULL;
UPDATE derivations SET tenant_id = NULL WHERE tenant_id IS NOT NULL;

ALTER TABLE builds
    ADD CONSTRAINT builds_tenant_fk
    FOREIGN KEY (tenant_id) REFERENCES tenants(tenant_id) ON DELETE SET NULL;

ALTER TABLE derivations
    ADD CONSTRAINT derivations_tenant_fk
    FOREIGN KEY (tenant_id) REFERENCES tenants(tenant_id) ON DELETE SET NULL;

-- Partial indexes: most rows will have NULL tenant_id (single-tenant
-- mode). No point indexing NULLs for a tenant-filtered WHERE clause.
CREATE INDEX builds_tenant_idx ON builds (tenant_id) WHERE tenant_id IS NOT NULL;
CREATE INDEX derivations_tenant_idx ON derivations (tenant_id) WHERE tenant_id IS NOT NULL;

-- ── Part B: poison persistence (phase 4a) ──────────────────────────

-- poisoned_at was previously in-memory only (state.poisoned_at =
-- Some(Instant::now()) in poison_and_cascade/handle_permanent_failure)
-- so TTL reset on scheduler restart. Now persisted and reloaded via
-- a separate load_poisoned_derivations query (since 'poisoned' is in
-- TERMINAL_STATUSES and load_nonterminal_derivations filters it out).
ALTER TABLE derivations ADD COLUMN poisoned_at TIMESTAMPTZ;
