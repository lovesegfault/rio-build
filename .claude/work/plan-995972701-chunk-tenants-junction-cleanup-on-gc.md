# Plan 995972701: chunk_tenants junction cleanup on GC — CASCADE is dead code

bughunter-mc91 found, bughunter-mc98 escalated (still unpromoted at mc=98, 7 merges floating). [`migrations/018_chunk_tenants.sql:21`](../../migrations/018_chunk_tenants.sql) declares `REFERENCES chunks(blake3_hash) ON DELETE CASCADE` — but rio-store **never hard-deletes** chunks. [`gc/sweep.rs:322`](../../rio-store/src/gc/sweep.rs) does `UPDATE chunks SET deleted=TRUE` (soft-delete); [`gc/mod.rs:596`](../../rio-store/src/gc/mod.rs) same. No `DELETE FROM chunks` anywhere in the codebase — `drain.rs` only deletes `pending_s3_deletes` rows after S3 success. The CASCADE trigger will **never fire**. Junction rows accumulate forever.

The correctness bug: [`find_missing_chunks_for_tenant`](../../rio-store/src/metadata/chunked.rs) at `:336-340` queries ONLY `chunk_tenants` — no `JOIN chunks`, no `WHERE deleted=FALSE`. A chunk soft-deleted by sweep + S3-deleted by drain still reports as **present** to any tenant that once uploaded it. Future worker-side chunker calls `FindMissingChunks` → false-present → skips `PutChunk` → manifest references a chunk whose S3 object is gone → `GetChunk` fails with `NotFound`.

Pre-[P0264](plan-0264-findmissingchunks-tenant-scope.md) the unscoped `find_missing_chunks` had the same no-deleted-filter flaw (checked `chunks WHERE blake3_hash=ANY` without `deleted=FALSE`), so P0264 **inherited** rather than introduced — but the migration author explicitly added CASCADE without verifying chunks are hard-deleted. The `:17` comment ("ON DELETE CASCADE: GC sweeping a chunk drops all tenant attributions atomically") is aspirational.

[P0339](plan-0339-extract-enqueue-chunk-deletes.md) extracted `enqueue_chunk_deletes` at [`gc/mod.rs:504-541`](../../rio-store/src/gc/mod.rs) — called from BOTH [`decrement_and_enqueue:608`](../../rio-store/src/gc/mod.rs) and [`sweep_orphan_chunks:340`](../../rio-store/src/gc/sweep.rs), takes `zeroed: &[(Vec<u8>, i64)]` + `&mut Transaction`. Option-(b) fix is now **single-site**: `DELETE FROM chunk_tenants WHERE blake3_hash = ANY(...)` inside the helper alongside the `pending_s3_deletes` INSERT. Same tx, same `zeroed` set. P0339 made the fix cheaper, didn't apply it.

Three fix options (option-b recommended):

- **(a)** `JOIN chunks WHERE deleted=FALSE` in `find_missing_chunks_for_tenant` — ~3 lines, but leaves junk rows accumulating; the PG index still bloats.
- **(b)** DELETE junction rows in `enqueue_chunk_deletes` tx alongside the S3-enqueue — single-site, same tx as soft-delete, junction stays in sync with `chunks.deleted`. Chosen here.
- **(c)** `PutChunk` `ON CONFLICT DO UPDATE SET deleted=FALSE` (resurrect on re-upload) — fixes the broader PutChunk-does-not-resurrect gap at [`chunk.rs:248`](../../rio-store/src/grpc/chunk.rs), but doesn't clean junction rows; orthogonal and larger scope.

Option-(b) has one subtlety: the TOCTOU guard in drain ([`drain.rs`](../../rio-store/src/gc/drain.rs) re-checks `chunks.(deleted AND refcount=0)` before S3 DELETE) means a resurrected chunk won't be S3-deleted — but with (b) the junction row IS deleted (sweep tx commits before resurrection). Result: chunk in S3, `chunks` row resurrected, zero junction rows → tenant sees "missing" on next `FindMissingChunks` → re-uploads → new junction row. Self-healing, one extra `record_chunk_tenant` INSERT; same recovery path the [`:310-315`](../../rio-store/src/metadata/chunked.rs) doc-comment already describes for "chunk in chunks with zero chunk_tenants rows." The alternative (skip junction DELETE when drain might resurrect) requires predicting drain's TOCTOU outcome from inside sweep's tx — not possible without a second query. One redundant junction INSERT on the rare resurrection path is the right trade.

## Entry criteria

