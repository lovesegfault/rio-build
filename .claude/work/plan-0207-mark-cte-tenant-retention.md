# Plan 0207: mark CTE tenant-retention seed + quota query + cache_server auth TODO

Wave 1. Adds the **6th UNION arm** to the mark-phase CTE in [`mark.rs`](../../rio-store/src/gc/mark.rs): paths survive GC if *any* tenant's retention window still covers them. Union-of-retention semantics — the most generous tenant wins. Also adds the per-tenant store-bytes accounting query (accounting only — enforcement is phase 5) and closes the [`cache_server/auth.rs:36`](../../rio-store/src/cache_server/auth.rs) `TODO(phase4b)`.

**Decoupled from P0206 for unit testing:** an empty `path_tenants` table makes the new UNION arm a no-op (0 rows contributed). This plan's unit test seeds rows directly into the table, so it can develop in parallel with P0206. But the **VM test** needs P0206's upsert to produce real rows from actual build completions — merge-order P0206→P0207.

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync-markers.md) merged (markers `r[store.gc.tenant-retention]` and `r[store.gc.tenant-quota]` exist in `store.md`)

## Tasks

### T1 — `feat(store):` 6th UNION arm in mark CTE — tenant retention

At [`rio-store/src/gc/mark.rs`](../../rio-store/src/gc/mark.rs) after the existing 5th UNION arm (`:74`):

```sql
UNION
-- r[impl store.gc.tenant-retention]
-- Union-of-retention: path survives if ANY tenant's window covers it.
-- Global grace (narinfo.created_at) is a floor; this extends, never shortens.
SELECT n.store_path FROM narinfo n
  JOIN path_tenants pt USING (store_path_hash)
  JOIN tenants t ON t.tenant_id = pt.tenant_id
  WHERE pt.first_referenced_at > now() - make_interval(hours => t.gc_retention_hours)
```

Update doc-comment at `:101` (currently says "UNION of five seeds") → "UNION of six seeds (roots, live-pins, recent-narinfo, build-history, resign-pending, tenant-retention)".

### T2 — `feat(store):` per-tenant store-bytes query

NEW file `rio-store/src/gc/tenant.rs`:

```rust
//! Per-tenant store accounting. Phase 4b: accounting only.
//! Phase 5: enforcement (reject PutPath above quota, or tenant-scoped GC).

use sqlx::PgPool;
use uuid::Uuid;

// r[impl store.gc.tenant-quota]
/// Sum of `narinfo.nar_size` over all paths this tenant has referenced.
/// COALESCE(..., 0) so a tenant with zero paths returns 0 not NULL.
pub async fn tenant_store_bytes(pool: &PgPool, tenant_id: Uuid) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        r#"SELECT COALESCE(SUM(n.nar_size), 0)::bigint
           FROM narinfo n
           JOIN path_tenants pt USING (store_path_hash)
           WHERE pt.tenant_id = $1"#,
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await
}
```

At [`rio-store/src/gc/mod.rs:44`](../../rio-store/src/gc/mod.rs) (after existing `pub mod sweep;`): add `pub mod tenant;`.

### T3 — `fix(store):` close `cache_server/auth.rs:36` TODO

Change `AuthenticatedTenant(Option<String>)` (write-only today) to carry both:

```rust
pub struct AuthenticatedTenant {
    pub tenant_id: Option<Uuid>,
    pub tenant_name: Option<String>,
}
```

Lookup both from `tenants` table by `cache_token`. Keep it write-only for now (no readers yet). Replace `TODO(phase4b)` with:

```rust
// TODO(phase5): per-tenant narinfo filtering via
//   JOIN path_tenants WHERE tenant_id = $auth.tenant_id
```

### T4 — `test(store):` retention-window unit test

Ephemeral-PG seeded unit test:

1. Seed `narinfo` with 2 paths (`created_at` past global grace for both)
2. Seed `tenants` with retention_hours = 48
3. Seed `path_tenants`: path A with `first_referenced_at = now() - 24h` (inside window), path B with `first_referenced_at = now() - 72h` (outside)
4. Run mark → assert path A reachable, path B unreachable

