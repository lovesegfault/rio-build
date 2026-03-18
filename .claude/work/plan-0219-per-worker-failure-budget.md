# Plan 0219: InfrastructureFailure split + configurable poison knobs

Wave 3. **Scope changed 2026-03-18** (design review; see `.claude/notes/plan-adjustments-2026-03-18.md`). Original plan added a `per_worker_failures: HashMap` — the user decision takes a different approach to the same concern (bad workers shouldn't poison a good build).

**The problem:** `BuildResultStatus::InfrastructureFailure` is OR'd with `TransientFailure` at [`completion.rs:162`](../../rio-scheduler/src/actor/completion.rs) — both route through `handle_transient_failure` → both insert into `failed_workers`. A worker with flaky FUSE (circuit breaker open), cgroup setup races, or an OOM-kill of the build process reports `InfrastructureFailure` — that's not the build's fault, but it counts toward poison anyway.

**The fix:** split the match arm. `InfrastructureFailure` gets its own handler: retry WITHOUT counting. Poison only on `TransientFailure` (build ran, exited non-zero, might succeed elsewhere — 3 workers all seeing that IS a build problem). Worker disconnect (`worker.rs:119`) keeps counting — a build that crashes the daemon 3× is poisoned; false-positives from unrelated worker deaths are rare and admin-clearable.

**Also:** `POISON_THRESHOLD` and distinct-worker semantics become config knobs (`scheduler.toml`). Defaults match current behavior (3, distinct).

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync-markers.md) merged (marker `r[sched.retry.per-worker-budget]` exists in `scheduler.md` — text already describes this design post-adjustment)

## Tasks

### T1 — `feat(scheduler):` split `InfrastructureFailure` out of the poison path

At [`rio-scheduler/src/actor/completion.rs:161-164`](../../rio-scheduler/src/actor/completion.rs), the current match arm:

```rust
rio_proto::types::BuildResultStatus::TransientFailure
| rio_proto::types::BuildResultStatus::InfrastructureFailure => {
    self.handle_transient_failure(drv_hash, worker_id).await;
}
```

Split into two arms:

```rust
rio_proto::types::BuildResultStatus::TransientFailure => {
    // Build ran, exited non-zero. Counts toward poison — 3 workers
    // all seeing this means it's not actually transient.
    self.handle_transient_failure(drv_hash, worker_id).await;
}
// r[impl sched.retry.per-worker-budget]
rio_proto::types::BuildResultStatus::InfrastructureFailure => {
    // Worker-local problem (FUSE EIO, cgroup setup fail, OOM-kill
    // of the build process). Not the build's fault. Retry WITHOUT
    // inserting into failed_workers.
    self.handle_infrastructure_failure(drv_hash, worker_id).await;
}
```

New handler `handle_infrastructure_failure` (below `handle_transient_failure` at `~:590`):

```rust
pub(super) async fn handle_infrastructure_failure(
    &mut self,
    drv_hash: &DrvHash,
    worker_id: &WorkerId,
) {
    // Reset to ready; DO NOT insert into failed_workers.
    // Poison only on TransientFailure (bona-fide build failure).
    //
    // This CAN loop if the infrastructure problem is widespread
    // (all workers have it) — but that's a cluster problem, not a
    // per-derivation one. The existing failed_workers HashSet still
    // excludes this worker from best_worker() via the Assigned→Ready
    // state-transition tracking? No — failed_workers IS the exclusion
    // list. Without inserting, this worker is immediately re-eligible.
    //
    // That's fine: InfrastructureFailure is the worker saying "I can't
    // right now." If it's still broken, it'll fail again. If it's
    // recovered (circuit closed), it'll succeed. No blacklist needed.
    info!(drv_hash = %drv_hash, worker_id = %worker_id,
          "infrastructure failure — retry without poison count");
    if let Some(state) = self.dag.node_mut(drv_hash) {
        state.reset_to_ready();
    }
}
```

**Design note:** the original plan's `per_worker_failures` HashMap would have let us cap retries on a persistently-broken worker. Without it, a worker that's ALWAYS returning InfrastructureFailure gets retried forever on that worker. This is acceptable: P0211 (`store_degraded` heartbeat → `has_capacity()` false) already excludes broken workers from assignment. The cap is upstream.

### T2 — `feat(scheduler):` config knobs `poison_threshold` + `require_distinct_workers`

At [`rio-scheduler/src/config.rs`](../../rio-scheduler/src/config.rs) (wherever `SchedulerConfig` lives — grep for it at dispatch):

