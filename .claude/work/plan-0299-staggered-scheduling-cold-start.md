# Plan 0299: Staggered scheduling — cold-start prefetch warm-gate

Originates from [`challenges.md:73`](../../docs/src/challenges.md) §9 (Cold Start at Scale). The existing mitigations — `ChunkCache` LRU on rio-store, per-derivation `PrefetchHint` before dispatch — absorb most of the thundering herd. What's missing: **the scheduler dispatches to a newly-registered worker immediately**, even though its FUSE cache is completely cold. First build on a fresh worker takes the full fetch latency on *every* input path.

[P0296](plan-0296-ephemeral-builders-opt-in.md) makes this the hot path: ephemeral-mode builders (`WorkerPoolSpec.ephemeral: true`) spawn one Job per assignment, so **every build is a cold start**. Without this plan, `W_LOCALITY=0.7` in [`assignment.rs`](../../rio-scheduler/src/assignment.rs) is dead weight for ephemeral pools — every worker's bloom is empty, `count_missing()` returns full-closure for everyone, locality scoring degenerates to zero.

**The shape:** `WorkerState.warm: bool`, default `false`. The dual-register protocol already has the trigger point — when the scheduler sees first-heartbeat-with-stream (step 3 of `r[sched.worker.dual-register]`), it sends an initial `PrefetchHint` with the "common set" (glibc, stdenv, coreutils — configurable, or derived from the first queued derivation's closure). Worker fetches into FUSE cache, sends `PrefetchComplete` ACK on the stream. Scheduler flips `warm = true`. [`best_worker()`](../../rio-scheduler/src/assignment.rs) filters `!warm` workers out of the candidate set (same mechanism as `has_capacity()` / `can_build()` hard filters).

**Degenerate case:** single-worker cluster, all workers cold — if the filter is strict, nothing ever dispatches. Warm-gate must have an escape hatch: if NO warm worker passes the filter, fall back to cold workers (and log it). The gate is an *optimization*, not a correctness constraint.

## Entry criteria

- [P0296](plan-0296-ephemeral-builders-opt-in.md) merged (ephemeral builders make cold-start per-build — the motivating case)

## Tasks

### T1 — `feat(proto):` `PrefetchComplete` WorkerMessage variant

MODIFY [`rio-proto/proto/types.proto`](../../rio-proto/proto/types.proto). `PrefetchHint` is currently fire-and-forget (scheduler→worker at `:333`, no ACK path). Add a worker→scheduler ACK in the `WorkerMessage` oneof (field 4 appears skipped — use field 6 to be safe, check proto at impl):

```protobuf
message WorkerMessage {
  oneof msg {
    WorkAssignmentAck ack = 1;
    BuildLogBatch log_batch = 2;
    CompletionReport completion = 3;
    WorkerRegister register = 5;
    PrefetchComplete prefetch_complete = 6;  // Warm-gate ACK
  }
}

// Worker signals its FUSE cache has warmed the hinted paths.
// Scheduler flips WorkerState.warm = true on receipt.
// paths_fetched is for observability (metric) — the scheduler
// doesn't gate on it, just on receipt of the message.
message PrefetchComplete {
  uint32 paths_fetched = 1;   // How many hint paths were actually fetched
  uint32 paths_cached = 2;    // How many were already in FUSE cache (0 for fresh worker)
}
```

### T2 — `feat(scheduler):` `WorkerState.warm` field + gate in `best_worker`

MODIFY [`rio-scheduler/src/state/worker.rs`](../../rio-scheduler/src/state/worker.rs) — add after `bloom`:

```rust
/// Warm-gate: true once the worker has ACKed the initial PrefetchHint
/// (PrefetchComplete on the BuildExecution stream). Cold workers
/// (warm=false) are filtered out of best_worker() candidates UNLESS
/// no warm worker passes the hard filter. Default false on
/// registration; flipped by handle_prefetch_complete().
///
/// Ephemeral pools (P0296): every worker starts cold. Without this
/// gate the first build on a fresh Job-worker eats full-closure
/// fetch latency. With it, the scheduler waits for cache warm
/// before dispatching — adds ~prefetch-time to time-to-first-
/// dispatch, but the build itself runs at warm speed.
pub warm: bool,
```

MODIFY [`rio-scheduler/src/assignment.rs`](../../rio-scheduler/src/assignment.rs) — `best_worker()` at `:165`. Two-pass filter:

```rust
// r[impl sched.assign.warm-gate]
// First pass: warm workers only. This is the normal path.
let warm_candidates: Vec<&WorkerState> = workers
    .values()
    .filter(|w| w.warm && hard_filter(w, drv, target_class))
    .collect();

// Fallback: no warm workers pass → relax the gate. Single-worker
// clusters, mass scale-up where ALL workers are fresh — can't
// deadlock. The gate is an optimization not a correctness constraint.
let candidates = if !warm_candidates.is_empty() {
    warm_candidates
} else {
    let cold: Vec<&WorkerState> = workers
        .values()
        .filter(|w| hard_filter(w, drv, target_class))
        .collect();
    if !cold.is_empty() {
        tracing::debug!(
            cold_count = cold.len(),
            "warm-gate fallback: no warm workers pass filter, dispatching cold"
        );
        metrics::counter!("rio_scheduler_warm_gate_fallback_total").increment(1);
    }
    cold
};
```

Extract the existing inline filter (`:174-199`) into a `hard_filter(w, drv, target_class) -> bool` helper so it's called from both passes without duplication.

### T3 — `feat(scheduler):` send initial PrefetchHint on registration + handle ACK

MODIFY the actor's stream-message handler (wherever `WorkerRegister` is currently processed — grep `WorkerRegister` in `rio-scheduler/src/`). On registration complete (first heartbeat + open stream):

```rust
// After creating the WorkerState entry — send initial prefetch.
// "Common set" derived from: configured paths (scheduler.toml
// `warm_paths`) OR if none configured, the input closure of the
// highest-priority queued derivation. Empty queue → skip, flip
// warm=true immediately (nothing to prefetch for).
if let Some(hint) = build_initial_prefetch_hint(&self.config, &self.dag) {
    let _ = worker.stream_tx.as_ref().unwrap()
        .send(SchedulerMessage { msg: Some(Msg::Prefetch(hint)) })
        .await;
    // warm stays false until PrefetchComplete arrives
} else {
    worker.warm = true; // empty queue, nothing to warm — gate open
}
```

Add a `WorkerMessage::PrefetchComplete` arm to the stream receiver loop:

```rust
Some(Msg::PrefetchComplete(pc)) => {
    if let Some(w) = self.workers.get_mut(&worker_id) {
        w.warm = true;
        metrics::histogram!("rio_scheduler_warm_prefetch_paths")
            .record(pc.paths_fetched as f64);
    }
}
```

### T4 — `feat(worker):` send `PrefetchComplete` after prefetch finishes

MODIFY the worker's `PrefetchHint` handler (grep `PrefetchHint` in `rio-worker/src/`). Currently it fetches paths into FUSE cache and returns. After the fetch loop:

```rust
// Signal warm — scheduler gates dispatch on this.
let _ = stream_tx.send(WorkerMessage {
    msg: Some(Msg::PrefetchComplete(PrefetchComplete {
        paths_fetched: fetched_count,
        paths_cached: cached_count,
    })),
}).await;
```

For ephemeral workers (P0296 single-shot mode): this fires once per pod lifetime (first and only PrefetchHint), then the single assignment arrives.

### T5 — `test:` warm-gate VM scenario

New subtest in [`nix/tests/scenarios/scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) (or `lifecycle.nix` — wherever worker-registration scenarios already live). Col-0 marker before `{` per tracey `.nix` parser rule:

```nix
# r[verify sched.assign.warm-gate]
# Scenario: fresh worker registers, scheduler sends PrefetchHint,
# worker ACKs PrefetchComplete, THEN assignment arrives. Asserts:
# (1) no WorkAssignment on the stream before PrefetchComplete is sent
# (2) rio_scheduler_warm_gate_fallback_total stays 0 (warm path taken)
# (3) time-from-register-to-first-assignment > prefetch duration
```

Scenario script: start a worker, inject a delay in the worker's prefetch handler (via env var `RIO_TEST_PREFETCH_DELAY_MS`), submit a build, assert the build's `assigned_at - worker_registered_at >= delay`. Second worker with no delay should receive assignments while the first is still warming (proves the gate is per-worker, not global).

## Exit criteria

- Fresh worker receives `PrefetchHint` within one tick of registration (unit test with `start_paused`)
- `best_worker()` with one warm + one cold worker → picks warm; with zero warm → falls back to cold and increments `rio_scheduler_warm_gate_fallback_total`
- Ephemeral-pool worker (P0296 Job mode) receives its single assignment only after `PrefetchComplete` — VM test asserts ordering on the stream
- `r[sched.assign.warm-gate]` shows both `impl` (assignment.rs) and `verify` (scheduling.nix) in tracey

## Tracey

References existing markers:
- `r[sched.worker.dual-register]` — T3 extends registration with prefetch-on-register (same protocol, new step)

Adds new markers to component specs:
- `r[sched.assign.warm-gate]` → `docs/src/components/scheduler.md` (see ## Spec additions below)

## Spec additions

New standalone marker in [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md), inserted between `r[sched.worker.dual-register]` (line `:390-401`) and `r[sched.worker.deregister-reassign]` (line `:403`):

```markdown
r[sched.assign.warm-gate]

A newly-registered worker (step 3 of `r[sched.worker.dual-register]` — first heartbeat with open stream) receives an initial `PrefetchHint` before any `WorkAssignment`. The worker fetches the hinted paths into its FUSE cache and replies with `PrefetchComplete` on the `BuildExecution` stream. The scheduler's `WorkerState.warm` flag starts `false` and flips `true` on receipt. `best_worker()` filters out `warm=false` workers from its candidate set — but falls back to cold workers if no warm worker passes the hard filter (single-worker clusters and mass-scale-up must not deadlock). Empty scheduler queue at registration time → warm flips true immediately (nothing to prefetch for). The warm-gate is per-worker: a second worker registering while the first is still warming does not delay builds that the second (already warm) worker can take.
```

## Files

```json files
[
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "T1: PrefetchComplete message + WorkerMessage oneof field 6"},
  {"path": "rio-scheduler/src/state/worker.rs", "action": "MODIFY", "note": "T2: WorkerState.warm field"},
  {"path": "rio-scheduler/src/assignment.rs", "action": "MODIFY", "note": "T2: best_worker two-pass warm filter + hard_filter() extract"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "T3: send PrefetchHint on register, handle PrefetchComplete"},
  {"path": "rio-worker/src/runtime.rs", "action": "MODIFY", "note": "T4: send PrefetchComplete after prefetch loop"},
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "T5: warm-gate ordering scenario + col-0 r[verify] marker"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "Spec: r[sched.assign.warm-gate] marker between dual-register and deregister-reassign"}
]
```

```
rio-proto/proto/
└── types.proto          # T1: PrefetchComplete
rio-scheduler/src/
├── state/worker.rs      # T2: warm field
├── assignment.rs        # T2: warm-gate filter
└── actor/mod.rs         # T3: hint-on-register + ACK handler
rio-worker/src/
└── runtime.rs           # T4: send PrefetchComplete
nix/tests/scenarios/
└── scheduling.nix       # T5: VM scenario
docs/src/components/
└── scheduler.md         # Spec marker
```

## Dependencies

```json deps
{"deps": [296], "soft_deps": [], "note": "phase-cleanup: challenges.md:73 user-decided deferral. P0296 ephemeral builders make every build a cold start — this becomes hot-path. discovered_from=296 (ephemeral mode is the motivating case). Without 296, STS workers warm once at pod-start and stay warm — the gate fires rarely."}
```

**Depends on:** [P0296](plan-0296-ephemeral-builders-opt-in.md) — ephemeral Job-per-assignment mode. Makes cold-start per-build. T4 interacts with P0296's single-shot worker runtime.

**Conflicts with:**
- [`assignment.rs`](../../rio-scheduler/src/assignment.rs) count=8, UNIMPL=[230]. P0230 target unknown without reading it — check at dispatch. The `best_worker()` filter block at `:174-199` is the shared touch-point.
- [`state/worker.rs`](../../rio-scheduler/src/state/worker.rs) count=6, UNIMPL=[211]. T2 appends a field at struct tail — likely independent of P0211's changes.
- [`types.proto`](../../rio-proto/proto/types.proto) — heavy proto file. T1 is EOF-additive (new message + one oneof field). P0296 also touches proto (ephemeral dispatch RPC) — since P0296 is a hard dep, sequential, no conflict.
- [`runtime.rs`](../../rio-worker/src/runtime.rs) — P0296 touches this heavily (single-shot exit path). T4 is a small addition inside the PrefetchHint handler. Serial after P0296.
- [`scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) — check collisions at dispatch; shared by several VM-scenario plans.