Marker: `// r[verify store.gc.tenant-retention]`

### T5 — `test(vm):` lifecycle.nix — retention extends global grace

Extend `gc-sweep` subtest in [`nix/tests/scenarios/lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) (TAIL append, **after** P0206's assertion):

1. Build a path with a tenant SSH key (P0206's upsert fires)
2. Backdate `path_tenants.first_referenced_at` past `tenants.gc_retention_hours` AND `narinfo.created_at` past global grace → `TriggerGC` → **swept** (both windows expired)
3. Control: inside tenant window but past global grace → `TriggerGC` → **survives** (tenant retention extends global)

### T6 — `docs(store):` note the 6th arm in store.md

At [`docs/src/components/store.md`](../../docs/src/components/store.md) under the `r[store.gc.tenant-retention]` marker P0204 seeded: add a brief note that the CTE now has 6 UNION arms (if the marker text doesn't already say this — P0204's text should cover it, but verify).

## Exit criteria

- `/nbr .#ci` green
- Unit test: inside-window path retained, outside-window unreachable (direct seed, no P0206 dependency)
- VM test: tenant retention extends global grace (path past global grace but inside tenant window survives)
- `grep 'TODO(phase4b)' rio-store/src/cache_server/auth.rs` returns empty

## Tracey

References existing markers:
- `r[store.gc.tenant-retention]` — T1 implements (mark.rs UNION arm); T4+T5 verify
- `r[store.gc.tenant-quota]` — T2 implements (tenant.rs); T4 verifies (seeded test asserts `tenant_store_bytes` matches sum)

## Files

```json files
[
  {"path": "rio-store/src/gc/mark.rs", "action": "MODIFY", "note": "T1: 6th UNION arm after :74; update doc-comment at :101; r[impl store.gc.tenant-retention]"},
  {"path": "rio-store/src/gc/tenant.rs", "action": "NEW", "note": "T2: tenant_store_bytes query; r[impl store.gc.tenant-quota]"},
  {"path": "rio-store/src/gc/mod.rs", "action": "MODIFY", "note": "T2: add pub mod tenant; at :44"},
  {"path": "rio-store/src/cache_server/auth.rs", "action": "MODIFY", "note": "T3: AuthenticatedTenant → (tenant_id, tenant_name) tuple; replace TODO(phase4b) with TODO(phase5)"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T5: gc-sweep retention subtest (TAIL append after P0206's assertion)"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "T6: note 6 UNION arms if not covered by P0204"}
]
```

```
rio-store/src/
├── gc/
│   ├── mark.rs                    # T1: 6th UNION arm
│   ├── tenant.rs                  # T2 (NEW): tenant_store_bytes
│   └── mod.rs                     # T2: pub mod tenant;
└── cache_server/auth.rs           # T3: tenant_id tuple
nix/tests/scenarios/lifecycle.nix  # T5: retention VM test
docs/src/components/store.md       # T6: 6-arm note
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [206], "note": "Unit test self-seeds (develop parallel w/ P0206). VM test needs P0206 rows — merge-order P0206→P0207."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync-markers.md) — markers must exist. **Soft dep [P0206](plan-0206-path-tenants-migration-upsert.md)** — merge-order only: unit test seeds `path_tenants` directly (table empty is fine, migration 012 just needs to exist), but VM test needs P0206's completion-hook upsert to produce real rows from actual builds.

**Conflicts with:**
- [`lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) — P0206 also extends gc-sweep. **Merge P0206 first** (both TAIL-append, low risk, but P0207's assertion depends on P0206's rows).
- [`gc/mod.rs`](../../rio-store/src/gc/mod.rs) — P0212 adds `run_gc` fn elsewhere in the 465-line file; T2 adds 1 line at `:44`. Trivially mergeable.
- [`cache_server/auth.rs`](../../rio-store/src/cache_server/auth.rs) — P0217 wraps `tenant_name` in `NormalizedName`. **P0207 first** (T3 changes the struct shape), then P0217 rebases.
