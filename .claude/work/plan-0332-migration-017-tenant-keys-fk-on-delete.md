# Plan 0332: Migration 017 — tenant_keys FK missing ON DELETE clause

Bughunter finding. [`migrations/014_tenant_keys.sql:14`](../../migrations/014_tenant_keys.sql) declares `tenant_id UUID NOT NULL REFERENCES tenants(tenant_id)` with **no `ON DELETE` clause**. PostgreSQL's default is `NO ACTION` — effectively `RESTRICT` at end-of-statement. Every other tenant-FK migration is explicit: [009](../../migrations/009_phase4.sql) uses `ON DELETE SET NULL` (×2), [012](../../migrations/012_path_tenants.sql) uses `ON DELETE CASCADE`, [015](../../migrations/015_realisation_deps.sql) uses `ON DELETE RESTRICT` (×2, with a comment explaining why). Migration 014 is the only silent default — introduced in [P0249](plan-0249-migration-batch-014-015-016.md).

**Consequence:** `DELETE FROM tenants WHERE tenant_id = $1` fails with `ERROR: update or delete on table "tenants" violates foreign key constraint "tenant_keys_tenant_id_fkey"` if ANY `tenant_keys` row references it — **including revoked keys** (`revoked_at IS NOT NULL`). A tenant with nothing but revoked keys is still undeletable.

No tenant-delete code path exists today (grep confirms: no `DELETE FROM tenants` anywhere in `rio-*/src/`). So this is **latent**, not live-breaking. But [P0256](plan-0256-per-tenant-signing-output-hash.md) (UNIMPL, consumes tenant_keys heavily) and any future tenant-lifecycle admin API will hit it.

**The clause choice:** `tenant_keys` rows are **owned by** the tenant — an orphan signing key is meaningless (the narinfo it signed references a gone tenant). This is the same shape as [012](../../migrations/012_path_tenants.sql)'s `path_tenants` CASCADE, not [009](../../migrations/009_phase4.sql)'s SET NULL (which nulls FK columns that are nullable — `tenant_keys.tenant_id` is `NOT NULL`, SET NULL would violate). **CASCADE is correct.**

Migration 014 already shipped (sqlx checksummed — can't edit the file). New migration 017: `ALTER TABLE ... DROP CONSTRAINT ... ADD CONSTRAINT ... ON DELETE CASCADE`.

## Tasks

### T1 — `fix(migrations):` migration 017 — ALTER CONSTRAINT to CASCADE

NEW [`migrations/017_tenant_keys_fk_cascade.sql`](../../migrations/017_tenant_keys_fk_cascade.sql):

```sql
-- migration 017: tenant_keys FK — add ON DELETE CASCADE.
--
-- 014 declared `REFERENCES tenants(tenant_id)` with no ON DELETE clause
-- (PG default = NO ACTION). Every other tenant-FK is explicit: 009 SET
-- NULL, 012 CASCADE, 015 RESTRICT. 014 was the only silent default
-- (P0249 shipped it; P0332 caught it).
--
-- CASCADE matches the 012 path_tenants pattern: tenant_keys are OWNED
-- BY the tenant (an orphan signing key signs narinfo for a gone tenant
-- — meaningless). SET NULL would violate NOT NULL at :14. RESTRICT would
-- make a tenant with only-revoked keys undeletable, which is the bug.
--
-- PG has no ALTER CONSTRAINT for FK referential actions. DROP + ADD.
-- PG auto-names CREATE TABLE FKs as `<table>_<col>_fkey`.

ALTER TABLE tenant_keys
    DROP CONSTRAINT tenant_keys_tenant_id_fkey;

ALTER TABLE tenant_keys
    ADD CONSTRAINT tenant_keys_tenant_id_fkey
    FOREIGN KEY (tenant_id) REFERENCES tenants(tenant_id)
    ON DELETE CASCADE;
```

**Constraint name:** PG's auto-naming for `CREATE TABLE ... REFERENCES` is `<table>_<column>_fkey`. Verify at dispatch: `SELECT conname FROM pg_constraint WHERE conrelid = 'tenant_keys'::regclass AND contype = 'f'` against a fresh-migrated TestDb. If PG picked a different name (unlikely — 014 has only one FK), adjust.

**Per coordinator guidance — check if tenant_keys is empty in prod:** it almost certainly isn't (P0256 hasn't landed, but tenant-key insertion might happen via seeding/fixtures). The DROP+ADD approach works regardless of row count — it's a constraint-metadata change, not a data migration. No `IF EXISTS` (house style, per 014's own comment at `:28-30`: "migrations run once, checksummed; a column-already-exists error is a schema-state bug to surface").