- [P0339](plan-0339-extract-enqueue-chunk-deletes.md) merged (`enqueue_chunk_deletes` helper exists — DONE)
- [P0264](plan-0264-findmissingchunks-tenant-scope.md) merged (`chunk_tenants` table + `find_missing_chunks_for_tenant` exist — DONE at mc=87)

## Tasks

### T1 — `fix(store):` DELETE chunk_tenants in enqueue_chunk_deletes

MODIFY [`rio-store/src/gc/mod.rs`](../../rio-store/src/gc/mod.rs) inside `enqueue_chunk_deletes` at `~:530`, after building `hashes: Vec<Vec<u8>>` and before the `pending_s3_deletes` INSERT:

```rust
    // Drop chunk_tenants junction rows for the zeroed chunks. The FK's
    // ON DELETE CASCADE (migrations/018) never fires because we
    // soft-delete chunks (UPDATE SET deleted=TRUE, never DELETE FROM).
    // Without this, junction rows accumulate forever and
    // find_missing_chunks_for_tenant (which queries ONLY chunk_tenants,
    // no JOIN chunks) reports a tombstoned chunk as present → worker
    // skips PutChunk → manifest references S3-deleted object → GetChunk
    // NotFound.
    //
    // TOCTOU w/ drain: a chunk resurrected by PutPath between sweep
    // and drain keeps its S3 object (drain re-checks deleted=TRUE
    // before issuing S3 DELETE) but LOSES its junction rows here.
    // Tenant sees "missing" on next FindMissingChunks → re-uploads →
    // record_chunk_tenant re-inserts. Self-healing — same recovery
    // path as "chunk with zero junction rows" (chunked.rs:310).
    //
    // Same tx as the S3-enqueue + soft-delete (callers hold the tx).
    // r[impl store.chunk.tenant-scoped]
    sqlx::query("DELETE FROM chunk_tenants WHERE blake3_hash = ANY($1)")
        .bind(&hashes)
        .execute(&mut **tx)
        .await?;
```

Note `&hashes` not `zeroed` — we already built the `Vec<Vec<u8>>` for the S3 INSERT; reuse it. Place AFTER the `if keys.is_empty() { return Ok(0) }` guard — empty-zeroed is a no-op; no junction-delete wasted RTT.

**Subtle:** `enqueue_chunk_deletes` early-returns `Ok(0)` when `backend.is_none()` (inline-only store). An inline-only store has no `PutChunk` clients (`require_cache()` returns `FAILED_PRECONDITION` at [`chunk.rs`](../../rio-store/src/grpc/chunk.rs)), so no junction rows ever exist — the early-return is correct for (b) too. No need to hoist the junction DELETE before the backend guard.

### T2 — `fix(store):` migration-018 CASCADE comment — honest

MODIFY [`migrations/018_chunk_tenants.sql`](../../migrations/018_chunk_tenants.sql) at `:17-18`. Current:

```sql
-- integrity. ON DELETE CASCADE: GC sweeping a chunk drops all tenant
-- attributions atomically.
```

Replace with:

```sql
-- integrity. ON DELETE CASCADE is belt-and-suspenders: GC soft-deletes
-- chunks (UPDATE SET deleted=TRUE, never DELETE FROM), so the CASCADE
-- trigger doesn't fire in practice. Junction rows are explicitly
-- DELETEd by enqueue_chunk_deletes (gc/mod.rs) in the same tx as the
-- soft-delete. The FK + CASCADE guard against a future hard-delete path.
```

**Do NOT remove the CASCADE.** If someone adds a hard-delete later (e.g., a "purge" admin RPC), the FK enforces the invariant. The bug was the comment claiming it did the work; the fix is honest docs + explicit DELETE.

### T3 — `test(store):` regression — find_missing post-sweep reports missing

NEW test in [`rio-store/src/gc/mod.rs`](../../rio-store/src/gc/mod.rs) `#[cfg(test)] mod tests` after `enqueue_skips_corrupt_hash` (`~:745`):

