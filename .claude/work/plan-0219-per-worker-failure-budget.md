# Plan 0219: per-worker failure counts — separate from distinct-worker poison

Wave 3. Adds `per_worker_failures: HashMap<WorkerId, u32>` to `DerivationState` — tracks retry attempts **per worker**, separate from the existing `failed_workers: HashSet` (distinct-worker poison count). `best_worker` excludes any worker with ≥ `MAX_SAME_WORKER_RETRIES` (const = 2) failures on the current derivation.

**Poison threshold stays on `failed_workers.len()`** (distinct-worker count) — unchanged semantics. A derivation that fails 3× on worker A is NOT poisoned; it's just not retried on A. Fail on A, B, C → `failed_workers.len()=3` → poisoned (as today).

In-memory only, NOT persisted — `state/derivation.rs:354`/`:421` db-load paths initialize empty. Reset-on-scheduler-restart is acceptable (matches pre-4a poison TTL behavior).

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync-markers.md) merged (marker `r[sched.retry.per-worker-budget]` exists in `scheduler.md`)

## Tasks

### T1 — `feat(scheduler):` `per_worker_failures` field on `DerivationState`

At [`rio-scheduler/src/state/derivation.rs:200`](../../rio-scheduler/src/state/derivation.rs) (beside `failed_workers: HashSet`):

```rust
/// Per-worker retry budget. Separate from `failed_workers` (distinct-
/// worker poison count). `best_worker` excludes any worker with
/// >= MAX_SAME_WORKER_RETRIES failures on this derivation.
///
/// In-memory ONLY — NOT persisted. db-load at :354/:421 initializes
/// empty. Reset on scheduler restart is acceptable (matches pre-4a
/// poison TTL behavior).
pub per_worker_failures: HashMap<WorkerId, u32>,
```

Default `HashMap::new()` in the constructor at `:291`. At `:354` and `:421` (db-load), initialize empty (comment why).

### T2 — `feat(scheduler):` increment on failure

At [`rio-scheduler/src/actor/completion.rs:68`](../../rio-scheduler/src/actor/completion.rs), where `failed_workers.insert(worker_id)`:

```rust
state.failed_workers.insert(worker_id.clone());  // existing — distinct count for poison
*state.per_worker_failures.entry(worker_id.clone()).or_insert(0) += 1;  // new — per-worker budget
```

### T3 — `feat(scheduler):` `best_worker` filter

At [`rio-scheduler/src/assignment.rs`](../../rio-scheduler/src/assignment.rs), near the existing `!drv.failed_workers.contains()` filter:

```rust
// r[impl sched.retry.per-worker-budget]
const MAX_SAME_WORKER_RETRIES: u32 = 2;

// ... in the filter chain:
&& drv.per_worker_failures.get(&worker_id).copied().unwrap_or(0) < MAX_SAME_WORKER_RETRIES
```

Poison threshold at `completion.rs:76` STAYS on `failed_workers.len()` — no change.

### T4 — `test(scheduler):` 3×-same-worker vs 3-distinct

Unit test demonstrating the distinction:

1. **3× on worker A** → `per_worker_failures[A]=3`, `failed_workers.len()=1` → derivation NOT poisoned, but A excluded from `best_worker()`
2. **1× each on A, B, C** → `failed_workers.len()=3` → derivation POISONED (as today, unchanged)

Marker: `// r[verify sched.retry.per-worker-budget]`

## Exit criteria

- `/nbr .#ci` green
- 3×-same-worker test: `failed_workers.len()==1` (NOT poisoned) AND worker A excluded from `best_worker()`
- 3-distinct-workers test: `failed_workers.len()==3` (poisoned — existing behavior unchanged)

## Tracey

References existing markers:
- `r[sched.retry.per-worker-budget]` — T3 implements (assignment.rs filter); T4 verifies (distinction test)

## Files

```json files
[
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "T1: per_worker_failures HashMap at :200; default at :291; empty at :354/:421 db-load (comment why)"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T2: increment at :68 alongside failed_workers.insert (poison at :76 unchanged)"},
  {"path": "rio-scheduler/src/assignment.rs", "action": "MODIFY", "note": "T3: MAX_SAME_WORKER_RETRIES filter; r[impl sched.retry.per-worker-budget]; T4: unit test; r[verify]"}
]
```

```
rio-scheduler/src/
├── state/derivation.rs            # T1: HashMap field
├── actor/completion.rs            # T2: increment at :68
└── assignment.rs                  # T3+T4: filter + test
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [206], "note": "completion.rs:68 distant from P0206's :377 (different fns). Serialize if same-day."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync-markers.md) — marker `r[sched.retry.per-worker-budget]` must exist.

**Conflicts with:**
- [`completion.rs`](../../rio-scheduler/src/actor/completion.rs) — **P0206** touches `:377`, this touches `:68`. 300 lines apart, different functions. Low risk — parallel OK; serialize if landing same-day.
- `assignment.rs` — single-writer in 4b.