MODIFY [`migrations/014_tenant_keys.sql`](../../migrations/014_tenant_keys.sql) — **comment-only** (no SQL change, checksum-safe):

```sql
    tenant_id     UUID NOT NULL REFERENCES tenants(tenant_id),
    --                                     ^^^^^^^^^^^^^^^^^^^
    -- No ON DELETE clause (PG default NO ACTION). Migration 017 adds
    -- CASCADE — left here for archaeology (sqlx checksums 014 as-is).
```

**Wait — sqlx checksums include comments.** Check at dispatch: does sqlx hash the full file content or only non-comment SQL? If full content, this comment-edit breaks the checksum. In that case, **skip the 014 edit** and put the archaeology in 017's header comment only.

### T2 — `test(store):` tenant cascade deletes keys — including revoked

Add to `rio-store/src/metadata/` tests (wherever tenant_keys queries land — grep for `tenant_keys` at dispatch; if none yet, `rio-store/tests/` or inline in `rio-store/src/db.rs` tests):

```rust
/// Deleting a tenant CASCADEs to tenant_keys — including revoked keys.
/// Migration 014 shipped with no ON DELETE (PG default NO ACTION),
/// which made a tenant with only-revoked keys undeletable. 017 fixed.
///
/// Precondition self-check: assert the tenant_keys rows EXIST before
/// the delete. A broken test setup that never inserts would pass the
/// post-delete "zero rows" check vacuously.
#[sqlx::test(migrator = "MIGRATOR")]
async fn tenant_delete_cascades_keys_including_revoked(pool: PgPool) -> TestResult {
    // Insert tenant + two keys (one active, one revoked).
    let tenant_id: Uuid = sqlx::query_scalar(
        "INSERT INTO tenants (tenant_name) VALUES ('doomed') RETURNING tenant_id"
    ).fetch_one(&pool).await?;

    sqlx::query(
        "INSERT INTO tenant_keys (tenant_id, key_name, ed25519_seed) VALUES ($1, 'active-key', $2)"
    ).bind(tenant_id).bind(vec![0u8; 32]).execute(&pool).await?;

    sqlx::query(
        "INSERT INTO tenant_keys (tenant_id, key_name, ed25519_seed, revoked_at) VALUES ($1, 'revoked-key', $2, now())"
    ).bind(tenant_id).bind(vec![1u8; 32]).execute(&pool).await?;

    // Precondition: both rows exist. Not "proves nothing" — if this
    // assert fails, the INSERT above is broken and the cascade check
    // below would pass on an empty table.
    let pre: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tenant_keys WHERE tenant_id = $1"
    ).bind(tenant_id).fetch_one(&pool).await?;
    assert_eq!(pre, 2, "precondition: both keys inserted");

    // THE ACTION: delete the tenant. Pre-017 this would FAIL here
    // with FK violation. Post-017 it cascades.
    sqlx::query("DELETE FROM tenants WHERE tenant_id = $1")
        .bind(tenant_id).execute(&pool).await?;

    // Post-condition: keys gone.
    let post: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tenant_keys WHERE tenant_id = $1"
    ).bind(tenant_id).fetch_one(&pool).await?;
    assert_eq!(post, 0, "CASCADE should delete both keys (including revoked)");

    Ok(())
}
```