```rust
/// Regression: chunk_tenants junction rows must drop when a chunk is
/// soft-deleted. Without the DELETE in enqueue_chunk_deletes, the
/// junction accumulates forever AND find_missing_chunks_for_tenant
/// (which queries ONLY chunk_tenants, no JOIN chunks) reports the
/// tombstoned chunk as present → worker skips PutChunk → manifest
/// references S3-deleted object.
///
/// Shape: seed chunk + junction row → enqueue_chunk_deletes →
/// assert junction row gone → assert find_missing says MISSING.
// r[verify store.chunk.tenant-scoped]
#[tokio::test]
async fn enqueue_drops_junction_rows() {
    let db = TestDb::new(crate::MIGRATOR).await;
    let backend: Arc<dyn ChunkBackend> = Arc::new(MemoryChunkBackend::default());

    // Seed a chunk at refcount=0 (post-decrement state).
    let hash = seed_chunk(&db.pool, 0xCA, 0, 100).await;

    // Seed tenant + junction row. FK requires tenant exists.
    let tid: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO tenants (tenant_name) VALUES ('junction-test') \
         RETURNING tenant_id",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO chunk_tenants (blake3_hash, tenant_id) VALUES ($1, $2)")
        .bind(&hash[..])
        .bind(tid)
        .execute(&db.pool)
        .await
        .unwrap();

    // Precondition self-check: junction row exists → find_missing
    // says PRESENT. Proves the test isn't vacuous (junction seed worked).
    let pre = crate::metadata::find_missing_chunks_for_tenant(
        &db.pool, &[hash.to_vec()], tid,
    )
    .await
    .unwrap();
    assert_eq!(pre, vec![false], "precondition: chunk should be present pre-sweep");

    // Act: enqueue_chunk_deletes on the "zeroed" chunk.
    let mut tx = db.pool.begin().await.unwrap();
    let zeroed = vec![(hash.to_vec(), 100i64)];
    enqueue_chunk_deletes(&mut tx, &zeroed, Some(&backend))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // Junction row gone.
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chunk_tenants WHERE blake3_hash = $1",
    )
    .bind(&hash[..])
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(rows, 0, "junction row should be deleted in same tx as S3 enqueue");

    // End-to-end: find_missing now says MISSING for this tenant.
    let post = crate::metadata::find_missing_chunks_for_tenant(
        &db.pool, &[hash.to_vec()], tid,
    )
    .await
    .unwrap();
    assert_eq!(post, vec![true], "tombstoned chunk must report missing");
}
```

Mutation check: comment out T1's DELETE → `rows == 1` + `post == vec![false]` — both asserts fail.

### T4 — `docs:` multi-tenancy.md — FindMissingChunks scoping is SHIPPED

MODIFY [`docs/src/multi-tenancy.md`](../../docs/src/multi-tenancy.md) at `:98`. Current `> **Current state:**` blockquote says scoping is "not configurable... deferred to Phase 5 if a use case emerges." P0264 shipped it via `chunk_tenants` junction. Replace with:

```markdown
> **Current state:** per-tenant scoping is implemented via the
> `chunk_tenants` junction table (migration 018). `FindMissingChunks`
> and `PutChunk` both require a JWT with `Claims.sub` (fail-closed on
> missing — `UNAUTHENTICATED`). A chunk is reported present to a
> tenant IFF that tenant has a junction row; different tenants can
> share the same `chunks` row (dedup preserved) while each sees only
> their own uploads. The global chunk namespace (option 1 above) is
> no longer available.
```

Also bump [`phases-archive/phase4.md:49`](../../docs/src/phases-archive/phase4.md) and [`phase5.md:24`](../../docs/src/phases-archive/phase5.md) — both list FindMissingChunks per-tenant scoping as **deferred**; mark DONE (P0264).

## Exit criteria

- `/nixbuild .#ci` green
- `grep 'DELETE FROM chunk_tenants' rio-store/src/gc/mod.rs` → 1 hit (T1)
- `grep 'belt-and-suspenders\|soft-deletes chunks' migrations/018_chunk_tenants.sql` → ≥1 hit (T2: honest comment)
- `cargo nextest run -p rio-store enqueue_drops_junction_rows` → 1 passed (T3)
- **T3 mutation:** revert T1 (comment out the DELETE) → `cargo nextest run -p rio-store enqueue_drops_junction_rows` → FAIL with `rows == 1` message. Proves the test catches the regression.
- `grep 'deferred to Phase 5\|not configurable' docs/src/multi-tenancy.md | grep -i FindMissing` → 0 hits (T4: stale note removed)
- `nix develop -c tracey query rule store.chunk.tenant-scoped` → shows ≥1 `impl` + ≥1 `verify` site (T1 + T3)
- Existing tests still pass: `cargo nextest run -p rio-store gc::` — `zeroes_and_enqueues_s3`, `sweep_chunked_path_decrements_and_enqueues`, `enqueue_skips_corrupt_hash` all green

## Tracey

