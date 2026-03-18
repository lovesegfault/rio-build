# Plan 0208: RETURNING (refcount=1) inserted-check — upsert refactor

Wave 1. Fixes a race in the chunk-upsert path: currently [`cas.rs`](../../rio-store/src/cas.rs) re-SELECTs `refcount` after the batch upsert at [`chunked.rs:117-127`](../../rio-store/src/metadata/chunked.rs) to decide which chunks are new (need backend upload) vs already-present (skip). A concurrent PutPath can bump refcount between the upsert and the re-SELECT → our insert gets marked already-present → chunk never uploaded → silent data loss on first GC drain.

**Audit B1 #8 (overrides Batch A #4):** The fix is `RETURNING blake3_hash, (refcount = 1) AS inserted` — NOT `xmax = 0`. The resurrection case: a chunk at refcount=0 (soft-deleted, awaiting GC drain) upserted by this PutPath has `xmax != 0` (CONFLICT fired → xmax says skip) but `refcount = 1` (→ upload). xmax would skip upload of a chunk S3 may have already deleted. `refcount = 1` is both SAFER and doesn't depend on PG system columns. Atomic with the INSERT, same single-statement shape. Changes return type `Result<()>` → `Result<HashSet<Vec<u8>>>`, deletes the re-query.

Closes `TODO(phase4b)` at [`rio-store/src/gc/drain.rs:109`](../../rio-store/src/gc/drain.rs).

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync-markers.md) merged (marker `r[store.cas.upsert-inserted]` exists in `store.md` — renamed from xmax-inserted at audit B1)

## Tasks

### T0 — DROPPED (was xmax psql spike — audit B1 #8: refcount=1 is documented behavior, no spike needed)

Prior T0 verified `xmax` semantics in ephemeral PG. With `refcount = 1`, RETURNING sees the post-UPDATE row state (SQL standard, not a PG quirk). No spike.

```sql
CREATE TABLE t (k int PRIMARY KEY, v int DEFAULT 0);
INSERT INTO t(k) SELECT unnest(ARRAY[1,1,2,3])
  ON CONFLICT(k) DO UPDATE SET v = t.v
  RETURNING k, (xmax = 0) AS inserted;
```

**Expected:** per-row output with correct `inserted` bools. Likely `(1,true),(1,false),(2,true),(3,true)` but possibly PostgreSQL dedupes the UNNEST input — **document what actually happens** in the PR description and in a comment above the Rust code.


### T1 — `fix(store):` chunked.rs — `.execute()` → `query_as().fetch_all()` with RETURNING

At [`rio-store/src/metadata/chunked.rs:117-127`](../../rio-store/src/metadata/chunked.rs):

```rust
// r[impl store.cas.upsert-inserted]
// refcount = 1 on the returned row means either (a) we freshly inserted
// it, or (b) we resurrected it from refcount=0 (soft-deleted, awaiting
// GC drain). BOTH cases need upload — for (b), S3 may have already
// deleted the object. Atomic with the INSERT — no re-query race.
// (Audit B1 #8: xmax = 0 would say inserted=false for (b) → skip → data loss.)
let rows: Vec<(Vec<u8>, bool)> = sqlx::query_as(
    r#"INSERT INTO chunks (blake3_hash, refcount, ...)
       SELECT * FROM unnest($1::bytea[], $2::bigint[], ...)
       ON CONFLICT (blake3_hash) DO UPDATE
         SET refcount = chunks.refcount + EXCLUDED.refcount
       RETURNING blake3_hash, (refcount = 1) AS inserted"#,
)
.bind(&hashes)
// ... existing binds ...
.fetch_all(&mut *tx)
.await?;

let inserted: HashSet<Vec<u8>> = rows
    .into_iter()
    .filter_map(|(h, ins)| ins.then_some(h))
    .collect();
Ok(inserted)
```

Return type changes: `Result<(), ...>` → `Result<HashSet<Vec<u8>>, ...>`.

### T2 — `fix(store):` cas.rs — thread inserted-set, delete re-query

At [`rio-store/src/cas.rs:147`](../../rio-store/src/cas.rs): caller now receives the inserted set. Thread it to `do_upload` at `:160` as a new parameter. **Delete the `SELECT blake3_hash, refcount FROM chunks` re-query at `:194-217` entirely** — the set from T1 IS the answer.

### T3 — `fix(store):` drain.rs — update comment, delete TODO

At [`rio-store/src/gc/drain.rs:109`](../../rio-store/src/gc/drain.rs): update comment to reference the xmax upstream fix (race now closed at insert time). Keep the `still_dead` re-check as belt-and-suspenders. Delete `TODO(phase4b)`.

### T4 — `test(store):` sequential-simulated concurrent upsert

Unit test: sequential calls simulate concurrent PutPath:

1. First upsert with chunks {A, B} → returns `{A, B}` (both inserted)
2. Second upsert with chunks {A, C} → returns `{C}` only (A already present → `inserted=false`)

### T5 — `test(store):` true-concurrent `tokio::join!`

Integration test: `tokio::join!` two PutPath calls with one shared chunk hash. Exactly one gets `inserted=true` for the shared chunk (refcount goes 0→1→2; only the 0→1 hop sees refcount=1).

Marker: `// r[verify store.cas.upsert-inserted]`

## Exit criteria

- `/nbr .#ci` green
- Code comment above RETURNING clause explains the resurrection case (refcount=0 → 1)
- Concurrent `tokio::join!` test: exactly one `inserted=true` per shared chunk
- `grep 'TODO(phase4b)' rio-store/src/gc/drain.rs` returns empty

## Tracey

References existing markers:
- `r[store.cas.upsert-inserted]` — T1 implements (chunked.rs RETURNING clause); T5 verifies (concurrent test)

The existing markers `r[store.chunk.refcount-txn]` and `r[store.put.wal-manifest]` at chunked.rs have **unchanged semantics** — no tracey bump needed.

## Files

```json files
[
  {"path": "rio-store/src/metadata/chunked.rs", "action": "MODIFY", "note": "T1: .execute() → query_as().fetch_all() with RETURNING (refcount=1); return type change; r[impl store.cas.upsert-inserted]"},
  {"path": "rio-store/src/cas.rs", "action": "MODIFY", "note": "T2: thread inserted-set to do_upload; delete :194-217 re-query"},
  {"path": "rio-store/src/gc/drain.rs", "action": "MODIFY", "note": "T3: update comment, delete TODO(phase4b); keep still_dead check"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "note psql spike result if fallback taken"}
]
```

```
rio-store/src/
├── metadata/chunked.rs            # T1: RETURNING (refcount=1)
├── cas.rs                         # T2: delete re-query
└── gc/drain.rs                    # T3: close TODO
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [], "note": "File-disjoint from all other 4b plans. T0 psql spike MUST precede T1 Rust."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync-markers.md) — marker `r[store.cas.upsert-inserted]` must exist.

**Conflicts with:** **none.** `chunked.rs`, `cas.rs`, `drain.rs` are all single-writer in 4b. The existing `r[impl store.chunk.refcount-txn]` and `r[impl store.put.wal-manifest]` at chunked.rs have unchanged semantics — no bump.
