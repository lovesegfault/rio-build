# Plan 996394104: Split scheduler grpc/mod.rs — SchedulerService + WorkerService impls

Consolidator (mc100) flagged [`rio-scheduler/src/grpc/mod.rs`](../../rio-scheduler/src/grpc/mod.rs) at 1087L, collision=33 (3rd highest in repo). ResolveTenant RPC added this window = 5th SchedulerService RPC (`submit_build:351`, `watch_build:583`, `query_build_status:626`, `cancel_build:648`, `resolve_tenant:692`) + 2 WorkerService RPCs (`build_execution:732`, `heartbeat:987`). Clean fault line: `impl SchedulerService` (~370L) vs `impl WorkerService` (~350L).

**Split target:** `grpc/scheduler_service.rs` + `grpc/worker_service.rs`; `mod.rs` keeps the `SchedulerGrpc` struct (`:26`) + constructors (`:46-179`) + shared `resolve_tenant_name` helper (`:192`) + the `check_actor_alive`/`ensure_leader`/`send_and_await` plumbing.

**Historical note:** P0066 created this `mod.rs` as a split result already. 33 collisions since. The consolidator says "Worth it if: a 6th SchedulerService RPC or 3rd WorkerService RPC lands — P0266 already added resources-ema to `build_execution` this window, next proto touch tips it." This plan is anticipatory — LOW complexity, moves code, no behavior change. Dispatch when a proto-touch plan enters the frontier or when collision count passes 35.

## Tasks

### T1 — `refactor(scheduler):` move impl SchedulerService → grpc/scheduler_service.rs

NEW [`rio-scheduler/src/grpc/scheduler_service.rs`](../../rio-scheduler/src/grpc/scheduler_service.rs) — move the `impl SchedulerService for SchedulerGrpc` block (`:347-721`) verbatim. Add `use super::SchedulerGrpc; use super::resolve_tenant_name;` and whatever else the block references. The `r[impl ...]` annotations move with the block.

### T2 — `refactor(scheduler):` move impl WorkerService → grpc/worker_service.rs

NEW [`rio-scheduler/src/grpc/worker_service.rs`](../../rio-scheduler/src/grpc/worker_service.rs) — move the `impl WorkerService for SchedulerGrpc` block (`:728-1080`) verbatim. Same import pattern.

### T3 — `refactor(scheduler):` mod.rs keeps struct + shared helpers

MODIFY [`rio-scheduler/src/grpc/mod.rs`](../../rio-scheduler/src/grpc/mod.rs) — add `mod scheduler_service; mod worker_service;` declarations; drop the moved impl blocks; keep `SchedulerGrpc` struct + ctors + `check_actor_alive`/`ensure_leader`/`send_and_await` + `resolve_tenant_name`. These become `pub(super)` so the new modules can call them.

Final mod.rs should be ~350L (struct + ctors + helpers + tests module declaration).

### T4 — `test:` grpc/tests.rs imports unchanged

MODIFY [`rio-scheduler/src/grpc/tests.rs`](../../rio-scheduler/src/grpc/tests.rs) — verify it still compiles. It tests both services through `SchedulerGrpc`; the split shouldn't affect test imports (they use `super::SchedulerGrpc` + proto-generated trait methods). If imports break, adjust to `use super::*` or explicit `use crate::grpc::SchedulerGrpc`.

## Exit criteria

- `/nbr .#ci` green
- `wc -l rio-scheduler/src/grpc/mod.rs` → ≤400 (T3: ~700L shed to new files)
- `wc -l rio-scheduler/src/grpc/scheduler_service.rs` → ≥350 (T1)
- `wc -l rio-scheduler/src/grpc/worker_service.rs` → ≥300 (T2)
- `grep -c 'impl SchedulerService\|impl WorkerService' rio-scheduler/src/grpc/mod.rs` → 0 (T3: both moved)
- `grep -c 'pub(super) fn\|pub(super) async fn' rio-scheduler/src/grpc/mod.rs` → ≥3 (T3: check_actor_alive, ensure_leader, send_and_await, resolve_tenant_name — at least 3 opened to pub(super))
- `cargo nextest run -p rio-scheduler` → all pass (T4: no behavior change)
- `nix develop -c tracey query validate` → 0 total error(s) (T1+T2: `r[impl ...]` annotations moved with their blocks — no dangling refs)
- **Refactor-only check:** `git diff --stat` shows zero `.proto` changes, zero Cargo.toml changes — pure code move