```rust
#[derive(Deserialize, Debug, Clone)]
pub struct PoisonConfig {
    /// Failures before poison. Default 3 (current POISON_THRESHOLD).
    #[serde(default = "default_poison_threshold")]
    pub threshold: u32,
    /// Whether failures must be on distinct workers. Default true
    /// (HashSet semantics — current behavior). false = any N failures
    /// poison regardless of worker, for single-worker dev deployments.
    #[serde(default = "default_true")]
    pub require_distinct_workers: bool,
}
fn default_poison_threshold() -> u32 { 3 }
fn default_true() -> bool { true }
```

Replace `POISON_THRESHOLD` const (completion.rs and worker.rs both reference it — grep at dispatch) with `self.config.poison.threshold`.

**`require_distinct_workers = false` semantics:** `failed_workers` stays a `HashSet` (for `best_worker` exclusion) but the poison check becomes a separate counter that increments regardless of contain-check. Simplest: add `failure_count: u32` to `DerivationState`, increment unconditionally in `record_failure_and_check_poison`, and the return becomes:

```rust
let count = if self.config.poison.require_distinct_workers {
    s.failed_workers.len() as u32
} else {
    s.failure_count
};
count >= self.config.poison.threshold
```

Persist `failure_count` alongside `failed_workers` in PG (minor migration? check if `derivations` table already has a count column — `grep failed_ migrations/` at dispatch).

### T3 — `test(scheduler):` three-scenario verification

```rust
// r[verify sched.retry.per-worker-budget]
#[tokio::test]
async fn infrastructure_failure_does_not_count_toward_poison() {
    // 3× InfrastructureFailure on distinct workers → NOT poisoned
    // (failed_workers stays empty). 4th attempt proceeds.
}

#[tokio::test]
async fn transient_failure_counts_toward_poison() {
    // 3× TransientFailure on distinct workers → POISONED
    // (current behavior, unchanged).
}

#[tokio::test]
async fn non_distinct_mode_counts_same_worker() {
    // With require_distinct_workers=false:
    // 3× TransientFailure on SAME worker → POISONED.
    // (Would NOT poison in default distinct mode.)
}
```

## Exit criteria

- `/nbr .#ci` green
- `InfrastructureFailure` 3× → `failed_workers.is_empty()`, derivation NOT poisoned
- `TransientFailure` 3× distinct → poisoned (unchanged)
- `require_distinct_workers=false` + `TransientFailure` 3× same-worker → poisoned
- `POISON_THRESHOLD` const removed (all refs → `config.poison.threshold`)

## Tracey

References existing markers:
- `r[sched.retry.per-worker-budget]` — T1 implements (the match arm split); T3 verifies
  - Marker text already updated to describe this design (spec edit @ adjustments commit)

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T1: split :162 arm; new handle_infrastructure_failure; r[impl sched.retry.per-worker-budget]"},
  {"path": "rio-scheduler/src/config.rs", "action": "MODIFY", "note": "T2: PoisonConfig struct"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "T2: failure_count: u32 field (for non-distinct mode)"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "T2: POISON_THRESHOLD → config.poison.threshold at :125"},
  {"path": "rio-scheduler/tests/poison.rs", "action": "NEW", "note": "T3: three-scenario; r[verify sched.retry.per-worker-budget]"}
]
```

```
rio-scheduler/src/
├── actor/completion.rs            # T1: split match arm + new handler
├── actor/worker.rs                # T2: const → config
├── config.rs                      # T2: PoisonConfig
└── state/derivation.rs            # T2: failure_count field
rio-scheduler/tests/poison.rs      # T3
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [206, 211], "note": "completion.rs:162 distant from P0206's :377. P0211 (store_degraded) is the upstream exclusion for persistently-broken workers — this plan relies on it."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync-markers.md) — marker text.

**Soft dep on [P0211](plan-0211-scheduler-consume-store-degraded.md):** the "won't a broken worker retry forever?" concern in T1's design note is handled by P0211's `has_capacity() → false` on `store_degraded`. If P0211 hasn't landed, a worker with circuit-open gets retried until heartbeat timeout deregisters it — still bounded, just slower.

**Conflicts with:**
- [`completion.rs`](../../rio-scheduler/src/actor/completion.rs) — **P0206** touches `:377`, this touches `:162` and `~:590`. Different functions, 200+ lines apart. Low risk.
- [`worker.rs`](../../rio-scheduler/src/actor/worker.rs) — this touches `POISON_THRESHOLD` ref at `:125`. P0211 touches `:290`. Different functions.
