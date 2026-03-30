# Plan 984472801: GC sort-before-ANY() deadlock helper — consolidate P0475's discipline

[P0475](plan-0475-pg-deadlock-chunk-rollback.md) fixed the chunk-rollback deadlock (SQLSTATE 40P01) at [`metadata/chunked.rs:253-278`](../../rio-store/src/metadata/chunked.rs) by sorting `chunk_hashes` before `UPDATE ... WHERE blake3_hash = ANY($1)` and wrapping the txn in a single-retry-on-40P01 loop. The fix is correct but **local** — the consolidator found three more `ANY($1)` batch-UPDATE sites with unsorted input that have the same circular-wait shape:

| Site | Input | Concurrent writer | Deadlock shape |
|---|---|---|---|
| [`gc/mod.rs:627`](../../rio-store/src/gc/mod.rs) | `unique_hashes` from `HashSet` iteration (NONDETERMINISTIC order) | concurrent `decrement_refcounts_for_manifest` on overlapping manifest | txn A locks h1→h3, txn B locks h3→h1 |
| [`gc/mod.rs:637`](../../rio-store/src/gc/mod.rs) | same `unique_hashes` (second UPDATE in same txn) | same | same txn, compounds lock set |
| [`gc/sweep.rs:402`](../../rio-store/src/gc/sweep.rs) | `hashes` from `candidates.chunks()` batch slice (SELECT order, not sorted) | concurrent rollback (`delete_manifest_chunked_uploading`) on overlapping hashes | sweep locks in SELECT order, rollback locks in sort order → NOT the same order |
| [`gc/mod.rs:567`](../../rio-store/src/gc/mod.rs) | `hashes` rebuilt from `zeroed` (RETURNING set — PG does NOT preserve input-array order) | concurrent `enqueue_chunk_deletes` via the other GC path | DELETE on `chunk_tenants` + INSERT UNNEST on `pending_s3_deletes`, both unsorted |

The `gc/sweep.rs:402` case is the sharpest: P0475 made rollback sort its input, but sweep does NOT sort — so rollback and sweep acquire row locks in DIFFERENT orders. Overlapping hash sets → classic circular wait → 40P01. Concurrent GC + active uploads is a production-likely load pattern.

The `gc/mod.rs:567` case is a **second-order** gap (plan-reviewer caught it post-draft): even after T3/T4 sort the `UPDATE chunks` input, the RETURNING set (`zeroed`) that feeds `enqueue_chunk_deletes` comes back in PG's internal order, not input order. The `DELETE FROM chunk_tenants WHERE blake3_hash = ANY($1)` at `:567` and the `INSERT ... UNNEST ... ON CONFLICT` at `:572` both bind `&hashes` built by iterating `zeroed` — unsorted. The `GC_LOCK_ID` advisory lock at [`gc/mod.rs:195`](../../rio-store/src/gc/mod.rs) does NOT protect this: it serializes GC-vs-GC (`pg_try_advisory_lock` returns false for the second GC), but `delete_manifest_chunked_uploading` (rollback path) doesn't take it, and both GC paths call `enqueue_chunk_deletes`.

**Bundle:** the consolidator also flagged (a) `is_deadlock()` at `chunked.rs:21` as a `fn`-local helper that should be a `MetadataError` variant or classifier method (40P01 is a distinct error class, same tier as `Serialization` 40001 at [`metadata/mod.rs:80`](../../rio-store/src/metadata/mod.rs)), and (b) `jitter()` at `chunked.rs:28` as a near-dup of the scheduler's jitter pattern at [`state/executor.rs:260`](../../rio-scheduler/src/state/executor.rs) — both are "cheap pseudo-random backoff" but with different APIs. The scheduler one is richer (configurable fraction); the chunked.rs one is a fixed 50-150ms. Not unifying across crates (different semantics), but the rio-store one belongs in a shared module if GC paths need it too.

## Entry criteria

- [P0475](plan-0475-pg-deadlock-chunk-rollback.md) merged (DONE — `is_deadlock`, `jitter`, sort+retry pattern exist at `chunked.rs:19-278`)

## Tasks

### T1 — `refactor(store):` extract `sorted_any_update` helper to `metadata/mod.rs`

MODIFY [`rio-store/src/metadata/mod.rs`](../../rio-store/src/metadata/mod.rs). Add a pub helper that owns the sort+retry discipline:

