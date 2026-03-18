# Plan 0139: Validation R2 — scheduler_live_pins + CN=rio-gateway bypass + 21 bugs (X-series)

## Design

A fresh deep review after R1 found **5 CRITICAL + 9 HIGH + 7 MEDIUM** bugs — including one that completely defeated the HMAC token system. Also: two vm-phase3b sections that were HOLLOW (ran without error but exercised nothing). This is the longest validation round: 23 commits, 976→991 tests.

**X1 (CRIT) — mTLS bypass defeated HMAC.** P0131's gateway bypass was: `if peer_certs.is_some() && token.is_none() → skip HMAC`. Every mTLS client has peer_certs. A compromised worker just omits the token → uploads arbitrary paths. The HMAC system was dead code in production. Fix: `x509-parser` extracts the client cert's Common Name; bypass only if `CN=rio-gateway`. Workers have `CN=rio-worker-*` → no bypass. New fuzz seed for duplicate-chunks + fuzz `Cargo.lock` bump for x509-parser dep.

**X2 (CRIT) — Build CRD stuck at `build_id="submitted"` sentinel.** Controller patched `build_id="submitted"` BEFORE calling `submit_build` (to prevent double-submit on reconcile race). But if `submit_build` failed, or the controller crashed before the second patch, the sentinel stayed forever — `apply()` saw non-empty `build_id` → assumed already submitted → `await_change()` forever. No resubmit path. Fix: `apply()` detects orphaned sentinel (build_id=="submitted" AND no watch task running) → falls through to resubmit. Bundled with 4 other drain_stream fixes: `build_id` plumbed correctly, `Progress` event sets `phase=Building`, single scopeguard (was two, one leaked), cleanup removes `watching` entry.

**X3 (CRIT) — Orphan scanner stale chunk_list.** Outer SELECT grabbed `chunk_list`, then looped calling `decrement_and_enqueue` per-path. With multiple store replicas, two scanners could process the same path: scanner A reads chunk_list, scanner B reads same, A decrements + deletes manifest, B decrements AGAIN (wrong chunks now, because the path→chunk mapping changed). Fix: re-read `chunk_list` INSIDE the tx with `FOR UPDATE OF m` — mirrors sweep.rs's existing pattern.

**X4 (CRIT) — Transient-failure retry wrote Failed to PG, Ready to mem.** Retry path: transition `Failed → Ready` in-mem, but the PG write happened BEFORE the transition (wrote Failed). Crash here → recovery loads Failed, never enqueues → hang. Fix: write Ready to PG matching final in-mem state.

**X5 (CRIT) — Recovery missed all-complete builds.** Crash between last-drv→Completed and build→Succeeded: recovery loads the build (status=Active), loads all derivations (all Completed), but nothing triggers `check_build_completion`. Build hangs Active forever. Fix: post-recovery sweep calling `check_build_completion` for every loaded build.

**X9 (HIGH) — Auto-pin live-build INPUTS.** GC's `extra_roots` (from `GcRoots` actor command) passed OUTPUTS of in-flight builds. But inputs weren't protected — a long-running build's input could be GC'd out from under it. New table `scheduler_live_pins` (migration 007); mark CTE gets seed (e) `SELECT store_path FROM scheduler_live_pins`; scheduler pins `assignment.input_paths` at dispatch, unpins at terminal.

**Other HIGH:** X6 `reassign_derivations` skipped `POISON_THRESHOLD` check (3 disconnects → Ready-but-undispatchable, not Poisoned) — extracted `poison_and_cascade` helper; X7 FastCDC produces duplicate chunks in one NAR → PG `ON CONFLICT` crashes on same-row-twice → dedup before `UNNEST`; X8 sweep didn't delete `realisations` (no FK to narinfo) → dangling rows → `wopQueryRealisation` 404s; X10 gateway reconnect counter reset on `WatchBuild Ok()` — but Ok() precedes the first event, so a scheduler that accepts-then-immediately-closes (recovery in progress) ate retries forever — reset on FIRST EVENT; X16 poison-TTL cleared in-mem `failed_workers` but not PG.

**Clamps:** X17 HMAC expiry `now + 2*build_timeout` with `u64::MAX` timeout → immortal token, clamped 7d; X18 `backoff_duration` exponential with large `retry_count` → `f64::INFINITY` → `from_secs_f64` panic, clamped 1yr; X19 concurrent `TriggerGC` calls both ran mark+sweep → `pg_try_advisory_lock(GC_LOCK_ID)` gate; X20 `s3_deletes_stuck` gauge (attempts >= 10 count); X21 `dispatch_ready` after `LeaderAcquired` eliminated ~10s post-recovery idle.