Adds new marker to component spec (see ## Spec additions):
- `r[store.chunk.tenant-scoped]` → [`docs/src/components/store.md`](../../docs/src/components/store.md) after the `r[store.chunk.grace-ttl]` paragraph (~:88). T1 `r[impl]`, T3 `r[verify]`.

References existing marker:
- `r[store.gc.pending-deletes]` — [`store.md:263`](../../docs/src/components/store.md). T1 modifies `enqueue_chunk_deletes` which carries this annotation at [`:503`](../../rio-store/src/gc/mod.rs); the junction DELETE is step 1b of the outbox pattern (drop tenant attribution alongside S3 enqueue). No new annotation — the fix is inside the existing impl site.

## Spec additions

**New `r[store.chunk.tenant-scoped]`** — goes to [`docs/src/components/store.md`](../../docs/src/components/store.md) after `:87` (the `r[store.chunk.grace-ttl]` paragraph), standalone, blank line before, col 0:

```
r[store.chunk.tenant-scoped]

`FindMissingChunks` is tenant-scoped via the `chunk_tenants` junction: a chunk is reported present to tenant X IFF a `(blake3_hash, tenant_id=X)` row exists. GC MUST delete junction rows in the same transaction as chunk soft-delete (the `chunks.blake3_hash` FK has `ON DELETE CASCADE`, but chunks are soft-deleted — `UPDATE SET deleted=TRUE`, never `DELETE` — so CASCADE never fires). A stale junction row for a tombstoned chunk would report false-present, causing the worker to skip `PutChunk` for a chunk whose S3 object is already deleted.
```

## Files

```json files
[
  {"path": "rio-store/src/gc/mod.rs", "action": "MODIFY", "note": "T1: DELETE FROM chunk_tenants inside enqueue_chunk_deletes ~:530; T3: enqueue_drops_junction_rows test ~:745"},
  {"path": "migrations/018_chunk_tenants.sql", "action": "MODIFY", "note": "T2: :17-18 CASCADE comment — belt-and-suspenders, honest about soft-delete"},
  {"path": "docs/src/multi-tenancy.md", "action": "MODIFY", "note": "T4: :98 Current-state blockquote — scoping SHIPPED via chunk_tenants"},
  {"path": "docs/src/phases-archive/phase4.md", "action": "MODIFY", "note": "T4: :49 FindMissingChunks scoping deferred → DONE (P0264)"},
  {"path": "docs/src/phases-archive/phase5.md", "action": "MODIFY", "note": "T4: :24 same deferred → DONE"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "Spec addition: r[store.chunk.tenant-scoped] marker after :87"}
]
```

```
rio-store/src/gc/
└── mod.rs                       # T1: DELETE in enqueue_chunk_deletes; T3: regression test
migrations/018_chunk_tenants.sql # T2: honest CASCADE comment
docs/src/
├── components/store.md          # Spec: +r[store.chunk.tenant-scoped]
├── multi-tenancy.md             # T4: scoping SHIPPED
└── phases-archive/
    ├── phase4.md                # T4: deferred → DONE
    └── phase5.md                # T4: deferred → DONE
```

## Dependencies

```json deps
{"deps": [339, 264], "soft_deps": [344, 345], "note": "Bughunter-mc91 found, mc98 escalated. P0339 extracted enqueue_chunk_deletes — single-site fix now. P0264 created chunk_tenants + find_missing_chunks_for_tenant — the bug's precondition. Both DONE. gc/mod.rs not in collisions-top-50 (P0339 was last touch). Soft-conflict P0344/P0345: both touch rio-store but different files (put_path_batch.rs + validate_metadata extraction, not gc/). discovered_from=bughunter (mc91+mc98)."}
```

**Depends on:** [P0339](plan-0339-extract-enqueue-chunk-deletes.md) — `enqueue_chunk_deletes` helper at [`gc/mod.rs:504`](../../rio-store/src/gc/mod.rs) is the single fix site; both GC paths (decrement + sweep) route through it. [P0264](plan-0264-findmissingchunks-tenant-scope.md) — `chunk_tenants` table + the false-present query exist.

**Conflicts with:** None hot. `gc/mod.rs` last touched by P0339 (DONE). [`migrations/018_chunk_tenants.sql`](../../migrations/018_chunk_tenants.sql) — [P0295](plan-0295-doc-rot-batch-sweep.md) T40 fixes the OTHER stale comment at `:16` (same-txn vs autocommit) — adjacent lines, coordinate at dispatch or sequence P0295-T40 before T2 here. [`docs/src/components/store.md`](../../docs/src/components/store.md) — count=unknown-low, additive marker insert at `:88`.
