# Plan 0452: scheduler FOD routing + executor kind gate — hard split, no fallback

[ADR-019](../../docs/src/decisions/019-builder-fetcher-split.md) splits the single worker type into builders (airgapped, arbitrary-code) and fetchers (internet-facing, hash-verified). P0451 landed the `ExecutorKind` enum, `ExecutorState` type, and the worker→builder rename. This plan wires the routing: scheduler-side hard filter, executor-side wrong-kind gate, and the proxy deletion that makes the builder airgap real.

The routing rule is a single boolean: `drv.is_fixed_output != (executor.kind == Fetcher)` → reject. FODs go ONLY to fetchers; non-FODs go ONLY to builders. No overflow, no fallback, no "just this once" — a queued FOD is preferable to a builder with internet access. The executor re-derives `is_fod` from the `.drv` (it already does per `wkr-fod-flag-trust`) and refuses wrong-kind assignments with `ExecutorError::WrongKind` before daemon spawn — defense-in-depth against scheduler bugs or stale-generation races.

## Entry criteria

- [P0451](plan-0451-worker-to-builder-rename-executor-kind.md) merged — needs `ExecutorKind` enum, `ExecutorState` type, `rio-worker` → `rio-builder` crate rename

## Tasks

### T1 — `feat(scheduler):` assignment.rs — `hard_filter()` gains executor-kind clause

At [`rio-scheduler/src/assignment.rs`](../../rio-scheduler/src/assignment.rs) `hard_filter()` (`:177` pre-P0451; signature becomes `&ExecutorState` post-P0451):

```rust
// r[impl sched.dispatch.fod-to-fetcher]
if drv.is_fixed_output != (executor.kind == ExecutorKind::Fetcher) {
    return false;
}
```

Place this EARLY in `hard_filter()` — it's a cheap boolean check that prunes before the feature/capacity checks. The XOR phrasing (`!=`) covers both directions: FOD→builder rejected, non-FOD→fetcher rejected.

### T2 — `feat(scheduler):` dispatch.rs — `find_executor_with_overflow()` skips overflow chain for FODs

At [`rio-scheduler/src/actor/dispatch.rs`](../../rio-scheduler/src/actor/dispatch.rs) `find_executor_with_overflow()` (`:182` pre-P0451):

```rust
// r[impl sched.dispatch.no-fod-fallback]
if drv.is_fixed_output {
    // Fetchers have no size classes — no overflow chain to walk.
    // Direct find_executor() against the fetcher pool; queue if empty.
    return self.find_executor(drv_hash, None);
}
```

Early-return before the overflow-chain construction. Fetchers are not size-classed (per ADR-019: "no size-class because fetches are network-bound"), so the overflow walk is meaningless. If no fetcher is available the FOD queues — the scheduler NEVER sends a FOD to a builder under pressure.

### T3 — `feat(scheduler):` rebalancer.rs — `CutoffRebalancer` exempts fetchers

At [`rio-scheduler/src/rebalancer.rs`](../../rio-scheduler/src/rebalancer.rs): the duration-EMA cutoff rebalancer operates on builder pools only. Filter fetcher executors out of the rebalancer's pool iteration (skip where `kind == Fetcher`). Fetcher replica count is a fixed `FetcherPool.spec.replicas` or a simple HPA on queue depth — not the scheduler's concern.

### T4 — `feat(builder):` config.rs — `executor_kind` field, delete `fod_proxy_url`

At [`rio-builder/src/config.rs`](../../rio-builder/src/config.rs):

