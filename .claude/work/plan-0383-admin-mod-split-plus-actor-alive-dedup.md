# Plan 0383: Split admin/mod.rs — P0356 parity + actor-alive helper dedup

Consolidator (mc160) flagged [`rio-scheduler/src/admin/mod.rs`](../../rio-scheduler/src/admin/mod.rs) at 861L / collision=20 (6th highest). [P0356](plan-0356-split-scheduler-grpc-service-impls.md) split `grpc/mod.rs` 1087L→351L (scheduler_service.rs + worker_service.rs + mod.rs shared surface). `admin/mod.rs` trails the same trajectory: partial extraction already exists ([`builds.rs`](../../rio-scheduler/src/admin/builds.rs) 74L, [`workers.rs`](../../rio-scheduler/src/admin/workers.rs) 72L, [`graph.rs`](../../rio-scheduler/src/admin/graph.rs) 62L, [`sizeclass.rs`](../../rio-scheduler/src/admin/sizeclass.rs) 190L — handlers at `:473,:486,:787,:807` delegate), but 6 of 11 RPC handlers remain fully inline in mod.rs.

Bughunter (mc168) T5 additionally found `check_actor_alive()` + `ensure_leader()` drift across THREE copies:
- [`grpc/mod.rs:115`](../../rio-scheduler/src/grpc/mod.rs) — `"scheduler actor is unavailable (panicked or exited)"`
- [`admin/mod.rs:167`](../../rio-scheduler/src/admin/mod.rs) — `"scheduler actor not running (internal error)"`
- [`worker_service.rs:71`](../../rio-scheduler/src/grpc/worker_service.rs) — `"scheduler actor unavailable"` (no `is_`)

