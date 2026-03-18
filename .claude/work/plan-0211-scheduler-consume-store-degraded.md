# Plan 0211: scheduler — consume `store_degraded` in WorkerState + has_capacity

Wave 2. Scheduler-side consumption of the `store_degraded` heartbeat flag: add the field to `WorkerState`, gate `has_capacity()` on it (same as `draining`), copy it from `HeartbeatRequest` in `handle_heartbeat`. A worker with an open FUSE circuit gets excluded from `best_worker()` — builds aren't assigned to a worker that can't fetch inputs.

**P0210 is NOT a hard dep.** Scheduler can consume a field nobody sends yet — proto wire-default is `false`, old workers just don't set it. Only P0205 (proto regen) is hard: `req.store_degraded` doesn't compile before that.

## Entry criteria

- [P0205](plan-0205-proto-heartbeat-store-degraded.md) merged (`req.store_degraded` exists in regenerated proto types)

## Tasks

### T1 — `feat(scheduler):` `WorkerState.store_degraded` + `has_capacity()` gate

At [`rio-scheduler/src/state/worker.rs:76`](../../rio-scheduler/src/state/worker.rs) (next to `draining`):

```rust
/// FUSE circuit breaker open on the worker — it can't fetch from store.
/// Treated like `draining`: `has_capacity()` returns false.
/// Wire-default false (old workers don't send it).
pub store_degraded: bool,
```

Default `false` in the constructor at `:104`.

At `has_capacity()` (`:124-128`):

```rust
pub fn has_capacity(&self) -> bool {
    !self.draining && !self.store_degraded && /* ... existing arithmetic ... */
}
```

Update the doc-comment at `:56-57` (currently mentions `draining` only) → mention both flags.

### T2 — `feat(scheduler):` copy field in `handle_heartbeat`

At [`rio-scheduler/src/actor/worker.rs:290`](../../rio-scheduler/src/actor/worker.rs), wherever other fields are copied from `HeartbeatRequest` → `WorkerState`:

```rust
let was_degraded = w.store_degraded;
w.store_degraded = req.store_degraded;
if !was_degraded && req.store_degraded {
    info!(worker_id = %w.id, "marked store-degraded; removing from assignment pool");
}
```

### T3 — `test(scheduler):` `has_capacity()` gates on `store_degraded`

Unit test in `state/worker.rs` tests module: `has_capacity()` returns `false` when `store_degraded=true` regardless of capacity arithmetic (set draining=false, set all capacity fields to max, set store_degraded=true → assert false).

### T4 — `test(scheduler):` degraded worker excluded from `best_worker()`

Integration test in actor tests: send heartbeat with `store_degraded=true` → assert worker is NOT in `best_worker()` result set. Marker: `// r[verify worker.heartbeat.store-degraded]` (scheduler verifies the worker contract).

## Exit criteria

- `/nbr .#ci` green
- `has_capacity()` unit test: returns false when `store_degraded=true` (all other fields favorable)
- `best_worker()` integration test: degraded worker excluded from assignment

## Tracey

References existing markers:
- `r[worker.heartbeat.store-degraded]` — T4 verifies (scheduler consumption test proves the worker→scheduler contract end-to-end)

## Files

```json files
[
  {"path": "rio-scheduler/src/state/worker.rs", "action": "MODIFY", "note": "T1: add store_degraded field at :76, default :104, gate has_capacity :124, comment :56; T3: unit test"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "T2: copy req.store_degraded in handle_heartbeat at :290; info! on false→true; T4: r[verify worker.heartbeat.store-degraded]"}
]
```

```
rio-scheduler/src/
├── state/worker.rs                # T1+T3: field + gate + unit test
└── actor/worker.rs                # T2+T4: copy + integration test
```

## Dependencies

```json deps
{"deps": [205], "soft_deps": [210], "note": "P0205 hard (proto regen). P0210 NOT hard — wire-default false is fine. Soft-serial w/ P0214 on actor/worker.rs (prefer P0214 first)."}
```

**Depends on:** [P0205](plan-0205-proto-heartbeat-store-degraded.md) — `req.store_degraded` doesn't exist before proto regen. [P0210](plan-0210-heartbeat-plumb-store-degraded.md) is NOT a hard dep: scheduler can consume a field nobody sends yet (wire-default `false`).

**Conflicts with:**
- [`actor/worker.rs`](../../rio-scheduler/src/actor/worker.rs) — **P0214** touches `handle_tick` at `:440`; this touches `handle_heartbeat` at `:290`. 150 lines apart, different functions, low merge risk. **Prefer P0214 first** (it has no deps) → this rebases trivially.
