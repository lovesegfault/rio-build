# Plan 994957501: Extract `enqueue_chunk_deletes` — S3-enqueue 28-line duplication

Consolidator flagged a 28-line structural duplicate on a race-critical path. [`gc/mod.rs:558-586`](../../rio-store/src/gc/mod.rs) (inside `decrement_and_enqueue`) and [`gc/sweep.rs:344-376`](../../rio-store/src/gc/sweep.rs) (inside `sweep_orphan_chunks`) both implement the identical `pending_s3_deletes` enqueue sequence: iterate zeroed chunks, `try_from` to `[u8; 32]`, `backend.key_for(&arr)`, batched unnest INSERT with `ON CONFLICT DO NOTHING`. The sweep.rs copy even opens with a comment at [`:335-338`](../../rio-store/src/gc/sweep.rs) saying "Identical to the enqueue block in `decrement_and_enqueue` (gc/mod.rs:558)" — the author knew at write time.

[P0262](plan-0262-putchunk-impl-grace-ttl.md) touched both files without consolidating. The duplication is on the **same** `r[store.gc.pending-deletes]` transactional-outbox path: both callers are "inside a transaction, have a `Vec<(Vec<u8>, i64)>` of zeroed chunks, need to `key_for` each and batch-INSERT." If the TOCTOU re-check comment at [`mod.rs:570-574`](../../rio-store/src/gc/mod.rs) ever changes (e.g., adding a `deleted_at` timestamp column), sweep.rs would silently drift — and drift here means orphaned S3 objects that drain can never reconcile.

No in-flight conflict: `rio-store/src/gc/mod.rs` and `rio-store/src/gc/sweep.rs` are not in the collision top-50.

## Tasks

### T1 — `refactor(store):` extract `enqueue_chunk_deletes` helper

NEW pub(super) helper at [`rio-store/src/gc/mod.rs`](../../rio-store/src/gc/mod.rs) — place it AFTER `DecrementStats` (around `:485`), BEFORE `decrement_and_enqueue`:

```rust
/// Enqueue S3 keys for zeroed chunks to `pending_s3_deletes` in the
/// given transaction. Batched via unnest — one RTT per call instead
/// of per-chunk (a 1000-chunk manifest would otherwise need 1000
/// INSERTs at ~1ms RTT = ~1s; batched it's ~1ms).
///
/// `blake3_hash` is written alongside `s3_key` so the drain task can
/// re-check `chunks.(deleted AND refcount=0)` before issuing the S3
/// DELETE — catches the TOCTOU where PutPath resurrected the chunk
/// after we enqueued it. `ON CONFLICT DO NOTHING`: duplicate enqueues
/// are idempotent (drain deletes the row after S3 success).
///
/// Skips hashes that fail `try_from` to `[u8; 32]` (can't-happen — the
/// `chunks` PK is BYTEA but every writer inserts exactly 32 bytes;
/// `warn!` + skip rather than panic so one corrupt row doesn't kill
/// the sweep). Returns the number of keys actually enqueued.
///
/// No-op if `backend` is None (inline-only store has no S3 keys).
// r[impl store.gc.pending-deletes]
pub(super) async fn enqueue_chunk_deletes(
    tx: &mut Transaction<'_, Postgres>,
    zeroed: &[(Vec<u8>, i64)],
    backend: Option<&Arc<dyn ChunkBackend>>,
) -> Result<u64, sqlx::Error> {
    let Some(backend) = backend else {
        return Ok(0);
    };
    if zeroed.is_empty() {
        return Ok(0);
    }
    let mut keys: Vec<String> = Vec::with_capacity(zeroed.len());
    let mut hashes: Vec<Vec<u8>> = Vec::with_capacity(zeroed.len());
    for (hash, _size) in zeroed {
        let Ok(arr) = <[u8; 32]>::try_from(hash.as_slice()) else {
            warn!(len = hash.len(), "GC: chunk hash wrong length, skipping S3 enqueue");
            continue;
        };
        keys.push(backend.key_for(&arr));
        hashes.push(hash.clone());
    }
    if keys.is_empty() {
        return Ok(0);
    }
    sqlx::query(
        "INSERT INTO pending_s3_deletes (s3_key, blake3_hash) \
         SELECT * FROM unnest($1::text[], $2::bytea[]) \
         ON CONFLICT DO NOTHING",
    )
    .bind(&keys)
    .bind(&hashes)
    .execute(&mut **tx)
    .await?;
    Ok(keys.len() as u64)
}
```

The `r[impl store.gc.pending-deletes]` annotation **moves** here. The current annotation at [`drain.rs:2`](../../rio-store/src/gc/drain.rs) stays — drain is step 2 of the outbox pattern (consume), this is step 1 (produce). Both implement the marker.

### T2 — `refactor(store):` call site in `decrement_and_enqueue`

MODIFY [`rio-store/src/gc/mod.rs`](../../rio-store/src/gc/mod.rs) at `:552-586`. Replace the 28-line `if let Some(backend) = backend { ... }` block with:

```rust
stats.s3_keys_enqueued = enqueue_chunk_deletes(tx, &zeroed, backend).await?;
```

The surrounding `stats.chunks_zeroed = ...` and `stats.bytes_freed = ...` at `:549-550` stay. Net: 28 lines → 1 line.

### T3 — `refactor(store):` call site in `sweep_orphan_chunks`

MODIFY [`rio-store/src/gc/sweep.rs`](../../rio-store/src/gc/sweep.rs) at `:335-376`. Replace the entire block (including the "Identical to..." comment header at `:335-343`) with:

```rust
// Enqueue S3 keys for zeroed chunks. If chunk_backend is None
// (inline-only store), there are no S3 keys to delete — but an
// inline-only store also has no PutChunk clients (require_cache()
// returns FAILED_PRECONDITION), so `zeroed` is empty and this is
// a no-op. The Option-check inside the helper is belt-and-suspenders.
super::enqueue_chunk_deletes(&mut tx, &zeroed, chunk_backend).await?;
```

Note `&mut tx` not `&mut **tx` here — `sweep_orphan_chunks` owns the transaction (`let mut tx = pool.begin()`), `decrement_and_enqueue` receives `&mut Transaction`. The helper takes `&mut Transaction`; at this call site `tx` is already the right type. Net: 42 lines → 6 lines (keeps the inline-only-store explanation, drops the "Identical to..." apology).

### T4 — `test(store):` helper unit test — corrupt-hash skip path

The `try_from` skip path (hash wrong length) is currently untested — both call sites pull `zeroed` from a `RETURNING blake3_hash` query, so the BYTEA is always well-formed in practice. Extraction makes this directly testable.

NEW test in [`rio-store/src/gc/mod.rs`](../../rio-store/src/gc/mod.rs) `#[cfg(test)] mod tests` (after `decrement_refcounts_no_zero` at `:650`):

```rust
/// enqueue_chunk_deletes: a hash that isn't 32 bytes is skipped
/// with a warn, not a panic. The well-formed siblings in the same
/// batch still enqueue. (Can't-happen in practice — chunks PK writers
/// all insert 32 bytes — but warn+skip beats killing the sweep.)
#[tokio::test]
// r[verify store.gc.pending-deletes]
async fn enqueue_skips_corrupt_hash() {
    let db = TestDb::new(rio_store::MIGRATOR).await;
    let backend = MemoryChunkBackend::default();
    let mut tx = db.pool.begin().await.unwrap();

    // One well-formed (32 bytes), one corrupt (7 bytes).
    let good = vec![0xAAu8; 32];
    let bad = vec![0xBBu8; 7];
    let zeroed = vec![(good.clone(), 100i64), (bad, 50i64)];

    let enqueued = enqueue_chunk_deletes(&mut tx, &zeroed, Some(&Arc::new(backend)))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // Only the well-formed one enqueued.
    assert_eq!(enqueued, 1);
    let rows: Vec<(Vec<u8>,)> =
        sqlx::query_as("SELECT blake3_hash FROM pending_s3_deletes")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, good);
}
```

## Exit criteria

- `/nixbuild .#ci` green
- `grep 'INSERT INTO pending_s3_deletes' rio-store/src/gc/mod.rs rio-store/src/gc/sweep.rs | grep -v '///' | wc -l` → exactly 1 (the helper; both inline copies gone)
- `grep 'Identical to the enqueue block' rio-store/src/gc/sweep.rs` → 0 hits (apology comment deleted with the code it described)
- `grep 'pub(super) async fn enqueue_chunk_deletes' rio-store/src/gc/mod.rs` → 1 hit
- `grep -c 'enqueue_chunk_deletes' rio-store/src/gc/mod.rs rio-store/src/gc/sweep.rs` ≥ 4 (def + self-call + sweep call + test)
- `cargo nextest run -p rio-store gc::` — existing `zeroes_and_enqueues_s3`, `sweep_chunked_path_decrements_and_enqueues` still pass (behavior preserved)
- `cargo nextest run -p rio-store enqueue_skips_corrupt_hash` → 1 passed (T4: skip path covered)
- `tracey query rule store.gc.pending-deletes` shows ≥2 `impl` sites (drain.rs:2 + the new helper) and ≥2 `verify` sites (drain.rs:214 + T4)

## Tracey

References existing marker:
- `r[store.gc.pending-deletes]` — [`store.md:263`](../../docs/src/components/store.md). T1 adds a second `r[impl]` site (the helper is step 1 of the outbox — produce; drain.rs is step 2 — consume). T4 adds a `r[verify]` for the skip-corrupt path.

No new markers. The duplication didn't have a spec gap — both copies served the same existing requirement. Consolidation doesn't change behavior.

## Files

```json files
[
  {"path": "rio-store/src/gc/mod.rs", "action": "MODIFY", "note": "T1: new enqueue_chunk_deletes helper ~:485; T2: call site replaces :552-586 block; T4: enqueue_skips_corrupt_hash test ~:650"},
  {"path": "rio-store/src/gc/sweep.rs", "action": "MODIFY", "note": "T3: replace :335-376 enqueue block with helper call (keeps inline-store note, drops Identical-to apology)"}
]
```

```
rio-store/src/gc/
├── mod.rs       # T1: +helper, T2: call site, T4: +test
└── sweep.rs     # T3: call site
```

## Dependencies

```json deps
{"deps": [262], "soft_deps": [], "note": "P0262 merged the second copy (sweep.rs orphan-chunk sweep) without consolidating — it's the discovered_from origin and the dep ensures both copies exist when this dispatches. No in-flight conflict: gc/mod.rs and gc/sweep.rs are NOT in collisions-top-50. The helper stays in mod.rs (not a new file) so it's pub(super)-visible to sweep.rs via the existing use super::* pattern."}
```

**Depends on:** [P0262](plan-0262-putchunk-impl-grace-ttl.md) — landed the `sweep_orphan_chunks` copy at [`sweep.rs:263`](../../rio-store/src/gc/sweep.rs). Without it there's only one copy and nothing to extract.

**Conflicts with:** None. `rio-store/src/gc/mod.rs` and `rio-store/src/gc/sweep.rs` do not appear in `onibus collisions top 50`. [P0338](plan-0338-tenant-signer-wiring-putpath.md) touches `rio-store/src/grpc/` and `signing.rs`, not `gc/`.