1. Add `executor_kind: ExecutorKind` field, sourced from `RIO_EXECUTOR_KIND` env (values: `builder` | `fetcher`). Default `Builder` if unset (backward-compat for tests that don't set it).
2. Delete the `fod_proxy_url` field — the Squid proxy is gone per ADR-019.

### T5 — `feat(builder):` runtime.rs — heartbeat includes `executor_kind`

At [`rio-builder/src/runtime.rs`](../../rio-builder/src/runtime.rs): the heartbeat payload to the scheduler gains `executor_kind` so `ExecutorState` on the scheduler side knows which pool this executor belongs to. P0451 added the proto field; this wires the builder-side write.

### T6 — `feat(builder):` executor/mod.rs — wrong-kind gate before daemon spawn

At [`rio-builder/src/executor/mod.rs:387`](../../rio-builder/src/executor/mod.rs): after re-deriving `is_fod` from the `.drv` (existing `wkr-fod-flag-trust` logic), check against `config.executor_kind`:

```rust
// r[impl builder.executor.kind-gate]
if is_fod != (config.executor_kind == ExecutorKind::Fetcher) {
    return Err(ExecutorError::WrongKind {
        is_fod,
        executor_kind: config.executor_kind,
    });
}
```

This runs BEFORE `spawn_daemon()` — a misrouted assignment must not grant a builder internet access even transiently. Add `WrongKind { is_fod: bool, executor_kind: ExecutorKind }` variant to `ExecutorError`.

### T7 — `refactor(builder):` spawn.rs — delete `http_proxy` env injection

At [`rio-builder/src/executor/daemon/spawn.rs:157-162`](../../rio-builder/src/executor/daemon/spawn.rs): delete the `http_proxy`/`https_proxy` env injection into the build sandbox. The Squid proxy is gone; fetchers egress directly (NetworkPolicy allows `0.0.0.0/0:80,443`), builders have no egress at all.

### T8 — `feat(scheduler):` metrics.rs — `rio_scheduler_fod_queue_depth` + `rio_scheduler_fetcher_utilization`

Per [ADR-019 § Observability](../../docs/src/decisions/019-builder-fetcher-split.md) and [`observability.md`](../../docs/src/observability.md) naming convention (`rio_{component}_`):

- `rio_scheduler_fod_queue_depth` (gauge) — number of FODs waiting for a fetcher
- `rio_scheduler_fetcher_utilization` (gauge) — fraction of fetchers busy

Register both; emit `fod_queue_depth` from the dispatch loop when a FOD queues (T2's no-fetcher path), emit `fetcher_utilization` from the same periodic task that drives `rio_scheduler_class_load_fraction`.

### T9 — `test(scheduler):` hard_filter matrix — FOD/non-FOD × builder/fetcher

Unit tests at `rio-scheduler/src/assignment.rs`:

| `is_fixed_output` | `executor.kind` | `hard_filter()` |
|---|---|---|
| `true` | `Fetcher` | passes (rest of filter permitting) |
| `true` | `Builder` | `false` |
| `false` | `Fetcher` | `false` |
| `false` | `Builder` | passes (rest of filter permitting) |

All four cells. `// r[verify sched.dispatch.fod-to-fetcher]` on the test module.

### T10 — `test(builder):` integration — fetcher refuses non-FOD assignment

Integration test: spawn executor with `RIO_EXECUTOR_KIND=fetcher`, send a non-FOD assignment, assert `ExecutorError::WrongKind` without daemon spawn. `// r[verify builder.executor.kind-gate]`.

Mirror case (builder refuses FOD) as a second test fn in the same file.

## Exit criteria

- `/nixbuild .#ci` green
- `cargo nextest run -p rio-scheduler hard_filter` → 4-cell matrix green
- `cargo nextest run -p rio-builder wrong_kind` → both directions green
- `grep 'is_fixed_output != .*ExecutorKind::Fetcher' rio-scheduler/src/assignment.rs` → ≥1 hit
- `grep 'ExecutorError::WrongKind' rio-builder/src/executor/mod.rs` → ≥1 hit
- `grep 'fod_proxy_url\|http_proxy' rio-builder/src/` → 0 hits (proxy fully deleted)
- `grep 'rio_scheduler_fod_queue_depth\|rio_scheduler_fetcher_utilization' rio-scheduler/src/` → ≥2 hits (registered + emitted)

## Tracey

Implements:
- `r[sched.dispatch.fod-to-fetcher]` — T1 impl, T9 verify
- `r[sched.dispatch.no-fod-fallback]` — T2 impl; verify is T9's `true×Builder` cell (FOD finds no builder even when builders are idle)
- `r[builder.executor.kind-gate]` — T6 impl, T10 verify

## Files

```json files
[
  {"path": "rio-scheduler/src/assignment.rs", "action": "MODIFY", "note": "T1: hard_filter() kind clause. T9: 4-cell matrix unit tests"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "T2: find_executor_with_overflow() early-return for FODs. T8: emit fod_queue_depth on queue"},
  {"path": "rio-scheduler/src/rebalancer.rs", "action": "MODIFY", "note": "T3: CutoffRebalancer skips kind==Fetcher. T8: emit fetcher_utilization alongside class_load_fraction"},
  {"path": "rio-builder/src/config.rs", "action": "MODIFY", "note": "T4: +executor_kind from RIO_EXECUTOR_KIND, -fod_proxy_url"},
  {"path": "rio-builder/src/runtime.rs", "action": "MODIFY", "note": "T5: heartbeat payload includes executor_kind"},
  {"path": "rio-builder/src/executor/mod.rs", "action": "MODIFY", "note": "T6: L387 wrong-kind gate, +ExecutorError::WrongKind variant"},
  {"path": "rio-builder/src/executor/daemon/spawn.rs", "action": "MODIFY", "note": "T7: delete L157-162 http_proxy env injection"},
  {"path": "rio-builder/tests/executor_kind_gate.rs", "action": "CREATE", "note": "T10: integration test — fetcher refuses non-FOD, builder refuses FOD"}
]
```

```
rio-scheduler/src/
├── assignment.rs          # T1 hard_filter + T9 tests
├── actor/dispatch.rs      # T2 no-overflow-for-FOD + T8 queue-depth metric
└── rebalancer.rs          # T3 fetcher exemption + T8 utilization metric
rio-builder/src/
├── config.rs              # T4 executor_kind field
├── runtime.rs             # T5 heartbeat wire
└── executor/
    ├── mod.rs             # T6 wrong-kind gate
    └── daemon/spawn.rs    # T7 proxy deletion
rio-builder/tests/
└── executor_kind_gate.rs  # T10 integration
```

## Dependencies

```json deps
{"deps": [451], "soft_deps": [], "note": "Hard dep on P0451: needs ExecutorKind enum, ExecutorState type, rio-worker→rio-builder rename. All file paths in this plan are post-rename."}
```

**Depends on:** [P0451](plan-0451-worker-to-builder-rename-executor-kind.md) — `ExecutorKind`/`ExecutorState` types, crate rename. This plan's file paths are post-P0451.
**Conflicts with:** P0451 touches the same files for the rename; strict sequencing (P0451 → P0452) avoids merge hell. No other frontier plans touch `assignment.rs`/`dispatch.rs`/`rebalancer.rs` per collisions scan.
