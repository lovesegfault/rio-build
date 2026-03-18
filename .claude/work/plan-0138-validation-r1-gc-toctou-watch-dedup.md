# Plan 0138: Validation R1 — GC chunk TOCTOU + controller watch dedup (iteration 3)

## Design

The first deep code review after the feature burst. Features worked — unit tests passed, vm-phase3b iteration 2 passed — but a fresh read of the code found **4 CRITICAL + 5 HIGH + 5 MEDIUM** bugs in paths the tests didn't exercise. This is the phase's inflection point: from "does it work" to "does it work when things go wrong."

**C1 — Build reconciler spawned duplicate watch tasks on every reconcile.** Each status patch triggered a re-reconcile (kube-runtime watches status). Each reconcile called `spawn_reconnect_watch`. After 10 status updates, 10 `drain_stream` tasks were all patching the same Build CR, racing each other. Fix: `Ctx.watching: DashMap<String, ()>` dedup gate — `apply()` checks `watching.contains_key(&ns_name)` before spawning.

**C2 — GC sweep-vs-PutPath TOCTOU for shared chunks.** Sweep's `FOR UPDATE OF manifests` locks the MANIFEST row, not the CHUNK rows. Scenario: sweep processes path A (chunk X refcount=1), enqueues X to `pending_s3_deletes`. Concurrently, `PutPath` for path B (also referencing chunk X) runs after sweep's SELECT but before drain deletes from S3. B completes → X refcount back to 1, but X is already enqueued. Drain deletes X from S3 → B's reads 404. Fix: migration `006_gc_safety.sql` adds `pending_s3_deletes.blake3_hash`; drain re-checks `chunks.refcount = 0 AND deleted = true` before S3 delete; `chunked.rs` upsert clears `deleted = false` on refcount bump.

**C3 — Orphan scanner DELETE had no status guard.** The SELECT filtered `status='uploading'` but the DELETE didn't. Upload completing between SELECT and DELETE got its valid narinfo reaped. Fix: `DELETE WHERE EXISTS (SELECT 1 FROM manifests WHERE ... AND status='uploading')`.

**C4 — Orphan-completion didn't fire `check_build_completion`.** Recovery's reconcile finds a derivation whose outputs exist in store (worker finished after crash) → marks Completed. But if that was the LAST outstanding derivation, the build stayed Active forever — nothing triggered the "are we done?" check. Fix: call `check_build_completion` after orphan-complete transition.

**H1-H5:** backstop timeout didn't persist `failed_worker` to PG (only in-mem); drain lacked `SKIP LOCKED` (multi-replica stores would fight over the same rows); gateway reconnect counter never reset on success (first hiccup ate 1 of 5 retries forever); backstop block in `handle_tick` duplicated 40 lines of `reassign_derivations` logic (refactored via `reassign_derivations` call + NaN guard on backstop computation); GC sweep and orphan shared ~50 lines of chunk-decrement logic (extracted to `decrement_and_enqueue` helper).

**M1-M5:** HMAC key file with trailing `\r\n` → wrong key; `GcRoots` didn't dedup (same path from multiple builds → duplicates in `extra_roots`, harmless but wasteful); recovery handling `Assigned` with NULL worker silently skipped (should orphan-complete-check); `PinPath` FK check via string-match not SQLSTATE (brittle); backstop NaN never fires (`est_duration.unwrap_or(7200) as f64 * 3.0` can't be NaN — but the clamp was wrong anyway).

**mkK3sNode extraction (`214173c`):** `common.nix` gains `mkK3sNode` helper (extracted from inline phase3a.nix code), parameterized by `name`, `serverNode`, `extraPackages`. Both phase3a and phase3b use it.

**vm-phase3b iteration 3 (`4625b4b`):** two new sections. F1 proves the C1 fix — `rio_controller_build_watch_spawns_total == 1` across multiple status-patch→reconcile cycles. C2 exercises non-dry-run GC commit + `PinPath`/`UnpinPath` round-trip.

