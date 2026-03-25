# Plan 941405703: store GC — drain TOCTOU lock + sweep path_tenants + cycle reclaim

Three correctness gaps in the store GC, all discovered by bughunter sweep
(report `bug_031` at `/tmp/bughunter/prompt-6701aef0_2/reports/`).

**Gap 1 — drain TOCTOU permanent data loss.** The `still_dead` re-check at
[`rio-store/src/gc/drain.rs:111`](../../rio-store/src/gc/drain.rs) uses a
plain `SELECT (deleted AND refcount = 0)`, not `SELECT ... FOR UPDATE`. The
comment at :95-109 acknowledges "PutPath can still flip it between this
check and the S3 delete" but dismisses the window as "small". It isn't:
if PutPath resurrects the chunk between the SELECT and the S3 DELETE, PG
says the chunk is alive (`refcount ≥ 1`) but S3 no longer has the object.
Subsequent PutPaths see `refcount ≥ 2` via the upsert and skip upload —
**permanent data loss**. `FOR UPDATE` serializes the re-check with the
upsert's `ON CONFLICT ... DO UPDATE` row lock.

**Gap 2 — sweep leaves orphaned path_tenants rows.** Step 2a at
[`rio-store/src/gc/sweep.rs:198-214`](../../rio-store/src/gc/sweep.rs)
deletes FK-less `realisations` rows but NOT `path_tenants` (migration
[`012_path_tenants.sql`](../../migrations/012_path_tenants.sql), also
FK-less to `narinfo`). Orphaned `path_tenants` rows survive the sweep; when
a different tenant later uploads the same store path, the stale row grants
wrong-tenant visibility via `r[store.gc.tenant-retention]`'s JOIN.

**Gap 3 — A↔B cycles + self-refs leak forever.** The reference re-check at
[`sweep.rs:175-183`](../../rio-store/src/gc/sweep.rs) queries "does any
`narinfo` row reference this path?" without excluding the current
unreachable batch. If A references B and B references A, and both are in
the unreachable set, the re-check sees A→B and skips B, sees B→A and skips
A — neither is ever swept. Same for self-references. Fix: add `AND
store_path_hash <> ALL($batch)` to exclude intra-batch referrers.

## Tasks

### T1 — `fix(store):` drain re-check uses SELECT FOR UPDATE

At [`drain.rs:111`](../../rio-store/src/gc/drain.rs), change:

```sql
SELECT (deleted AND refcount = 0) FROM chunks WHERE blake3_hash = $1
```

to:

```sql
SELECT (deleted AND refcount = 0) FROM chunks WHERE blake3_hash = $1 FOR UPDATE
```

Update the comment block at :95-109: remove the "window is small"
dismissal, replace with "FOR UPDATE serializes this re-check with
concurrent PutPath upserts — the upsert's ON CONFLICT row lock blocks
until this tx commits or rolls back, so a resurrection-between-check-and-
S3-delete is impossible." Annotate with
`// r[impl store.gc.pending-deletes]` (existing marker — the spec text
at store.md:260 already says "re-checks the chunk state"; this fix makes
the re-check race-free).

### T2 — `fix(store):` sweep step 2a' deletes path_tenants

After step 2a (realisations DELETE, sweep.rs:214) and before step 2b
(narinfo DELETE, sweep.rs:219), insert:

```rust
// Step 2a': DELETE path_tenants for this path. NOT via CASCADE —
// path_tenants has NO FK to narinfo (012_path_tenants.sql). Without
// this explicit DELETE, orphaned rows grant wrong-tenant visibility
// when a different tenant later re-uploads the same store path.
// r[impl store.gc.sweep-path-tenants]
sqlx::query("DELETE FROM path_tenants WHERE store_path_hash = $1")
    .bind(store_path_hash)
    .execute(&mut *tx)
    .await?;
```

### T3 — `fix(store):` sweep re-check excludes unreachable batch

At [`sweep.rs:175-183`](../../rio-store/src/gc/sweep.rs), the re-check
query gains an exclusion arm. The batch of `store_path_hash` values being
swept is already in scope (the outer loop iterates it); bind it as
`&[&[u8]]` or `Vec<Vec<u8>>` depending on the existing binding style:

```sql
SELECT EXISTS (
  SELECT 1 FROM narinfo
   WHERE (SELECT store_path FROM narinfo WHERE store_path_hash = $1)
         = ANY("references")
     AND store_path_hash <> ALL($2)
   LIMIT 1
)
```

`$2` binds the current batch's hash slice. Annotate with
`// r[impl store.gc.sweep-cycle-reclaim]`.

### T4 — `test(store):` drain TOCTOU regression test

