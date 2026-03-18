# Plan 0159: Round 3 — trace-chain break across mpsc actor channel + empty-bearer bypass

## Design

Second branch-review. Two critical findings.

**Trace-chain break (`8196337`):** The gRPC `submit_build` handler correctly links to the gateway span via `link_parent`, but **span context does not cross the mpsc channel to the actor task.** `handle_merge_dag`'s `#[instrument]` span is a fresh root, so `dispatch.rs` calling `current_traceparent()` captured an ORPHAN span — not the gateway's trace. The worker received a valid traceparent but it belonged to a disjoint trace. The phase-4a design goal ("gateway→scheduler→worker trace continuity") was never actually achieved; the VM assertion had been watered down to "trace exists" after three failed loosening attempts in P0157.

Fix: carry traceparent as **plain data** through `MergeDagRequest` → `DerivationState` → `WorkAssignment`. Set at DAG-merge time for new nodes, first-submitter wins on dedup, empty for recovered state. `dispatch.rs` now reads `state.traceparent` instead of `current_traceparent()`. This handles ALL dispatch paths: immediate (from merge), deferred (from completion/heartbeat), post-recovery. Restored phase2b span-count ≥3 VM assertion — verified passing.

**Empty-bearer bypass (`266592b`):** `"Bearer "` (trailing space) → `strip_prefix` yields `Some("")` → matched `WHERE cache_token = ''` if an operator mistakenly created such a tenant. `CreateTenant` accepted it; `has_cache_token` hid it (`IS NOT NULL` check passed). Fixed both sides: middleware rejects empty token; `CreateTenant` rejects empty `cache_token`.

Also: `handle_reconcile_assignments` missing `#[instrument]` made its `inject_current` a no-op (`bdf2e44`); `from_poisoned_row` not guarding `Duration::from_secs_f64(+inf)` panic (`8583056`); bare `tokio::spawn` for cgroup reporter → `spawn_monitored` (`8842220`); consolidated duplicated tenant-resolve blocks (`1533daa`); added `sched.admin.list-tenants`/`create-tenant` spec markers (`a89c7d0`); minor test gaps (`68`, `66`, `65`).

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/command.rs", "action": "MODIFY", "note": "MergeDagRequest.traceparent field"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "DerivationState.traceparent; set at merge time, first-submitter wins"},
  {"path": "rio-scheduler/src/actor/merge.rs", "action": "MODIFY", "note": "populate DerivationState.traceparent from MergeDagRequest"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "read state.traceparent NOT current_traceparent()"},
  {"path": "rio-scheduler/src/actor/recovery.rs", "action": "MODIFY", "note": "#[instrument] on handle_reconcile_assignments; infinity guard in from_poisoned_row"},
  {"path": "rio-store/src/cache_server/auth.rs", "action": "MODIFY", "note": "reject empty bearer token after strip_prefix"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "CreateTenant rejects empty cache_token"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "consolidated tenant-resolve helper"},
  {"path": "rio-worker/src/cgroup.rs", "action": "MODIFY", "note": "compute_fractions pure fn extraction; spawn_monitored"},
  {"path": "nix/tests/phase2b.nix", "action": "MODIFY", "note": "restored span-count ≥3 assertion"}
]
```

## Tracey

- `r[verify obs.trace.w3c-traceparent]` — `8196337`
- `r[verify sched.trace.assignment-traceparent]` — `8196337` (second verify, dispatch-carry test)
- `r[verify obs.metric.worker-util]` — `05d3f35`
- `r[impl sched.admin.list-tenants]` + `r[verify sched.admin.list-tenants]` — `a89c7d0`
- `r[impl sched.admin.create-tenant]` + `r[verify sched.admin.create-tenant]` — `a89c7d0`
- `r[verify obs.metric.transfer-volume]` — `a89c7d0`

8 marker annotations.

## Entry

- Depends on P0151: traceparent (fixes its core bug)
- Depends on P0154: bearer auth (fixes its bypass)

## Exit

Merged as `8196337..acc5460` (16 commits). `.#ci` green including `.#checks.x86_64-linux.vm-phase2b`. Phase doc round-3 summary: "6ee4f10..3abfc95, +15 commits".
