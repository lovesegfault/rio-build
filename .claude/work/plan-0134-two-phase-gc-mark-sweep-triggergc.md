# Plan 0134: Two-phase GC — mark/sweep/orphan/drain + TriggerGC RPC

## Design

Phase 2c's chunk store had no garbage collection. Paths accumulated forever; `chunks.refcount` was maintained but nothing ever deleted at zero. This plan implemented the full GC pipeline: mark (reachability from roots) → sweep (delete unreachable, decrement chunks, enqueue S3) → drain (background S3 deletion with retry).

**Mark** lives in `rio-store/src/gc/mark.rs`. A recursive CTE walks `narinfo."references"` (TEXT[], quoted — PG reserved word) from root seeds: (a) `gc_roots` table (explicit pins, JOIN to narinfo since `gc_roots.store_path_hash` is BYTEA but CTE needs TEXT), (b) manifests with `status='uploading'` (temp roots via grace period, not explicit), (c) `narinfo WHERE created_at > now() - grace_hours` (recently-uploaded), (d) `unnest(extra_roots)` — live-build outputs passed in by the scheduler. The CTE's recursive arm: `SELECT unnest(n."references") FROM narinfo n JOIN reachable r ON n.store_path = r.store_path`. Output: `store_path_hash` of everything in `narinfo` NOT in `reachable` AND `manifests.status='complete'`.

**Sweep** (`gc/sweep.rs`) processes unreachable paths in txn batches of ~100: (1) `SELECT manifest_data.chunk_list FOR UPDATE` (TOCTOU guard — path could get re-pinned between mark and sweep), (2) `DELETE narinfo` (CASCADEs to manifests/manifest_data/content_index/realisations), (3) `UPDATE chunks SET refcount=refcount-1 WHERE blake3_hash = ANY($1)`, (4) `UPDATE chunks SET deleted=true WHERE ... AND refcount=0 RETURNING blake3_hash`, (5) `INSERT pending_s3_deletes` for each returned hash. All single-tx per batch. `dry_run=true` → `ROLLBACK` + return stats only.

**Orphan scanner** (`gc/orphan.rs`) handles a different failure mode: upload interrupted mid-`PutPath` leaves a `status='uploading'` manifest forever. Background task every 15min: `SELECT ... WHERE status='uploading' AND updated_at < now() - 2h`, then the same chunk-decrement path as sweep.

**Drain** (`gc/drain.rs`): `SELECT id, s3_key FROM pending_s3_deletes WHERE attempts < 10 ORDER BY enqueued_at LIMIT 100`. Per row: `chunk_backend.delete_by_key(s3_key)` → `DELETE` on success, `UPDATE attempts=attempts+1, last_error=$1` on fail + exponential backoff sleep. Metric: `rio_store_s3_deletes_pending` gauge.

Migration `005_gc.sql` creates `pending_s3_deletes` (BIGSERIAL PK, enqueued_at index WHERE attempts < 10) and `gc_roots` (store_path_hash BYTEA PK REFERENCES narinfo ON DELETE CASCADE). `gc_roots` is for explicit pins ONLY — NOT scheduler live-build roots, which pass as `extra_roots` param since outputs may not exist in narinfo yet (FK would reject).

**GcRoots** actor command (`rio-scheduler/src/actor/command.rs`): iterates `dag.iter_nodes()` non-terminal → collects `expected_output_paths ∪ output_paths`. The scheduler's `AdminService.TriggerGC` calls `actor.gc_roots()`, then proxies to new `StoreAdminService.TriggerGC` (store-side RPC in `store.proto`) with the result as `extra_roots`, forwarding the `GCProgress` stream back. `PinPath`/`UnpinPath` RPCs also added.

The second commit (`ccc1ae1`, 4 commits later) completed the RPC wiring: `StoreAdminService` registration in store `main.rs`, `store_admin_client` in `rio-proto/client.rs`, scheduler admin proxy, and the verify annotations for `store.gc.two-phase` / `store.gc.pending-deletes` / `ctrl.pdb.workers`. It also carried the F3 gateway `WatchBuild` reconnect wiring as a side-payload (semantically belongs to P0135 but landed here).

## Files

```json files
[
  {"path": "migrations/005_gc.sql", "action": "NEW", "note": "pending_s3_deletes + gc_roots tables"},
  {"path": "rio-store/src/gc/mod.rs", "action": "NEW", "note": "pub mod mark/sweep/orphan/drain + decrement_and_enqueue helper"},
  {"path": "rio-store/src/gc/mark.rs", "action": "NEW", "note": "recursive CTE over narinfo.references from root seeds"},
  {"path": "rio-store/src/gc/sweep.rs", "action": "NEW", "note": "batch DELETE CASCADE + chunk decrement + enqueue S3"},
  {"path": "rio-store/src/gc/orphan.rs", "action": "NEW", "note": "stale-uploading scanner, 15min period"},
  {"path": "rio-store/src/gc/drain.rs", "action": "NEW", "note": "S3 DeleteObject loop with retry/backoff"},
  {"path": "rio-store/src/grpc/admin.rs", "action": "NEW", "note": "StoreAdminService: TriggerGC/PinPath/UnpinPath"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "register StoreAdminService"},
  {"path": "rio-store/src/backend/chunk.rs", "action": "MODIFY", "note": "key_for + delete + delete_by_key methods"},
  {"path": "rio-store/src/lib.rs", "action": "MODIFY", "note": "pub mod gc"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "spawn orphan scanner + drain task"},
  {"path": "rio-scheduler/src/actor/command.rs", "action": "MODIFY", "note": "GcRoots { reply: oneshot<Vec<String>> }"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "handle GcRoots"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "TriggerGC proxy: gc_roots → store_client.trigger_gc → forward stream"},
  {"path": "rio-scheduler/src/admin/tests.rs", "action": "MODIFY", "note": "GcRoots collection test"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "store_admin_client"},
  {"path": "rio-proto/proto/store.proto", "action": "MODIFY", "note": "StoreAdminService: TriggerGC/PinPath/UnpinPath"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "GcRequest.extra_roots + GcProgress"},
  {"path": "rio-proto/src/client.rs", "action": "MODIFY", "note": "store_admin_client helper"},
  {"path": "rio-proto/src/lib.rs", "action": "MODIFY", "note": "re-export StoreAdminServiceClient"}
]
```

## Tracey

Markers implemented:
- `r[impl store.gc.two-phase]` — sweep (`9f654d7`).
- `r[impl store.gc.pending-deletes]` — drain (`9f654d7`).
- `r[verify store.gc.two-phase]` — mark+sweep roundtrip test (`ccc1ae1`).
- `r[verify store.gc.pending-deletes]` — drain iteration test (`ccc1ae1`).
- `r[verify ctrl.pdb.workers]` — controller PDB test (`ccc1ae1`, annotation belongs semantically to P0132 but landed here).

## Entry

- Depends on P0127: phase 3a complete (StoreService, actor command pattern, AdminService).
- Depends on P0086: phase 2c chunk refcount (`chunks.refcount` column, `ChunkBackend` trait).

## Exit

Merged as `9f654d7` + `ccc1ae1` (2 commits, non-contiguous, gap=4). `.#ci` green at merge. Tests: mark seeds 5-path chain, pins middle, asserts tail unreachable + head+middle+closure reachable; sweep seeds unreachable path+chunks, asserts DELETE + `pending_s3_deletes` row + refcount decremented; drain enqueue→iterate→assert S3 delete called + row gone.