Check at dispatch what the actual `tenants` schema needs for INSERT (it may have more NOT NULL columns than `tenant_name` — grep migration 009 for the CREATE TABLE).

## Exit criteria

- `/nbr .#ci` green
- `ls migrations/017_*.sql` → exactly 1 file (T1)
- `grep 'ON DELETE CASCADE' migrations/017_*.sql` → 1 hit (T1: clause present)
- `grep 'DROP CONSTRAINT tenant_keys_tenant_id_fkey' migrations/017_*.sql` → 1 hit (T1: explicit drop, not ALTER)
- sqlx migrate check passes (017 checksummed cleanly; 014 unchanged OR comment-only-edit is checksum-safe — resolve at dispatch)
- `cargo nextest run -p rio-store tenant_delete_cascades_keys_including_revoked` → pass (T2: cascade works)
- **Mutation check:** run the T2 test against a DB migrated through 016 only (skip 017) → the `DELETE FROM tenants` line fails with FK violation. Proves the test catches the regression, not just that 017 is syntactically valid.
- `grep 'tenant_keys.*ON DELETE\|017' migrations/014_tenant_keys.sql migrations/017_*.sql` — archaeology comment exists in at least one of the two (T1)

## Tracey

References existing markers:
- `r[store.tenant.sign-key]` — T1 indirectly serves (the FK on `tenant_keys` is the referential integrity under the spec at [`store.md:188`](../../docs/src/components/store.md)). But the cascade behavior itself isn't a spec'd contract — it's implementation-correctness, not user-visible behavior.

No new markers. Tenant deletion semantics aren't spec'd yet (no tenant-lifecycle admin API exists). When one lands, IT gets the `r[store.tenant.delete-cascade]` marker; this plan just makes the schema ready.

## Files

```json files
[
  {"path": "migrations/017_tenant_keys_fk_cascade.sql", "action": "NEW", "note": "T1: DROP + ADD CONSTRAINT with ON DELETE CASCADE"},
  {"path": "migrations/014_tenant_keys.sql", "action": "MODIFY", "note": "T1 MAYBE: archaeology comment at :14 pointing to 017 — ONLY if sqlx checksums ignore comments (check at dispatch; skip if not)"},
  {"path": "rio-store/src/db.rs", "action": "MODIFY", "note": "T2: tenant_delete_cascades_keys_including_revoked test — OR in rio-store/tests/ if tenant_keys queries don't live in db.rs yet"}
]
```

```
migrations/
├── 014_tenant_keys.sql           # T1 MAYBE: archaeology comment (checksum-dependent)
└── 017_tenant_keys_fk_cascade.sql  # T1: NEW — DROP + ADD CASCADE
rio-store/src/db.rs               # T2: cascade regression test
```

## Dependencies

```json deps
{"deps": [249], "soft_deps": [256], "note": "Hard-dep P0249 (DONE): migration 014 exists, the constraint name tenant_keys_tenant_id_fkey exists. Soft-dep P0256: consumes tenant_keys heavily — if P0256 dispatches before this lands and adds a tenant-delete path, it'll hit the FK error. Coordinator noted 'P0256 may hit this'. P0256 is UNIMPL; sequencing this before P0256 avoids that. discovered_from=bughunter (no specific plan number in origin — pure schema audit). PG constraint-name auto-generation is deterministic (<table>_<col>_fkey for CREATE TABLE inline REFERENCES), but verify at dispatch against a TestDb pg_constraint query."}
```

**Depends on:** [P0249](plan-0249-migration-batch-014-015-016.md) merged (DONE) — migration 014 exists.
**Sequence before:** [P0256](plan-0256-per-tenant-signing-output-hash.md) — consumes `tenant_keys`; any tenant-delete path it adds would FK-fail without 017.
**Conflicts with:** `migrations/` directory — 017 is the next unallocated number (016 is the current max). If another plan claims 017 first, bump to 018. [`rio-store/src/db.rs`](../../rio-store/src/db.rs) not in collisions top-30. Low conflict risk — pure addition.
