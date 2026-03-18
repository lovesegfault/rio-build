# Plan 0089: Bloom filter — rio-common lib + worker heartbeat wiring

## Design

For worker-locality scoring: workers send "approximately which store paths I have cached" in `HeartbeatRequest.local_paths`; the scheduler (P0091) uses it for transfer-cost scoring — a worker with most inputs already cached is a better dispatch candidate.

### Self-describing BloomFilter (blake3-based)

`blake3` because this is a WIRE PROTOCOL — worker builds, scheduler queries, both sides must compute the same bit indices or every query is a false negative. Rules out platform-dependent hashes (`gxhash` is AES-NI based, different output on ARM). `blake3` is portable AND already a dep (P0082). Single 256-bit call, first 16 bytes split into two `u64`s for Kirsch-Mitzenmacher double-hashing — no seeded re-hash needed.

Sizing: standard `m = -(n*ln(p))/(ln2)^2`, `k = (m/n)*ln2`. Clamps zero expected items to 1 (crashing on empty inventory at startup is annoying), FPR to `(1e-9, 0.5)`, `k` to `[1, 16]`. `indices()` is a FREE function not a `&self` method: `insert()` needs `&mut self.bits` inside the loop; a `&self` method would borrow-conflict. Copying two `u32` fields out is free.

`to_wire`/`from_wire` return/take tuples (not the proto struct) because `rio-common` can't depend on `rio-proto` (cycle: proto → common for limits). Caller constructs the proto struct. Hardcoded `BLAKE3_256 = 3`. `maybe_contains` not `contains`: the "maybe" is the whole point. A caller treating this as exact would be a bug; the name makes that harder.

Proto: adds `BLOOM_HASH_ALGORITHM_BLAKE3_256 = 3`. Proptest: zero false negatives always, FPR within 2× of target for n=1000 p=0.01, wire roundtrip preserves membership.

### Worker heartbeat integration

`Cache` holds `Arc<RwLock<BloomFilter>>`, built from SQLite inventory at startup, incrementally updated on each `insert()`. Heartbeat snapshots and serializes it into `HeartbeatRequest.local_paths` every 10s.

Never deleted from — evicted paths stay as stale positives. That's fine: the scheduler treats the filter as a HINT for scoring, not ground truth. A stale positive means a worker scores slightly better than it should for one dispatch; the actual fetch still happens if the path isn't really cached. Only restart clears it. Sizing: 50k expected items at 1% FPR ≈ 60 KB filter. Headroom for workers with bigger caches; cheap enough that over-sizing doesn't matter. If actual inventory blows past this, FPR degrades gracefully.

Insert AFTER SQLite write succeeds: if before and the write failed, we'd have a bloom positive that doesn't correct on restart (restart rebuilds from SQLite, which doesn't have the path). After means bloom and SQLite stay consistent modulo evictions.

`Cache::bloom_handle()`: `main.rs` extracts the `Arc<RwLock>` BEFORE `Cache` moves into `mount_fuse_background`. FUSE owns the `Cache`; heartbeat owns a read-handle to just the bloom. Same underlying `RwLock` — inserts by FUSE ops show up in subsequent heartbeat snapshots. `build_heartbeat_request` takes `Option<&BloomHandle>` not `Option<&Cache>` for the same reason. Clone-out-of-lock before serializing (don't hold the guard across the gRPC send). `to_wire()` tuple unpacked into proto struct here (cycle avoidance).

Test gotcha: `Cache::insert` uses `Handle::block_on` (designed for FUSE's sync callbacks on dedicated threads). `#[tokio::test]` async context → nested-runtime panic. `spawn_blocking` moves to blocking-pool thread where `block_on` is legal.

## Files

```json files
[
  {"path": "rio-common/src/bloom.rs", "action": "NEW", "note": "blake3 Kirsch-Mitzenmacher, sizing clamps, indices() free function, to_wire/from_wire tuples (cycle avoidance), maybe_contains"},
  {"path": "rio-common/src/lib.rs", "action": "MODIFY", "note": "pub mod bloom"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "BLOOM_HASH_ALGORITHM_BLAKE3_256 = 3; HeartbeatRequest.local_paths"},
  {"path": "rio-worker/src/fuse/cache.rs", "action": "MODIFY", "note": "Arc<RwLock<BloomFilter>>, rebuild from SQLite at startup, insert-after-write, bloom_handle() extractor"},
  {"path": "rio-worker/src/lib.rs", "action": "MODIFY", "note": "build_heartbeat_request takes Option<&BloomHandle>, clone-out-of-lock"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "extract bloom_handle before Cache moves into FUSE mount"}
]
```

## Tracey

Predates tracey adoption (adopted phase-3a, `f3957f7`). No `r[impl`/`r[verify` markers added in this cluster's commits. `tracey_covered=0`.

## Entry

- Depends on **P0031** (2a FUSE cache, `395e826`): `rio-worker/src/fuse/cache.rs` SQLite inventory exists; bloom wraps it.

## Exit

Merged as `5d82961..2886943` (2 commits). Tests: 742 → 759 (+17: 12 bloom unit, 3 bloom proptest, 2 worker heartbeat integration with wire roundtrip + rebuilt-from-sqlite-on-restart).

Consumer is P0091 (worker scoring). Producer-only at this point — scheduler side stores the bloom but doesn't use it until P0091.
