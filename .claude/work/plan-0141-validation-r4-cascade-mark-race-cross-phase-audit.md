# Plan 0141: Validation R4 — CASCADE FK + mark-vs-PutPath race + cross-phase audit (Z-series, V5-V14)

## Design

Six parallel audit agents read the ENTIRE codebase (phases 1a-3b) looking for bugs. Found 2 CRITICAL + 9 HIGH + 17 MEDIUM + 10 hollow VM assertions + doc drift. The cross-phase scope is why this round touched `nix/tests/phase{1a,2a,2b,2c,3a}.nix` too — not just phase3b.

**Z1 (CRIT) — `build_derivations` FK had NO `ON DELETE CASCADE`.** `cleanup_failed_merge` calls `delete_build(build_id)`. But `build_derivations` rows reference `builds` — FK violation. The error was silently caught by `warn!`. Orphan Active build + rows stayed in PG. On failover, recovery resurrected the build and RAN it. Client got `StoreUnavailable` at submit time, but the build silently executed LATER. Compound with Z4+Z16: spurious `BuildCompleted` event with empty `output_paths`. Migration `008_round4.sql` adds CASCADE. Also: `check_cached_outputs` moved BEFORE `persist_merge_to_db` — if all outputs are cached, no PG write → rollback is in-memory only.

**Z2 (CRIT) — Mark-vs-PutPath race.** Path P is past grace period → mark computes unreachable. Concurrently, `PutPath` for Q (referencing P) completes. Sweep deletes P → Q.references now dangles. Hybrid fix: new `GC_MARK_LOCK_ID` PG advisory lock — mark takes it EXCLUSIVE (~1s duration), PutPath takes it SHARED (no PutPath-vs-PutPath contention). Sweep ALSO re-checks references per-path via GIN index (migration 008 adds it). Metric: `rio_store_gc_path_resurrected_total`.

**HIGH (Z3-Z11):**
- **Z3:** Phantom-Assigned after crash-between-persist-and-try_send. Worker reconnects → recovery's `contains_key(w)` true → SKIP reassign. But no `running_since` set → backstop won't fire → stuck forever. Fix: cross-check `worker.running_builds` even when worker present.
- **Z4:** `transition_build` returned `Ok(())` on rejection (e.g., `Active → Active`) → callers emitted spurious events/metrics/cleanup. New `TransitionOutcome { Applied, Rejected }`; callers skip side-effects on Rejected.
- **Z5:** Corrupt LRU bytes never evicted — verify-fail returned error but bytes stayed. Next read hits same bytes. `lru.invalidate(hash)` on verify-fail.
- **Z6:** `info!(url = %cfg.database_url)` leaked PG password. `redact_db_url` helper replaces password with `***`.
- **Z7:** GC advisory lock leaked on task cancel (conn returned to pool, lock held until sqlx recycles). `scopeguard` detach.
- **Z8:** `verify_fod_hashes` hardcoded SHA256. `sha1`/`sha512` FODs false-rejected. `FodHashAlgo` enum dispatch + `sha1` dep.
- **Z9:** FOD verify ran AFTER upload — bad output already in store. Local NAR hash via `dump_path_streaming` + `DigestWriter` in `spawn_blocking`, BEFORE upload.
- **Z10:** `submit_build` called BEFORE sentinel patch. If patch failed → stream dropped → next reconcile resubmits → zombie build. Reordered: patch sentinel FIRST.
- **Z11:** `publish_event` (K8s Events) defined in P0132, ZERO callers. Wired: `apply()` → Submitted; `drain_stream` → Started/Succeeded/Failed/Cancelled; `scaling.rs` → ScaledUp/ScaledDown.

**MEDIUM batch (Z12-Z28):** duration nanos underflow; heartbeat `systems` bound (unbounded alloc); `class_queue_depth` gauge zeroed per-pass (stale entries otherwise); `try_send` rollback also unpins+deletes assignment; Z16 orphan skip (Rejected transition); chunk key validated before `Path::join` (path traversal); `pending_s3_deletes` INSERT batched via `unnest`; histogram `scopeguard`; optional grace period (`grace_period_hours` proto3 optional — zero-grace now possible); Cancelled first event short-circuits gateway; `__noChroot` checked on inline `BasicDerivation` (not just uploaded drvs); `drv_cache` cap; `compute_desired` min>max swap (clamp panic); PEM validates `-----BEGIN` marker; bloom `m` overflow clamp+warn; quantity decimal fractions (`"1.5Gi"`).