## Tracey

No new markers. Pure code-move refactor. Existing `r[impl ...]` annotations in `grpc/mod.rs` move with their blocks to the new files; tracey's source scan follows them (source_include covers `rio-scheduler/src/**/*.rs`).

## Files

```json files
[
  {"path": "rio-scheduler/src/grpc/scheduler_service.rs", "action": "NEW", "note": "T1: impl SchedulerService for SchedulerGrpc (submit_build, watch_build, query_build_status, cancel_build, resolve_tenant)"},
  {"path": "rio-scheduler/src/grpc/worker_service.rs", "action": "NEW", "note": "T2: impl WorkerService for SchedulerGrpc (build_execution, heartbeat)"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "T3: +mod scheduler_service; +mod worker_service; drop moved impl blocks; open check_actor_alive/ensure_leader/send_and_await/resolve_tenant_name to pub(super)"},
  {"path": "rio-scheduler/src/grpc/tests.rs", "action": "MODIFY", "note": "T4: adjust imports if needed (likely none — tests use SchedulerGrpc directly)"}
]
```

```
rio-scheduler/src/grpc/
├── mod.rs                  # T3: struct + ctors + helpers only (~350L)
├── scheduler_service.rs    # T1: NEW — impl SchedulerService (~370L)
├── worker_service.rs       # T2: NEW — impl WorkerService (~350L)
└── tests.rs                # T4: import check
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [287, 266], "note": "consolidator-mc100 finding. 1087L, collision=33 (3rd highest). ResolveTenant added this window = 5th SchedulerService RPC. Fault line at the two impl blocks. P0066 created mod.rs as a prior split result; 33 collisions since. ANTICIPATORY: consolidator says 'Worth it if: 6th SchedulerService RPC or 3rd WorkerService RPC lands' — dispatch when proto-touch plan enters frontier or collision>35. Soft-dep P0287 (touches grpc/mod.rs :503 for x-rio-trace-id — in the SchedulerService impl block that T1 moves; sequence P0287→this or T1 moves P0287's edit with the block). Soft-dep P0266 (added resources-ema to build_execution in WorkerService — already merged, block T2 moves includes it). Pure refactor — no behavior change, no proto, no Cargo.toml. Net effect: future plans touching SubmitBuild don't collide with plans touching heartbeat."}
```

**Depends on:** none — pure refactor of existing code.
**Soft-dep:** [P0287](plan-0287-trace-linkage-submitbuild-metadata.md) — T2 adds `x-rio-trace-id` at `grpc/mod.rs:503` inside the SchedulerService impl that T1 moves. If P0287 merges first, T1 moves its edit along with the block. If this merges first, P0287's `:503` line reference becomes `scheduler_service.rs:~160` — P0287 re-greps at dispatch.
**Conflicts with:** [`rio-scheduler/src/grpc/mod.rs`](../../rio-scheduler/src/grpc/mod.rs) count=33 — T3 is a large delete-and-move; any UNIMPL plan touching it (P0287-T2, P0311-T22) needs to land before this OR re-locate their edits. **Anticipatory plan — dispatch AFTER P0287 and P0311-T22 to minimize collision chain.** [`rio-scheduler/src/grpc/tests.rs`](../../rio-scheduler/src/grpc/tests.rs) — T4 touch is minimal (import check); P0287-T5 and P0311-T22 also add tests here, all additive.