## Files

```json files
[
  {"path": "rio-store/src/gc/mod.rs", "action": "MODIFY", "note": "decrement_and_enqueue helper (extracted from sweep+orphan)"},
  {"path": "rio-store/src/gc/sweep.rs", "action": "MODIFY", "note": "use decrement_and_enqueue"},
  {"path": "rio-store/src/gc/orphan.rs", "action": "MODIFY", "note": "use decrement_and_enqueue + DELETE WHERE EXISTS status='uploading'"},
  {"path": "rio-store/src/gc/drain.rs", "action": "MODIFY", "note": "SKIP LOCKED + re-check refcount=0 AND deleted=true before S3 delete"},
  {"path": "rio-store/src/metadata/chunked.rs", "action": "MODIFY", "note": "upsert clears deleted=false on refcount bump"},
  {"path": "rio-store/src/metadata/mod.rs", "action": "MODIFY", "note": "expose upsert path"},
  {"path": "rio-store/src/grpc/admin.rs", "action": "MODIFY", "note": "PinPath FK check via SQLSTATE 23503"},
  {"path": "migrations/006_gc_safety.sql", "action": "NEW", "note": "ALTER pending_s3_deletes ADD blake3_hash"},
  {"path": "rio-scheduler/src/actor/recovery.rs", "action": "MODIFY", "note": "orphan-completion fires check_build_completion + handle Assigned-NULL-worker"},
  {"path": "rio-scheduler/src/actor/tests/recovery.rs", "action": "MODIFY", "note": "orphan-completion triggers build-done test"},
  {"path": "rio-scheduler/src/actor/tests/integration.rs", "action": "MODIFY", "note": "test fix"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "backstop via reassign_derivations + NaN guard"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "GcRoots dedup via HashSet"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "reset reconnect counter on successful WatchBuild"},
  {"path": "rio-controller/src/reconcilers/build.rs", "action": "MODIFY", "note": "Ctx.watching dedup gate before spawn_reconnect_watch"},
  {"path": "rio-controller/src/reconcilers/mod.rs", "action": "MODIFY", "note": "Ctx.watching: DashMap<String, ()>"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "init watching DashMap"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests.rs", "action": "MODIFY", "note": "test Ctx watching"},
  {"path": "rio-controller/Cargo.toml", "action": "MODIFY", "note": "dashmap dep"},
  {"path": "rio-common/src/hmac.rs", "action": "MODIFY", "note": "trim CRLF from key file read"},
  {"path": "nix/tests/common.nix", "action": "MODIFY", "note": "mkK3sNode helper extracted"},
  {"path": "nix/tests/phase3a.nix", "action": "MODIFY", "note": "use common.mkK3sNode"},
  {"path": "nix/tests/phase3b.nix", "action": "MODIFY", "note": "iteration 3 — F1 + C2 sections"},
  {"path": "docs/src/phases/phase3b.md", "action": "MODIFY", "note": "iteration 3 — validation round findings"}
]
```

## Tracey

No tracey markers landed. These are bugfixes to features whose markers already exist (P0132, P0134, P0135). The round-4 docs catchup (P0141) later added retroactive `impl` markers for some of these paths (e.g., `ctrl.build.watch-by-uid` documents the C1 fix mechanism, though this round keyed by `{ns}/{name}` — the UID-keying was Y1 in P0140).

## Entry

- Depends on P0130: recovery orphan-completion path (C4 fixes it).
- Depends on P0131: HMAC key load (M1 CRLF trim).
- Depends on P0134: GC sweep/orphan/drain (C2, C3, H1, H5 fix them).
- Depends on P0135: controller reconnect + gateway reconnect counter (C1, H3).

## Exit

Merged as `fe58a74..0b2f72f` (14 commits). `.#ci` green at merge. vm-phase3b iteration 3 passes — F1 proves `build_watch_spawns_total == 1`, C2 proves real GC sweep commits (non-dry-run).