**LOW batch (`fa7461a`):** dedupe helpers; tighten tmp match pattern; fix docstring; edge dedup.

**VM hollow fixes (V5-V14, `914a413`):** V5 narHash exact match vs local (phase1a); V11 stamp content grep (phase2a); V10 gateway↔scheduler shared traceID (phase2b); V7 bigblob chunk delta (phase2c); V9 size_class baseline delta (phase2c); V8 prefetch_paths_sent (phase3a); B9 `kubectl get events` on Build CRD (phase3a); V12 T2 dedicated `CN=rio-worker` client cert (phase3b); V13 T3 NOT_SERVING probe during standby proves shared HealthReporter (phase3b); V14 C1 `pathsScanned` regex (phase3b).

**Retroactive tracey (`dadc70c`):** 5 `r[impl]` annotations for spec rules WRITTEN during this round's docs catchup (`3311280`): `ctrl.build.watch-by-uid` (Y1), `gw.reconnect.backoff` (X10), `sched.recovery.gate-dispatch` (P0130), `sched.backstop.timeout` (P0129), `worker.cancel.cgroup-kill` (P0129).

**Post-Z flake fixes (`553eef4..6adf0da`):** golden `query_path_info` sigs field env-dependent (skip); phase2b V10 Python syntax (for-loop in `-c` → temp script, then simplified to separate trace check); phase3b V3 slow-build sleep 15s→60s (Z3 reconciles completed-during-downtime); V14 proto3 omits zero-value `pathsScanned` from JSON; f-string lint.

## Files

