# Plan 0249: Migration batch — 014 tenant_keys + 015 realisation_deps + 016 jwt_revoked

Single batched migration plan claiming the next three migration numbers. sqlx checksum-locks migrations — post-merge renumbering is painful. Batching means one `.sqlx/` regen, one `sqlx migrate run` validation, one plan that owns "the migration numbers are correct."

**USER DECISIONS applied:**
- **Q3:** migration 015 is a **junction table** `realisation_deps(realisation_id, dep_realisation_id)`, NOT a JSONB column
- **Q4:** migration 016 `jwt_revoked(jti, revoked_at)` for persistent revocation — gateway stays PG-free, scheduler does the lookup

Numbering: `ls migrations/` at `6b5d4f4` ends at 011. 4b P0206=012, 4c P0227=013. Phase 5 starts at 014. **Verify at dispatch.**

## Entry criteria

- [P0247](plan-0247-spike-ca-wire-capture-schema-adr.md) merged (wire shape captured — informs column sizes in 015, though schema is already decided per Q3)
- [P0227](plan-0227-build-samples-ema-migration.md) merged (migration 013 exists — numbering depends on it)

## Tasks

### T1 — `feat(migrations):` 014_tenant_keys

NEW `migrations/014_tenant_keys.sql`:

```sql
-- Per-tenant signing keys. Cluster key stays as fallback when tenant has none.
-- r[store.tenant.sign-key] — P0256 consumes.
CREATE TABLE tenant_keys (
    key_id        SERIAL PRIMARY KEY,
    tenant_id     UUID NOT NULL REFERENCES tenants(tenant_id),
    key_name      TEXT NOT NULL,
    ed25519_seed  BYTEA NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at    TIMESTAMPTZ NULL
);
-- Active-key lookup is the hot path (signing every narinfo)
CREATE INDEX tenant_keys_active_idx ON tenant_keys(tenant_id) WHERE revoked_at IS NULL;
```

### T2 — `feat(migrations):` 015_realisation_deps (junction table per Q3)

NEW `migrations/015_realisation_deps.sql`:

```sql
-- USER Q3: junction table, NOT JSONB. CA-depends-on-CA resolution (P0253).
-- Populated by wopRegisterDrvOutput; queried by scheduler resolve.
-- r[sched.ca.resolve] — P0253 consumes.
CREATE TABLE realisation_deps (
    realisation_id     BIGINT NOT NULL REFERENCES realisations(id) ON DELETE CASCADE,
    dep_realisation_id BIGINT NOT NULL REFERENCES realisations(id) ON DELETE CASCADE,
    PRIMARY KEY (realisation_id, dep_realisation_id)
);
-- Reverse lookup: "who depends on this realisation?" (for cutoff cascade)
CREATE INDEX realisation_deps_reverse_idx ON realisation_deps(dep_realisation_id);
```

Verify `realisations.id` column name at dispatch (`\d realisations` in test PG, or grep migrations).

### T3 — `feat(migrations):` 016_jwt_revoked (per Q4)

NEW `migrations/016_jwt_revoked.sql`:

```sql
-- USER Q4: persistent JWT revocation. Gateway stays PG-free — SCHEDULER
-- does the lookup (it already has PG + does tenant-resolve). Gateway
-- forwards jti in SubmitBuildRequest.jwt_jti.
-- r[gw.jwt.verify] — P0259 consumes (scheduler-side check).
CREATE TABLE jwt_revoked (
    jti         TEXT PRIMARY KEY,
    revoked_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    reason      TEXT NULL
);
-- Cleanup: jti older than max-JWT-lifetime can be swept (exp < now() means
-- the JWT is invalid regardless of revocation). No index needed for that —
-- full scan during maintenance sweep is fine.
```

### T4 — `feat(scheduler):` add is_ca column (batched from P0250 T4)

[P0250](plan-0250-ca-detect-plumb-is-ca.md)'s `is_ca BOOLEAN` column is batched here to keep migration count down:

Append to `014_tenant_keys.sql` (or a separate `014b_` — implementer's call):

```sql
-- Batched from P0250: scheduler CA detection flag.
ALTER TABLE derivations ADD COLUMN IF NOT EXISTS is_ca BOOLEAN NOT NULL DEFAULT FALSE;
```

### T5 — `feat(sqlx):` prepare offline cache

```bash
nix develop -c cargo sqlx prepare --workspace
```

Commit `.sqlx/` — **forgetting this fails offline builds** (4c R13 pattern).

### T6 — `test:` migrate clean on fresh DB

```bash
nix develop -c bash -c 'cd rio-store && sqlx migrate run --database-url $DATABASE_URL'
```

Via `rio-test-support` ephemeral PG. All three migrations apply; schema matches expectations.

## Exit criteria

- `/nbr .#ci` green
- `ls migrations/ | sort -V | tail -3` → `014_*.sql 015_*.sql 016_*.sql`
- `.sqlx/` committed (diff shows new query hashes)
- `sqlx migrate run` clean on fresh ephemeral PG

## Tracey

none — schema prologue. Markers (`r[store.tenant.sign-key]`, `r[sched.ca.resolve]`, `r[gw.jwt.verify]`) are IMPL'd by downstream plans that query these tables.

## Files

```json files
[
  {"path": "migrations/014_tenant_keys.sql", "action": "NEW", "note": "T1+T4: tenant_keys table + derivations.is_ca column"},
  {"path": "migrations/015_realisation_deps.sql", "action": "NEW", "note": "T2: junction table (USER Q3)"},
  {"path": "migrations/016_jwt_revoked.sql", "action": "NEW", "note": "T3: jti revocation (USER Q4)"},
  {"path": "rio-scheduler/.sqlx/placeholder", "action": "MODIFY", "note": "T5: cargo sqlx prepare --workspace regen (actually many files under workspace .sqlx/ — this is a proxy entry)"}
]
```

```
migrations/
├── 014_tenant_keys.sql           # T1+T4: tenant_keys + derivations.is_ca
├── 015_realisation_deps.sql      # T2: junction table (Q3)
└── 016_jwt_revoked.sql           # T3: jti revocation (Q4)
.sqlx/                            # T5: regenerated offline cache
```

## Dependencies

```json deps
{"deps": [247, 227], "soft_deps": [], "note": "sqlx checksum-lock — single migration commit. Q3 (junction) + Q4 (PG table) user-decided. VERIFY at dispatch: ls migrations/ | tail -1 → expect 013."}
```

**Depends on:** [P0247](plan-0247-spike-ca-wire-capture-schema-adr.md) — wire-shape captured (informs 015 column sizing, though schema structure is Q3-decided). [P0227](plan-0227-build-samples-ema-migration.md) — migration 013 exists, numbering chains from it.
**Conflicts with:** none — migrations are strictly additive, numbering is serial by construction. `.sqlx/` regen is deterministic per schema.