**VM hollow fixes:** V1 — S1 never ran recovery because scheduler used `always_leader()` from boot. Fixed: wire to k3s Lease, kubeconfig copy BEFORE `waitForControlPlane` → scheduler standby → acquire → `LeaderAcquired` → recovery runs. Assert journalctl log + metric. V2 — C2 never committed because all paths were in grace period → `unreachable=vec[]` → sweep body never runs. Fixed: backdate S1's output past grace.

## Files

```json files
[
  {"path": "migrations/007_live_pins.sql", "action": "NEW", "note": "scheduler_live_pins table for GC input-closure protection"},
  {"path": "rio-store/src/gc/mark.rs", "action": "MODIFY", "note": "CTE seed (e) scheduler_live_pins"},
  {"path": "rio-store/src/gc/orphan.rs", "action": "MODIFY", "note": "X3: re-read chunk_list FOR UPDATE inside tx"},
  {"path": "rio-store/src/gc/sweep.rs", "action": "MODIFY", "note": "X8: delete realisations before narinfo CASCADE"},
  {"path": "rio-store/src/gc/drain.rs", "action": "MODIFY", "note": "X20: s3_deletes_stuck gauge"},
  {"path": "rio-store/src/grpc/put_path.rs", "action": "MODIFY", "note": "X1: x509-parser CN check for bypass"},
  {"path": "rio-store/src/grpc/admin.rs", "action": "MODIFY", "note": "X19: pg_try_advisory_lock on TriggerGC"},
  {"path": "rio-store/src/cas.rs", "action": "MODIFY", "note": "X7: dedup chunk_hashes before UNNEST"},
  {"path": "rio-store/src/metadata/chunked.rs", "action": "MODIFY", "note": "X7: dedup path"},
  {"path": "rio-store/Cargo.toml", "action": "MODIFY", "note": "x509-parser dep"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "X4: write Ready to PG not Failed"},
  {"path": "rio-scheduler/src/actor/recovery.rs", "action": "MODIFY", "note": "X5: post-recovery check_build_completion sweep"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "X6: POISON_THRESHOLD check + X16: poison-TTL clears PG"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "X9: pin inputs + X17: HMAC expiry clamp 7d"},
  {"path": "rio-scheduler/src/actor/build.rs", "action": "MODIFY", "note": "X9: unpin at terminal"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "X21: dispatch_ready after LeaderAcquired"},
  {"path": "rio-scheduler/src/actor/tests/recovery.rs", "action": "MODIFY", "note": "X4/X5 tests"},
  {"path": "rio-scheduler/src/actor/tests/worker.rs", "action": "MODIFY", "note": "X6 test"},
  {"path": "rio-scheduler/src/actor/tests/dispatch.rs", "action": "MODIFY", "note": "X9 pin test"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "pin/unpin queries + clear_failed_workers"},
  {"path": "rio-scheduler/src/state/worker.rs", "action": "MODIFY", "note": "X18: backoff clamp 1yr + clippy manual_clamp allow"},
  {"path": "rio-scheduler/Cargo.toml", "action": "MODIFY", "note": "test dep"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "X10: reset reconnect counter on first event not Ok()"},
  {"path": "rio-controller/src/reconcilers/build.rs", "action": "MODIFY", "note": "X2: orphaned sentinel resubmit + 4 drain_stream fixes"},
  {"path": "nix/tests/phase3b.nix", "action": "MODIFY", "note": "V1: wire to k3s Lease + V2: backdate for GC + CN=rio-gateway test cert"},
  {"path": "Cargo.toml", "action": "MODIFY", "note": "x509-parser workspace dep"},
  {"path": "docs/src/phases/phase3b.md", "action": "MODIFY", "note": "validation round 2 summary — 21 bugs"}
]
```

## Tracey

No tracey markers landed. All fixes are to features whose markers exist from P0128-P0134. The X-series bugs don't have independent spec requirements — they're violations of the existing ones.

## Entry

- Depends on P0138: R1 baseline (fixes layer on top).
- Specifically: X1→P0131 (HMAC), X3→P0138:C2 (orphan scanner), X4/X5→P0130 (recovery), X7→P0086 (FastCDC), X9→P0134 (GC mark), X10→P0138:H3 (reconnect counter).

## Exit

Merged as `54c5baa..c57158d` (23 commits). `.#ci` green at merge. 991 tests. vm-phase3b: V1 proves recovery ACTUALLY runs (journalctl "recovered N derivations" + `rio_scheduler_recovery_derivations_loaded` metric); V2 proves GC sweep ACTUALLY commits (1 path deleted, `pathsCollected` field populated).