```rust
/// Execute a batch `UPDATE ... WHERE <key> = ANY($1)` with deadlock-safe
/// lock ordering. Sorts the input before binding so all callers acquire
/// PG row locks in the same deterministic order (prevents circular wait
/// → SQLSTATE 40P01). Wraps in a single retry-on-40P01 loop: the sort
/// SHOULD prevent deadlock but PG can still hit it on index-page splits
/// under extreme contention; one retry is cheap, unbounded retry masks
/// real problems.
///
/// The `body` closure receives the SORTED slice and must perform the full
/// transaction (begin→UPDATE→commit). On 40P01, the closure is re-invoked
/// after jitter (PG aborts the whole txn on deadlock, not just the stmt).
pub async fn with_sorted_retry<T, F, Fut>(
    mut keys: Vec<Vec<u8>>,
    body: F,
) -> Result<T>
where
    F: Fn(&[Vec<u8>]) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    keys.sort_unstable();
    for attempt in 0..2 {
        match body(&keys).await {
            Err(MetadataError::Deadlock(e)) if attempt == 0 => {
                warn!(error = %e, "40P01 on batch UPDATE; retrying once after jitter");
                tokio::time::sleep(jitter()).await;
            }
            r => return r,
        }
    }
    unreachable!("loop returns or continues exactly once")
}
```

Move `jitter()` from `chunked.rs:28` to `metadata/mod.rs` as a crate-private `pub(crate) fn` alongside the helper. Keep the 50-150ms fixed range — this is PG-retry jitter, not scheduler backoff; unifying with `rio-scheduler`'s configurable fraction is NOT in scope (different crate, different semantics, cross-crate dep not justified for 4 lines).

### T2 — `fix(store):` add `MetadataError::Deadlock` variant + classify 40P01

MODIFY [`rio-store/src/metadata/mod.rs:57`](../../rio-store/src/metadata/mod.rs). Add a variant after `Serialization` (both are 40xxx transaction-abort codes, both retriable):

```rust
/// Deadlock detected (PG code 40P01). Two transactions have a circular
/// lock-wait on overlapping row sets. Retriable — PG aborted one txn;
/// retry will likely succeed. Prevention: sort batch-UPDATE input so
/// all writers acquire locks in the same order (see `with_sorted_retry`).
/// Maps to `aborted`.
#[error("deadlock detected (retry)")]
Deadlock(#[source] sqlx::Error),
```

Extend the `From<sqlx::Error>` impl to match `code() == Some("40P01")` → `Deadlock`, same pattern as the existing `40001` → `Serialization` arm. DELETE `is_deadlock()` at `chunked.rs:21-23` — callers match the variant instead.

### T3 — `fix(store):` apply sort discipline to GC refcount-decrement path

MODIFY [`rio-store/src/gc/mod.rs:613-644`](../../rio-store/src/gc/mod.rs). The `unique_hashes` `Vec` is built from `HashSet` iteration at `:613-621` — iteration order is nondeterministic across runs. Add `unique_hashes.sort_unstable();` after the dedup-collect, before the first UPDATE at `:627`. Both UPDATEs (`:627` decrement, `:637` mark-deleted) then bind a sorted slice.

This function runs inside a caller-provided `&mut Transaction` (not its own txn), so it CANNOT use `with_sorted_retry` (retry would need to replay the whole outer txn). The sort is the primary defense; retry is the caller's responsibility if they wrap the txn. Add a doc-comment noting this.

### T4 — `fix(store):` apply sort discipline to GC sweep orphan-chunk path

MODIFY [`rio-store/src/gc/sweep.rs:382`](../../rio-store/src/gc/sweep.rs). After `let hashes: Vec<Vec<u8>> = batch.iter().map(...).collect();`, add `let mut hashes = hashes; hashes.sort_unstable();` (or collect into a mut binding directly). The UPDATE at `:402` then binds sorted input.

This function DOES own its txn (`pool.begin()` at `:380`), so it CAN use `with_sorted_retry`. Refactor the per-batch body into the closure form:

```rust
for batch in candidates.chunks(SWEEP_BATCH_SIZE) {
    let hashes: Vec<Vec<u8>> = batch.iter().map(|(h, _)| h.clone()).collect();
    let (zd, bf) = with_sorted_retry(hashes, |sorted| async move {
        let mut tx = pool.begin().await?;
        let zeroed: Vec<(Vec<u8>, i64)> = sqlx::query_as(/* ... ANY($1) ... */)
            .bind(sorted)
            .fetch_all(&mut *tx).await?;
        super::enqueue_chunk_deletes(&mut tx, &zeroed, chunk_backend).await?;
        tx.commit().await?;
        Ok((zeroed.len() as u64, zeroed.iter().map(|(_, s)| *s as u64).sum()))
    }).await?;
    chunks_deleted += zd;
    bytes_freed += bf;
}
```

The closure lifetime may fight the borrow checker (`pool` + `chunk_backend` captured by reference across await). If so, fall back to inline sort + manual retry loop (same shape as `chunked.rs:269-278` before this plan's T5 refactors it).

### T5 — `refactor(store):` migrate `delete_manifest_chunked_uploading` to helper

MODIFY [`rio-store/src/metadata/chunked.rs:246-279`](../../rio-store/src/metadata/chunked.rs). Replace the hand-rolled sort+retry loop with a `with_sorted_retry` call wrapping `delete_manifest_chunked_uploading_inner`. The `_inner` fn already takes `&[Vec<u8>]` so the closure body is a direct pass-through. DELETE the now-unused `is_deadlock` + `jitter` local definitions (moved in T1/T2).

### T6 — `test(store):` concurrent GC-sweep vs rollback no-deadlock

NEW test at [`rio-store/src/gc/sweep.rs`](../../rio-store/src/gc/sweep.rs) `#[cfg(test)] mod`. Mirror the pattern of `rollback_overlapping_no_deadlock` at `chunked.rs:781`:

1. Seed 100 chunks with `refcount=0, created_at = now() - 1h` (GC-eligible)
2. Seed an uploading manifest referencing 50 of them (overlap set)
3. `tokio::join!` a sweep iteration and a `delete_manifest_chunked_uploading` call
4. `tokio::time::timeout(Duration::from_secs(5), joined)` — 40P01 manifests as the timeout tripping (PG's deadlock detector runs at 1s intervals, but a pre-fix run deadlocks before detection under the right interleaving)

The test is probabilistic (depends on PG's lock-acquisition interleaving), but with 100 rows and reversed iteration orders it trips reliably pre-fix. `// r[verify store.chunk.lock-order]`.

### T7 — `fix(store):` sort at `enqueue_chunk_deletes` entry — RETURNING order is not input order

MODIFY [`rio-store/src/gc/mod.rs`](../../rio-store/src/gc/mod.rs) `enqueue_chunk_deletes`. The `zeroed: &[(Vec<u8>, i64)]` parameter arrives in PG RETURNING order (nondeterministic). The function iterates it to build parallel `keys`/`hashes` vecs, then binds `&hashes` to both the `chunk_tenants` DELETE (`:567`) and the `pending_s3_deletes` UNNEST INSERT (`:572`) — both acquire row locks in that order.

Sort the input at function entry so both statements bind sorted arrays and `keys`/`hashes` stay index-aligned:

```rust
pub(crate) async fn enqueue_chunk_deletes(
    tx: &mut Transaction<'_, Postgres>,
    zeroed: &[(Vec<u8>, i64)],
    backend: &Option<Arc<dyn ChunkBackend>>,
) -> Result<u64> {
    // r[impl store.chunk.lock-order]
    // Sort by hash before building the parallel keys/hashes vecs: the
    // input is a RETURNING set (PG internal order, not input-array
    // order). Both the chunk_tenants DELETE and pending_s3_deletes
    // INSERT below bind ANY()/UNNEST() — unsorted → circular-wait
    // against concurrent enqueue_chunk_deletes or rollback. One sort
    // here covers both statements AND all callers (T3's decrement path,
    // T4's sweep path).
    let mut zeroed: Vec<_> = zeroed.to_vec();
    zeroed.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    // ... existing keys/hashes build loop + DELETE + INSERT ...
```

The `.to_vec()` clone is cheap relative to two PG roundtrips. If the clone is unacceptable, change the signature to `Vec<(Vec<u8>, i64)>` (owned) and sort in place — callers (T3 `decrement_refcounts_for_manifest` at `:648`, T4 sweep at `:419`) already have owned vecs they don't re-use.

## Exit criteria

- `with_sorted_retry` helper exists at `metadata/mod.rs` with doc-comment explaining the discipline
- `MetadataError::Deadlock` variant exists; `From<sqlx::Error>` matches 40P01
- `grep 'is_deadlock' rio-store/src/` → 0 hits (replaced by variant match)
- `grep 'fn jitter' rio-store/src/metadata/chunked.rs` → 0 hits (moved to mod.rs)
- `gc/mod.rs` `unique_hashes` is sorted before both ANY() UPDATEs
- `gc/sweep.rs` batch `hashes` is sorted before ANY() UPDATE (via helper or inline)
- `enqueue_chunk_deletes` sorts `zeroed` by hash before building `keys`/`hashes` (T7)
- `cargo nextest run -p rio-store sweep_vs_rollback_no_deadlock` passes
- `cargo nextest run -p rio-store rollback_overlapping_no_deadlock` still passes (T5 refactor didn't break it)

## Tracey

References existing markers:
- `r[store.chunk.refcount-txn]` — T3 is a correctness fix under this invariant (refcount updates must be atomic AND deadlock-free)
- `r[store.put.wal-manifest]` — T5 refactors the rollback path; existing `r[impl]` annotation at `chunked.rs:252` stays

Adds new marker to component spec:
- `r[store.chunk.lock-order]` → [`docs/src/components/store.md`](../../docs/src/components/store.md) (see ## Spec additions below)

## Spec additions

Add after `r[store.chunk.refcount-txn]` at [`store.md:87`](../../docs/src/components/store.md):

```
r[store.chunk.lock-order]
All batch row-locking statements keyed on `blake3_hash` (`UPDATE chunks ... WHERE blake3_hash = ANY($1)`, `DELETE FROM chunk_tenants WHERE blake3_hash = ANY($1)`, `INSERT ... ON CONFLICT` with UNNEST over hash arrays) MUST bind a sorted input array. PostgreSQL acquires row locks in ANY()/UNNEST scan order; unsorted overlapping sets across concurrent transactions create circular lock-wait → SQLSTATE 40P01. Sorting makes lock-acquisition order deterministic across all writers. Note: a RETURNING set is NOT in input-array order — re-sort before passing to downstream ANY() statements. A single defensive retry on 40P01 is permitted (index-page splits can still deadlock under extreme contention); unbounded retry is NOT permitted (masks real lock-order bugs).
```

## Files

```json files
[
  {"path": "rio-store/src/metadata/mod.rs", "action": "MODIFY", "note": "T1: with_sorted_retry helper + jitter move. T2: Deadlock variant + 40P01 classifier"},
  {"path": "rio-store/src/metadata/chunked.rs", "action": "MODIFY", "note": "T5: migrate to helper, delete is_deadlock+jitter locals"},
  {"path": "rio-store/src/gc/mod.rs", "action": "MODIFY", "note": "T3: sort unique_hashes before ANY() at :627,:637. T7: sort zeroed at enqueue_chunk_deletes entry"},
  {"path": "rio-store/src/gc/sweep.rs", "action": "MODIFY", "note": "T4: sort batch hashes, wrap in with_sorted_retry. T6: sweep-vs-rollback deadlock test"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "Spec additions: r[store.chunk.lock-order] marker after :87"}
]
```

```
rio-store/src/
├── metadata/
│   ├── mod.rs         # T1 helper+jitter, T2 Deadlock variant
│   └── chunked.rs     # T5 migrate to helper
└── gc/
    ├── mod.rs         # T3 sort unique_hashes, T7 sort zeroed in enqueue_chunk_deletes
    └── sweep.rs       # T4 sort+retry, T6 test
docs/src/components/
└── store.md           # new r[store.chunk.lock-order] marker
```

## Dependencies

```json deps
{"deps": [475], "soft_deps": [], "note": "P0475 (DONE) shipped the sort+retry pattern at chunked.rs; this plan extracts it to a helper and applies it to the four GC sites P0475 missed (three UPDATE chunks + one chunk_tenants DELETE via RETURNING-order leak). discovered_from=consolidator; T7 added post-plan-review."}
```

**Depends on:** [P0475](plan-0475-pg-deadlock-chunk-rollback.md) (DONE) — `is_deadlock`, `jitter`, and the sort+retry reference implementation exist.
**Conflicts with:** `rio-store/src/metadata/mod.rs` and `rio-store/src/gc/mod.rs` not in top-30 collisions. `docs/src/components/store.md` is touched by multiple doc-batch plans (P0295-T116 adds `r[store.tenant.filter-scope]` near `:195`) — this plan adds at `:87`, non-overlapping sections.
