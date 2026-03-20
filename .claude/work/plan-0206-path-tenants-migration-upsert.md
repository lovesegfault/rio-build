# Plan 0206: path_tenants — migration 012 + scheduler upsert + completion hook

**Critical path head for phase 4b.** The `tenants.gc_retention_hours` column (migration [`009_phase4.sql:16`](../../migrations/009_phase4.sql), default 168h) is **dead data today** — `rio-cli create-tenant --gc-retention-hours 720` sets it but [`mark.rs`](../../rio-store/src/gc/mark.rs) never reads it. Every tenant gets the global grace regardless of their configured retention. This plan creates the `path_tenants` join table and the scheduler-side upsert that populates it on build completion. P0207 then wires the table into the mark CTE.

**Assumption A1 accepted:** migration is a **NEW file** `012_path_tenants.sql`, NOT a 009 append. Migrations 010+011 already exist; `sqlx::migrate!` at [`rio-store/src/lib.rs:34`](../../rio-store/src/lib.rs) is checksum-checked. The 009 header comment at line 7 ("Part C appended later") was the design intent BEFORE 010/011 landed — that intent is overtaken. Appending to 009 would break every persistent DB with `VersionMismatch`. Migration 012 header documents this.

**Assumption A5 accepted:** `narinfo.tenant_id` drop is safe in the same migration. Grep across `rio-*/src/**/*.rs` for `narinfo.*tenant_id` returns empty — zero Rust readers. Column added at [`migrations/002_store.sql:30`](../../migrations/002_store.sql), never queried. The [`cache_server/auth.rs:36`](../../rio-store/src/cache_server/auth.rs) TODO reads `tenants.cache_token`, not `narinfo.tenant_id`. If a hidden reader exists, migration fails fast at CI with a compile error.

If the tenant-lookup at `completion.rs` turns out to need a different borrow pattern than `self.builds.get(build_id).and_then(|b| b.tenant_id)`, downstream P0207's VM test reshapes — report early.

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync-markers.md) merged (marker `r[sched.gc.path-tenants-upsert]` exists in `scheduler.md`)

## Tasks

### T1 — `feat(store):` migration 012 — path_tenants table + narinfo.tenant_id drop

NEW file `migrations/012_path_tenants.sql`:

```sql
-- migration 012: path_tenants join table for per-tenant GC retention.
--
-- WHY 012 NOT 009-APPEND: The 009 header said "Part C appended later",
-- but migrations 010+011 landed after 009. sqlx::migrate! is checksum-
-- checked — even a comment change to 009 breaks every persistent DB
-- with VersionMismatch. 012 is the only checksum-safe path.
--
-- WHY NO FK→narinfo: follows migrations/007_live_pins.sql precedent —
-- path_tenants references paths that MAY not be in narinfo yet (upsert
-- fires at completion, narinfo PUT may lag).
--
-- narinfo.tenant_id drop: column added at 002:30, zero Rust readers
-- (grep empty). Replaced by this N:M join table.

CREATE TABLE path_tenants (
    store_path_hash   BYTEA       NOT NULL,
    tenant_id         UUID        NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    first_referenced_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (store_path_hash, tenant_id)
);

CREATE INDEX path_tenants_retention_idx ON path_tenants (tenant_id, first_referenced_at);

ALTER TABLE narinfo DROP COLUMN tenant_id;
```

Do NOT touch [`migrations/002_store.sql:30`](../../migrations/002_store.sql) comment — checksum-locked. The 012 header covers it.

### T2 — `feat(scheduler):` `SchedulerDb::upsert_path_tenants`

Add to [`rio-scheduler/src/db.rs`](../../rio-scheduler/src/db.rs) after `pin_live_inputs` (`:591-625` is the pattern — SHA-256 + UNNEST batch):

```rust
/// Upsert (output_path, tenant_id) cartesian product into path_tenants.
/// SHA-256 each path (matching narinfo.store_path_hash semantics).
/// ON CONFLICT DO NOTHING — composite PK (store_path_hash, tenant_id).
/// Best-effort: caller warns on Err but does NOT fail completion.
pub async fn upsert_path_tenants(
    &self,
    output_paths: &[String],
    tenant_ids: &[Uuid],
) -> Result<u64, sqlx::Error> {
    // Cartesian product: every path × every tenant
    let mut hashes: Vec<Vec<u8>> = Vec::with_capacity(output_paths.len() * tenant_ids.len());
    let mut tids: Vec<Uuid> = Vec::with_capacity(output_paths.len() * tenant_ids.len());
    for p in output_paths {
        let h = sha2::Sha256::digest(p.as_bytes()).to_vec();  // per db.rs:602 pattern
        for t in tenant_ids {
            hashes.push(h.clone());
            tids.push(*t);
        }
    }
    let result = sqlx::query(
        r#"INSERT INTO path_tenants (store_path_hash, tenant_id)
           SELECT * FROM unnest($1::bytea[], $2::uuid[])
           ON CONFLICT DO NOTHING"#,
    )
    .bind(&hashes)
    .bind(&tids)
    .execute(&self.pool)
    .await?;
    Ok(result.rows_affected())
}
```

