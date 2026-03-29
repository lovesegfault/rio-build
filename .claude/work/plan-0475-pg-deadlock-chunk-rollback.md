# Plan 475: Fix PG deadlock in chunk-rollback — consistent lock order

Manual rsb testing under load hit `ERROR: deadlock detected` (SQLSTATE `40P01`) in [`delete_manifest_chunked_uploading`](../../rio-store/src/cas.rs) at `:252`. The rollback path fires `UPDATE chunks SET refcount = refcount - 1 WHERE blake3_hash = ANY($1)` with an array of chunk hashes. When two concurrent `put_chunked` calls fail and roll back with *overlapping* hash sets (common — large NARs share dedup'd chunks), Postgres acquires row locks in the order the hashes appear in each `ANY()` array. Array A = `[h1, h2, h3]`, array B = `[h3, h2, h1]` → txn A locks h1 waits for h3, txn B locks h3 waits for h1 → circular wait → deadlock.

This becomes *rare* after [P0474](plan-0474-put-chunked-concurrency-bound.md) lands (bounded concurrency → fewer concurrent rollbacks), but doesn't disappear: two large NARs substituted in the same second still overlap. The fix is textbook deadlock prevention — **sort the hash array before the UPDATE**. Consistent lock-acquisition order eliminates circular waits entirely.

Discovered during P0473 rsb testing; the deadlock message surfaced in store logs after a burst of substitution failures triggered mass rollback.

## Entry criteria

- [P0474](plan-0474-put-chunked-concurrency-bound.md) merged (bounded chunk-upload concurrency — reduces rollback pressure, but this fix is still needed for correctness)

## Tasks

### T1 — `fix(store):` sort chunk hashes before rollback UPDATE

MODIFY [`rio-store/src/cas.rs`](../../rio-store/src/cas.rs) in `delete_manifest_chunked_uploading` (called at `:252`). Before the `UPDATE chunks SET refcount = refcount - 1 WHERE blake3_hash = ANY($1)`:

```rust
// Consistent lock order prevents deadlock (40P01) when concurrent
// rollbacks have overlapping chunk sets. PG acquires row locks in
// ANY() scan order; sorting makes that order deterministic.
let mut hashes = chunk_hashes.to_vec();
hashes.sort_unstable();
sqlx::query!("UPDATE chunks SET refcount = refcount - 1 WHERE blake3_hash = ANY($1)", &hashes)
```

Check whether the function lives in `rio-store/src/cas.rs` directly or in `rio-store/src/metadata.rs` (the call site is `metadata::delete_manifest_chunked_uploading` — follow the import). Apply the sort at the function body, not the call site, so all callers get the protection.

### T2 — `fix(store):` same sort in the forward (non-rollback) refcount-increment path

The forward path `UPDATE chunks SET refcount = refcount + 1 WHERE blake3_hash = ANY($1)` (wherever it lives — grep for `refcount + 1.*ANY` or `refcount = refcount + 1`) has the same vulnerability if two concurrent `put_chunked` calls increment overlapping sets. Apply the identical sort there. If the increment is done per-chunk rather than batch, this task is N/A — verify and note in the commit.

### T3 — `test(store):` deadlock regression test

NEW integration test in [`rio-store/tests/`](../../rio-store/tests/) (needs real PG from `rio-test-support`). Spawn two tasks that concurrently call `delete_manifest_chunked_uploading` with deliberately reverse-ordered overlapping hash arrays (e.g., `[h1..h50]` and `[h50..h1]`). Before the fix this would deadlock ~deterministically; after the fix both complete. Use `tokio::time::timeout(Duration::from_secs(5), ...)` so a regression fails fast rather than hanging the test suite.

### T4 — `fix(store):` defensive catch-retry on 40P01

Belt-and-suspenders: wrap the UPDATE in a retry loop that catches `SQLSTATE 40P01` and retries once after a small jittered backoff. The sort in T1 *should* make this unreachable, but PG can still deadlock on index-page splits under extreme contention. A single retry is cheap insurance:

```rust
for attempt in 0..2 {
    match sqlx::query!(...).execute(pool).await {
        Err(e) if is_deadlock(&e) && attempt == 0 => {
            tokio::time::sleep(jitter(50..150)).await;
            continue;
        }
        r => return r,
    }
}
```

Keep the retry bounded (once, not forever) — an unbounded retry loop masks real problems.

## Exit criteria

- `cargo nextest run -p rio-store rollback_overlapping_no_deadlock` → pass within 5s timeout
- `grep 'sort_unstable\|\.sort()' rio-store/src/cas.rs rio-store/src/metadata.rs` → ≥1 hit preceding the rollback UPDATE
- `grep '40P01\|is_deadlock' rio-store/src/` → ≥1 hit (T4 retry)
- Manual rsb smoke: `nix build --store ssh-ng://... nixpkgs#{python3,ruby,nodejs}` (three concurrent large NARs) completes without deadlock in store logs
- `/nbr .#ci` green

## Tracey

References existing markers:
- `r[store.chunk.refcount-txn]` — T1/T2 harden the refcount transaction against concurrent-rollback deadlock
- `r[store.put.wal-manifest]` — T1 makes the write-ahead rollback path reliable under contention

## Files

```json files
[
  {"path": "rio-store/src/cas.rs", "action": "MODIFY", "note": "T1: sort before rollback UPDATE at :252 call site (or follow to metadata.rs)"},
  {"path": "rio-store/src/metadata.rs", "action": "MODIFY", "note": "T1+T2: sort in delete_manifest_chunked_uploading body + forward increment path; T4: 40P01 retry"},
  {"path": "rio-store/tests/cas_deadlock.rs", "action": "NEW", "note": "T3: concurrent-rollback deadlock regression test"}
]
```

```
rio-store/
├── src/
│   ├── cas.rs          # T1: call-site context
│   └── metadata.rs     # T1/T2: sort; T4: retry
└── tests/
    └── cas_deadlock.rs # T3: regression test
```

## Dependencies

```json deps
{"deps": [474], "soft_deps": [473], "note": "P0474 reduces rollback pressure; this fixes the remaining deadlock. Serialize after P0474 — both touch cas.rs."}
```

**Depends on:** [P0474](plan-0474-put-chunked-concurrency-bound.md) — bounded concurrency reduces rollback frequency. P0473 (already merged) provided the scheduler-side bound.

**Conflicts with:** [P0474](plan-0474-put-chunked-concurrency-bound.md) touches `rio-store/src/cas.rs:189` (upload helper); this plan touches `:252` (rollback) and `metadata.rs`. Different functions — should rebase cleanly, but land P0474 first per its blocker priority.
