# Plan 0140: Validation R3 — scheduler helper extraction + watch UID-dedup + orphan races (Y-series, D-series)

## Design

Deep review of R2's code. 1 HIGH + 2 MEDIUM bugs, two more hollow VM sections, and ~200 lines of needless duplication across the scheduler actor. The smallest round, but Y1 is a subtle regression of a bug R1 thought it fixed.

**Y1 (HIGH) — Controller watch-dedup delete+recreate race.** R1's C1 fix keyed the `watching` DashMap by `{ns}/{name}`. Scenario: Build `foo/bar` is running, `drain_stream` active, entry in `watching`. User deletes and immediately recreates `foo/bar` (same name, different UID). New reconcile spawns new watch, inserts `foo/bar` → now two entries would collide but DashMap just holds one. Old `drain_stream` finishes → its scopeguard removes `foo/bar` → removes the NEW entry. Next reconcile of the NEW Build: `watching` says no watch running → spawns ANOTHER. C1's duplicate-watch bug is back. Fix: key by `b.uid()` — Kubernetes guarantees UID uniqueness across the cluster lifetime.

**Y2 (MED) — Pin leak on orphan-completion.** X9 added pin-at-dispatch + unpin-at-terminal. But `handle_reconcile_assignments`'s orphan-complete branch (worker never reconnected, outputs found in store) transitioned to Completed without calling `unpin_live_inputs`. Pins leaked until next scheduler restart. Fix + test seeds pins before reconcile, asserts unpin.

**Y3 (MED) — Orphan scanner reap-then-reupload race.** X3's `FOR UPDATE` re-checked `status='uploading'` but NOT the stale threshold. Scenario: upload of path P stalls >2h (threshold), scanner SELECTs it. Client retries upload — same path, fresh `updated_at`. Scanner's `FOR UPDATE` sees `status='uploading'` (still true!) → proceeds → reaps fresh upload. Fix: `AND updated_at < threshold` in BOTH the `FOR UPDATE` re-read AND the `DELETE EXISTS`.

**Y5 (LOW, latent) — `poison_and_cascade` unconditional PG write on transition failure.** All 3 callers guarantee the precondition (status is Ready/Assigned), so unreachable today. But if it fired: in-mem says NOT poisoned (transition rejected), PG says Poisoned → recovery diverges. Added `debug_assert!` precondition + early-return.

**Y4 (LOW, comment) — Advisory-lock-on-panic comment wrong.** Comment claimed "panic → PoolConnection drops → conn closes → lock released." Actually: `PoolConnection::drop` returns conn to pool, lock stays held until sqlx recycles (could be minutes). No panic points between lock-acquire and release currently; comment corrected, note added to avoid `.unwrap()` there.

**D1-D5 (duplication, ~200 lines removed):** Extracted `persist_status` (13 call sites had the same `db.update_derivation_status` dance), `record_failure_and_check_poison` (3 sites), `unpin_best_effort` (5 sites), `ensure_running` (4 sites), `TERMINAL_STATUSES: &[DerivationStatus]` const (2 SQL queries + load filter had inline lists — drift risk).

**V3/V4 (VM hollow):** V3 — S1 recovered 0 rows because B1 and S1-setup both completed before scheduler restart (nothing non-terminal in PG). Fix: background 15s-sleep build BEFORE restart → PG has non-terminal row → recovery loads real data. PG query asserts `COUNT(*) WHERE status NOT IN (terminal) >= 1`. V4 — B1 didn't prove the HMAC verifier was loaded (broken `load` → `verifier=None` → all PutPath accepted → B1 passes anyway). Fix: assert `rio_store_hmac_bypass_total{cn="rio-gateway"} >= 1` after seedBusybox — metric only increments when verifier is non-None.

**Bonus (`1332151`):** vm-phase3a fix — apply WorkerPool CR AFTER `ctr images import`, eliminating `ErrImagePull` noise in pod logs that made debugging harder.

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "D1: persist_status + D4: record_failure_and_check_poison + Y5: poison_and_cascade early-return"},
  {"path": "rio-scheduler/src/actor/recovery.rs", "action": "MODIFY", "note": "D2: unpin_best_effort + Y2: unpin on orphan-completion"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "use extracted helpers"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "D3: ensure_running helper"},
  {"path": "rio-scheduler/src/actor/build.rs", "action": "MODIFY", "note": "use unpin_best_effort"},
  {"path": "rio-scheduler/src/actor/tests/fault.rs", "action": "MODIFY", "note": "helper extraction test adjustments"},
  {"path": "rio-scheduler/src/actor/tests/recovery.rs", "action": "MODIFY", "note": "Y2 pin-leak test + V3 seeded in-flight"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "helper support"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "D5: TERMINAL_STATUSES const + fix intra-doc link"},
  {"path": "rio-controller/src/reconcilers/build.rs", "action": "MODIFY", "note": "Y1: watching keyed by uid + Y4 comment"},
  {"path": "rio-controller/src/reconcilers/mod.rs", "action": "MODIFY", "note": "watching: DashMap<String (uid), ()>"},
  {"path": "rio-store/src/gc/orphan.rs", "action": "MODIFY", "note": "Y3: re-check updated_at < threshold in FOR UPDATE + DELETE EXISTS"},
  {"path": "rio-store/src/grpc/admin.rs", "action": "MODIFY", "note": "Y4: correct advisory-lock comment"},
  {"path": "nix/tests/phase3b.nix", "action": "MODIFY", "note": "V3 seeds in-flight build + V4 asserts hmac_bypass_total metric"},
  {"path": "nix/tests/phase3a.nix", "action": "MODIFY", "note": "apply WorkerPool CR after ctr images import"},
  {"path": "docs/src/phases/phase3b.md", "action": "MODIFY", "note": "round 3 summary"}
]
```

## Tracey

No tracey markers landed. The UID-dedup mechanism gets a retroactive `r[impl ctrl.build.watch-by-uid]` in P0141's `dadc70c`.

## Entry

- Depends on P0139: R2 baseline (Y1 regresses C1 via X2's watching changes; Y2 fixes X9's pin path; Y3 refines X3's FOR UPDATE).

## Exit

Merged as `fbadbfc..1332151` (11 commits). `.#ci` green at merge. 994 tests. vm-phase3b: V3 proves recovery loads REAL rows (PG query count >= 1); V4 proves HMAC verifier is ACTIVE (bypass metric increments).