All pre-date the P0356 split (admin variant from [`dd530256`](https://github.com/search?q=dd530256&type=commits), grpc from [`47d5ce40`](https://github.com/search?q=47d5ce40&type=commits)); P0356 faithfully preserved the drift. Operator-visible: log-grep for actor-death needs three different strings. The admin copy's comment at `:165-166` explicitly says "Same pattern as SchedulerGrpc::check_actor_alive (grpc/mod.rs:~180)" — the ~180 line ref is also stale post-P0356.

## Tasks

### T1 — `refactor(scheduler):` hoist check_actor_alive + ensure_leader to shared grpc/mod.rs or actor_guards.rs

Create `rio-scheduler/src/grpc/actor_guards.rs` (or append to `grpc/mod.rs` — check at dispatch which has less churn) with:

```rust
use tonic::Status;
use std::sync::atomic::{AtomicBool, Ordering};

/// Actor-dead check. If the actor panicked, all commands hang on
/// a closed channel — return UNAVAILABLE early.
pub(crate) fn check_actor_alive(actor: &crate::actor::ActorHandle) -> Result<(), Status> {
    if !actor.is_alive() {
        return Err(Status::unavailable(
            "scheduler actor is unavailable (panicked or exited)",
        ));
    }
    Ok(())
}

// r[impl sched.grpc.leader-guard]
/// Return UNAVAILABLE when this replica is not the leader. Clients
/// with a health-aware balanced channel route elsewhere.
pub(crate) fn ensure_leader(is_leader: &AtomicBool) -> Result<(), Status> {
    if !is_leader.load(Ordering::Relaxed) {
        return Err(Status::unavailable("not leader (standby replica)"));
    }
    Ok(())
}
```

Free functions (not methods on `SchedulerGrpc`/`AdminGrpc`) so all three sites delegate with a thin wrapper. Keep the per-struct wrapper methods (different self types) but make them one-liners calling the shared fn. The `ensure_leader` comment's "bare `Status::unavailable` (not `failed_precondition`) because tonic's p2c balancer ejects..." rationale at grpc/mod.rs:132-137 moves to the shared fn's doc-comment.

### T2 — `refactor(scheduler):` admin/logs.rs — extract log-serving RPC chain

Extract from [`admin/mod.rs:188-345`](../../rio-scheduler/src/admin/mod.rs): `get_build_logs` handler (`:348`) + `try_ring_buffer` (`:190`) + `try_s3` (`:217`) + `gunzip_and_chunk` (`:280`) + `chunks_to_stream` (`:838`) + `extract_drv_hash` (`:854`). ~200L total. The handler delegates to the new file the same way `list_workers` → `workers.rs` already does.

### T3 — `refactor(scheduler):` admin/gc.rs — extract GC RPC chain

Extract `trigger_gc` handler (`:528`) + `spawn_store_size_refresh` (`:105`). ~150L. `spawn_store_size_refresh` is ALREADY `pub fn` (called from main.rs) — it moves to gc.rs and gets re-exported via `pub use gc::spawn_store_size_refresh;` in mod.rs.

### T4 — `refactor(scheduler):` admin/tenants.rs — extract tenant RPCs

Extract `create_tenant` (`:715`) + `list_tenants` (`:697`) + `tenant_row_to_proto` (`:820`). ~90L.

### T5 — `refactor(scheduler):` admin/mod.rs — residual shared surface

Post-extraction, mod.rs keeps: `AdminGrpc` struct + `::new` (`:137-163`), one-line wrapper methods that delegate to T2-T4 extractions, `check_actor_alive`/`ensure_leader` wrappers calling T1 shared fns, `cluster_status` (`:433` — 40L, small enough to stay inline), `drain_worker` (`:640`), `clear_poison` (`:675`). Target: ≤350L (parity with P0356's grpc/mod.rs→351L).

## Exit criteria

- `/nixbuild .#ci` green (or nextest-standalone clause-4c)
- `wc -l rio-scheduler/src/admin/mod.rs` → ≤400L (down from 861L)
- `test -f rio-scheduler/src/admin/logs.rs && test -f rio-scheduler/src/admin/gc.rs && test -f rio-scheduler/src/admin/tenants.rs` — all three exist
- `grep -c 'fn check_actor_alive\|fn ensure_leader' rio-scheduler/src/` → 2 shared defs (actor_guards.rs or grpc/mod.rs) + ≤3 one-line wrapper methods delegating
- `grep 'scheduler actor' rio-scheduler/src/ | grep 'unavailable\|not running' | sort -u | wc -l` → 1 (single canonical string; the 3-string drift collapsed)
- `cargo nextest run -p rio-scheduler admin::\|grpc::tests` → same test count pre/post split
- `nix develop -c tracey query rule sched.grpc.leader-guard` → shows ≥1 impl site (the `r[impl]` annotation moved with ensure_leader to the shared fn)

## Tracey

References existing markers:
- `r[sched.grpc.leader-guard]` — T1 shared `ensure_leader` implements this; the annotation moves from `grpc/mod.rs:124` + `admin/mod.rs:176` (duplicate `r[impl]`) to the single shared fn

No new markers. The split is file reorganization + string dedup.

## Files

```json files
[
  {"path": "rio-scheduler/src/grpc/actor_guards.rs", "action": "NEW", "note": "T1: shared check_actor_alive + ensure_leader free fns; single canonical error string"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "T1: :115 check_actor_alive body → delegate to actor_guards; :137 ensure_leader → delegate; +mod actor_guards decl"},
  {"path": "rio-scheduler/src/grpc/worker_service.rs", "action": "MODIFY", "note": "T1: :71 inline 'scheduler actor unavailable' → call actor_guards::check_actor_alive"},
  {"path": "rio-scheduler/src/admin/logs.rs", "action": "NEW", "note": "T2: get_build_logs + try_ring_buffer + try_s3 + gunzip_and_chunk + chunks_to_stream + extract_drv_hash (~200L from mod.rs:188-345,838-861)"},
  {"path": "rio-scheduler/src/admin/gc.rs", "action": "NEW", "note": "T3: trigger_gc + spawn_store_size_refresh (~150L from mod.rs:105-137,528-638)"},
  {"path": "rio-scheduler/src/admin/tenants.rs", "action": "NEW", "note": "T4: create_tenant + list_tenants + tenant_row_to_proto (~90L from mod.rs:697-786,820-836)"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "T5: shrink to AdminGrpc struct + ::new + wrapper methods + cluster_status/drain_worker/clear_poison inline; +mod decls for logs/gc/tenants; pub use gc::spawn_store_size_refresh"}
]
```

```
rio-scheduler/src/
├── grpc/
│   ├── mod.rs              # T1: check_actor_alive/ensure_leader → delegate
│   ├── actor_guards.rs     # T1: NEW — shared guards (1 canonical string)
│   └── worker_service.rs   # T1: :71 inline → delegate
└── admin/
    ├── mod.rs              # T5: shrink 861L→≤400L
    ├── logs.rs             # T2: NEW (~200L)
    ├── gc.rs               # T3: NEW (~150L)
    └── tenants.rs          # T4: NEW (~90L)
```

## Dependencies

```json deps
{"deps": [356], "soft_deps": [271, 304, 311], "note": "P0356 merged — provides the grpc/mod.rs split pattern + worker_service.rs:71 target. No hard-dep on any UNIMPL plan. Soft-dep P0271 (cursor pagination — touches admin/builds.rs; this split doesn't touch builds.rs so non-overlapping, but P0271's entry into the admin module may benefit from the cleaner post-split state). Soft-dep P0304-T123 (worker_service.rs:198 tokio::spawn→spawn_monitored — same file as T1 here, non-overlapping: T1 touches :71, T123 touches :183-198). Soft-dep P0304-T107 (admin/mod.rs:388 parse_build_id dup — T107 touches :388, this split may move that code to builds.rs or leave it in mod.rs; re-grep at T107 dispatch). Soft-dep P0311-T43 (cli.nix cutoffs subtest — touches sizeclass.rs not mod.rs). Discovered_from=consolidator-mc160 (admin split) + bughunter-mc168 (T5 actor-alive drift)."}
```

**Depends on:** [P0356](plan-0356-split-scheduler-grpc-service-impls.md) — merged; provides the split pattern and `worker_service.rs`.

**Conflicts with:** `admin/mod.rs` collision=20. [P0304](plan-0304-trivial-batch-p0222-harness.md)-T107 touches `:388` (parse_build_id dup); T123 touches `worker_service.rs:198`. Both non-overlapping with this plan's edits (T1=`:71`, T5 leaves `:388` region in mod.rs or moves to builds.rs — check at dispatch). [P0271](plan-0271-admin-cursor-pagination.md) touches `admin/builds.rs` not mod.rs. Sequence: this split is a big-diff; prefer late in a quiet window after the fixture-dedup (0380) and scaling-split (0381) merge — consolidation cluster.