```json files
[
  {"path": "migrations/008_round4.sql", "action": "NEW", "note": "Z1: build_derivations CASCADE + Z2: narinfo.references GIN index"},
  {"path": "rio-store/src/gc/sweep.rs", "action": "MODIFY", "note": "Z2: per-path references re-check"},
  {"path": "rio-store/src/gc/mod.rs", "action": "MODIFY", "note": "Z2: GC_MARK_LOCK_ID"},
  {"path": "rio-store/src/grpc/put_path.rs", "action": "MODIFY", "note": "Z2: shared advisory lock + Z18: batch unnest"},
  {"path": "rio-store/src/grpc/admin.rs", "action": "MODIFY", "note": "Z2: exclusive advisory lock + Z7: scopeguard + Z21: optional grace"},
  {"path": "rio-store/src/cas.rs", "action": "MODIFY", "note": "Z5: lru.invalidate on verify-fail"},
  {"path": "rio-store/src/backend/chunk.rs", "action": "MODIFY", "note": "Z17: key validated before Path::join"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "Z6: redact_db_url"},
  {"path": "rio-store/src/lib.rs", "action": "MODIFY", "note": "exports"},
  {"path": "rio-store/Cargo.toml", "action": "MODIFY", "note": "deps"},
  {"path": "rio-scheduler/src/actor/merge.rs", "action": "MODIFY", "note": "Z1: check_cached_outputs BEFORE persist + Z4: TransitionOutcome"},
  {"path": "rio-scheduler/src/actor/recovery.rs", "action": "MODIFY", "note": "Z3: phantom-Assigned cross-check + Z16: orphan skip"},
  {"path": "rio-scheduler/src/actor/build.rs", "action": "MODIFY", "note": "Z4: handle Rejected"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "Z12: duration nanos + Z15: rollback unpins"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "Z14: gauge zeroed + Z15 rollback"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "retroactive r[impl sched.backstop.timeout]"},
  {"path": "rio-scheduler/src/actor/tests/merge.rs", "action": "MODIFY", "note": "Z1 tests"},
  {"path": "rio-scheduler/src/actor/tests/recovery.rs", "action": "MODIFY", "note": "Z3/Z4 tests"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "delete_assignment"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "Z13: systems bound"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "histogram scopeguard"},
  {"path": "rio-scheduler/src/lib.rs", "action": "MODIFY", "note": "exports"},
  {"path": "rio-scheduler/src/queue.rs", "action": "MODIFY", "note": "doc fix"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "Z9: FOD verify BEFORE upload + DigestWriter"},
  {"path": "rio-worker/src/executor/inputs.rs", "action": "MODIFY", "note": "Z8: FodHashAlgo dispatch"},
  {"path": "rio-worker/src/cgroup.rs", "action": "MODIFY", "note": "retroactive r[impl worker.cancel.cgroup-kill]"},
  {"path": "rio-worker/src/lib.rs", "action": "MODIFY", "note": "exports"},
  {"path": "rio-worker/Cargo.toml", "action": "MODIFY", "note": "sha1 dep"},
  {"path": "rio-controller/src/reconcilers/build.rs", "action": "MODIFY", "note": "Z10: patch BEFORE submit + Z11: wire Events"},
  {"path": "rio-controller/src/reconcilers/mod.rs", "action": "MODIFY", "note": "retroactive r[impl ctrl.build.watch-by-uid]"},
  {"path": "rio-controller/src/scaling.rs", "action": "MODIFY", "note": "Z11: ScaledUp/Down events + Z25: min>max swap"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "Events wiring"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "Z28: quantity decimal"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests.rs", "action": "MODIFY", "note": "quantity tests"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "Z22: Cancelled first event + retroactive r[impl gw.reconnect.backoff]"},
  {"path": "rio-gateway/src/handler/mod.rs", "action": "MODIFY", "note": "Z24: drv_cache cap"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "Z23: __noChroot on inline BasicDerivation"},
  {"path": "rio-gateway/tests/golden_conformance.rs", "action": "MODIFY", "note": "skip env-dependent sigs field"},
  {"path": "rio-common/src/tls.rs", "action": "MODIFY", "note": "Z26: PEM validates BEGIN marker"},
  {"path": "rio-common/src/bloom.rs", "action": "MODIFY", "note": "Z27: m overflow clamp+warn"},
  {"path": "rio-common/src/config.rs", "action": "MODIFY", "note": "Z6: redact_db_url helper"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "Z21: grace_period_hours optional"},
  {"path": "nix/tests/phase1a.nix", "action": "MODIFY", "note": "V5: narHash exact match"},
  {"path": "nix/tests/phase2a.nix", "action": "MODIFY", "note": "V11: stamp content grep"},
  {"path": "nix/tests/phase2b.nix", "action": "MODIFY", "note": "V10: trace propagation + syntax fixes"},
  {"path": "nix/tests/phase2c.nix", "action": "MODIFY", "note": "V7+V9: chunk/size_class deltas"},
  {"path": "nix/tests/phase2c-derivation.nix", "action": "MODIFY", "note": "V9"},
  {"path": "nix/tests/phase3a.nix", "action": "MODIFY", "note": "V8+B9: prefetch metric + kubectl events"},
  {"path": "nix/tests/phase3b.nix", "action": "MODIFY", "note": "V12/V13/V14 + timing fixes"},
  {"path": "docs/src/phases/phase3b.md", "action": "MODIFY", "note": "round 4 summary"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "drift fixes + new spec rules"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "drift fixes"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "drift fixes + new spec rules"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "drift fixes + new spec rules"},
  {"path": "docs/src/components/worker.md", "action": "MODIFY", "note": "drift fixes + new spec rules"},
  {"path": "docs/src/security.md", "action": "MODIFY", "note": "drift fixes"},
  {"path": "docs/src/errors.md", "action": "MODIFY", "note": "drift fixes"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "new metrics"},
  {"path": "docs/src/configuration.md", "action": "MODIFY", "note": "drift fixes"},
  {"path": "docs/src/data-flows.md", "action": "MODIFY", "note": "drift fixes"}
]
```

## Tracey

Markers implemented (all retroactive, `dadc70c`):
- `r[impl ctrl.build.watch-by-uid]` — Y1's UID-keyed dedup (P0140 code, spec rule written here).
- `r[impl gw.reconnect.backoff]` — X10's reconnect loop (P0139 code, spec rule written here).
- `r[impl sched.recovery.gate-dispatch]` — `recovery_complete` flag (P0130 code, spec rule written here).
- `r[impl sched.backstop.timeout]` — `running_since` + tick check (P0129 code, spec rule written here).
- `r[impl worker.cancel.cgroup-kill]` — `cgroup.kill` write (P0129 code, spec rule written here).

These 5 spec rules were written in `3311280` (component docs catchup), then `dadc70c` added the `impl` annotations. "Tracey markers added retroactively" — the code existed first, the spec caught up.

## Entry

- Depends on P0140: R3 baseline.

## Exit

Merged as `8b22d05..6adf0da` (21 commits). `.#ci` green at merge. 1016 tests. Cross-phase VM tests strengthened (V5-V14 across all 7 phase test files). 5 new tracey spec rules landed in docs, `r[impl]` annotations added to existing code.