New test in `rio-store/src/gc/drain.rs` `#[cfg(test)]` (or the existing
integration test module): simulate the race by (a) enqueueing a
`pending_s3_deletes` row for chunk X, (b) spawning a task that upserts
chunk X (resurrection) concurrent with (c) `drain_once`. With `FOR
UPDATE`, the upsert blocks until drain's tx commits — assert the S3
delete was skipped (`rio_store_gc_chunk_resurrected_total` incremented)
OR the upsert saw `refcount = 0` and re-uploaded. Either outcome is
correct; permanent loss is not. Mark
`// r[verify store.gc.pending-deletes]`.

### T5 — `test(store):` sweep path_tenants cleanup + A↔B cycle reclaim

Two tests in the sweep test module:

- `test_sweep_deletes_path_tenants` — insert narinfo row + path_tenants
  row for same hash, mark unreachable, sweep, assert `path_tenants` row
  is gone. `// r[verify store.gc.sweep-path-tenants]`
- `test_sweep_reclaims_two_cycle` — insert A→B and B→A, mark both
  unreachable, sweep, assert both narinfo rows deleted (not stuck at
  `paths_resurrected`). Also cover self-reference (A→A).
  `// r[verify store.gc.sweep-cycle-reclaim]`

## Exit criteria

- `/nixbuild .#ci` green
- `grep -c 'path_tenants' rio-store/src/gc/sweep.rs` ≥ 1
- `grep 'FOR UPDATE' rio-store/src/gc/drain.rs` shows the re-check query
- `grep '<> ALL' rio-store/src/gc/sweep.rs` shows batch exclusion
- `cargo nextest run -p rio-store gc` shows 3 new tests passing
- `tracey query rule store.gc.sweep-path-tenants` + `store.gc.sweep-cycle-reclaim` show impl + verify

## Tracey

References existing markers:
- `r[store.gc.pending-deletes]` — T1 hardens the drain re-check this marker specifies; T4 verifies
- `r[store.gc.two-phase]` — T3 touches the sweep re-check described in this marker's Phase 2 text

Adds new markers to component specs:
- `r[store.gc.sweep-path-tenants]` → `docs/src/components/store.md` (see ## Spec additions) — T2 implements, T5 verifies
- `r[store.gc.sweep-cycle-reclaim]` → `docs/src/components/store.md` (see ## Spec additions) — T3 implements, T5 verifies

## Spec additions

Add to `docs/src/components/store.md` after line 214 (after the orphan
cleanup paragraph, before `r[store.gc.tenant-retention]`):

```markdown
r[store.gc.sweep-path-tenants]
Sweep MUST delete `path_tenants` rows for each swept `store_path_hash`
in the same transaction as the `narinfo` DELETE. `path_tenants` has no
FK CASCADE to `narinfo` (migration 012); without explicit cleanup,
orphaned rows survive the sweep and grant wrong-tenant visibility when
a different tenant later re-uploads the same store path (the stale row
still JOINs in the `r[store.gc.tenant-retention]` CTE arm).

r[store.gc.sweep-cycle-reclaim]
The sweep-phase reference re-check MUST exclude referrers that are
themselves in the current unreachable batch. Without this exclusion,
mutual-reference cycles (A→B, B→A) and self-references (A→A) are never
swept: the re-check sees an intra-batch referrer and skips both paths
forever. The exclusion is `AND store_path_hash <> ALL($batch_hashes)`.
```

## Files

```json files
[
  {"path": "rio-store/src/gc/drain.rs", "action": "MODIFY", "note": "T1: SELECT FOR UPDATE on still_dead re-check; T4: TOCTOU regression test"},
  {"path": "rio-store/src/gc/sweep.rs", "action": "MODIFY", "note": "T2: step 2a' path_tenants DELETE; T3: batch exclusion on re-check; T5: 2 tests"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "2 new markers: sweep-path-tenants, sweep-cycle-reclaim"}
]
```

```
rio-store/src/gc/
├── drain.rs                          # T1+T4: FOR UPDATE + test
└── sweep.rs                          # T2+T3+T5: path_tenants DELETE, batch exclusion, tests
docs/src/components/
└── store.md                          # 2 new markers
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "standalone GC correctness — no upstream deps"}
```

**Depends on:** none. All three fixes are self-contained SQL/logic changes
inside the existing GC flow.

**Conflicts with:** `rio-store/src/gc/drain.rs` and `sweep.rs` are not in
collisions top-30. No serialization needed. Independent of
[P941405701](plan-941405701-nar-entry-name-validation.md) and
[P941405702](plan-941405702-framed-total-nar-size-align.md) (different
crates).