### T3 — `feat(scheduler):` call upsert from completion

At [`rio-scheduler/src/actor/completion.rs`](../../rio-scheduler/src/actor/completion.rs) after `:377` `trigger_log_flush`:

```rust
// r[impl sched.gc.path-tenants-upsert]
// Best-effort: GC may under-retain on failure, but never fail completion.
let tenant_ids: Vec<Uuid> = interested_builds
    .iter()
    .filter_map(|id| self.builds.get(id)?.tenant_id)  // Option<Uuid> at state/build.rs:104
    .collect();
if !tenant_ids.is_empty() && !output_paths.is_empty() {
    if let Err(e) = self.db.upsert_path_tenants(&output_paths, &tenant_ids).await {
        warn!(?e, output_paths = output_paths.len(), tenants = tenant_ids.len(),
              "path_tenants upsert failed; GC retention may under-retain");
    }
}
```

### T4 — `test(scheduler):` dedup + idempotence unit test

In `rio-scheduler/src/actor/tests/completion.rs` (ephemeral PG via `rio-test-support`):

1. Create 2 tenants, submit 1 derivation from both (dedup → 1 execution)
2. Completion → assert `SELECT count(*) FROM path_tenants WHERE store_path_hash = sha256(convert_to($out, 'UTF8'))` = **2** (same hash, distinct tenant_id) — (NOT `digest()` — that's pgcrypto, which is not enabled. `sha256()` is the PG11+ builtin; `convert_to()` gets the bytea it wants.)
3. Call `upsert_path_tenants` again with same inputs → `rows_affected() == 0` (ON CONFLICT DO NOTHING)

Marker: `// r[verify sched.gc.path-tenants-upsert]`

### T5 — `test(vm):` lifecycle.nix assertion

Extend the `gc-sweep` subtest in [`nix/tests/scenarios/lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) near `:1800` (TAIL append): after building with a tenant SSH key, assert `SELECT count(*) FROM path_tenants WHERE store_path_hash = sha256(convert_to('$out', 'UTF8'))` ≥ 1. (The `≥ 1` not `= 1` because other VM test paths may also reference it.)

## Exit criteria

- `/nbr .#ci` green
- Ephemeral-PG unit test: 2 tenants × 3 paths = 6 rows inserted; re-call → 0 new rows (ON CONFLICT DO NOTHING)
- VM test: `path_tenants` row appears post-build with correct `store_path_hash`

## Tracey

References existing markers:
- `r[sched.gc.path-tenants-upsert]` — T3 implements this (call site at completion.rs); T4 verifies (unit test)

## Files

```json files
[
  {"path": "migrations/012_path_tenants.sql", "action": "NEW", "note": "T1: CREATE TABLE path_tenants + INDEX + ALTER narinfo DROP tenant_id"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "T2: add upsert_path_tenants after pin_live_inputs (:591-625 pattern)"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T3: call upsert after :377 trigger_log_flush; r[impl sched.gc.path-tenants-upsert]"},
  {"path": "rio-scheduler/src/actor/tests/completion.rs", "action": "MODIFY", "note": "T4: 2-tenant dedup + idempotence test; r[verify]"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T5: gc-sweep subtest assertion near :1800"}
]
```

```
migrations/
└── 012_path_tenants.sql           # T1 (NEW)
rio-scheduler/src/
├── db.rs                          # T2: upsert_path_tenants
└── actor/
    ├── completion.rs              # T3: call site at :377+
    └── tests/completion.rs        # T4: r[verify]
nix/tests/scenarios/lifecycle.nix  # T5: VM assertion
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [], "note": "Wave-1 critical-path head. Merge before P0207 (VM test needs upsert to produce rows)."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync-markers.md) — marker `r[sched.gc.path-tenants-upsert]` must exist before `r[impl]` annotation.

**Conflicts with:**
- [`completion.rs`](../../rio-scheduler/src/actor/completion.rs) — P0219 also touches it (`:68` vs `:377` — 300 lines apart, different functions). Low risk; parallel OK. Serialize if landing same-day.
- [`lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) — P0207 also extends gc-sweep. **Merge P0206 first** so P0207's VM test can assert positive row counts (empty `path_tenants` makes the retention arm a no-op).
- [`db.rs`](../../rio-scheduler/src/db.rs) — P0204's `TODO(phase4c)` retag is at `:9`, T2 adds at `:591+`. Distant, no conflict.
