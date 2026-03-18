# Plan 0210: heartbeat — plumb `circuit.is_open()` → `store_degraded`

Wave 2. ~20 lines of plumbing once P0205 (proto field) and P0209 (circuit breaker) merge. Reads `CircuitBreaker::is_open()` from the FUSE layer and sets `HeartbeatRequest.store_degraded` — scheduler then treats the worker like `draining` (P0211).

Also closes the deferral blockquotes at [`docs/src/challenges.md:100`](../../docs/src/challenges.md) and [`docs/src/failure-modes.md:52`](../../docs/src/failure-modes.md) — P0204 turned them into forward-refs; this plan deletes them entirely since the behavior is now implemented.

## Entry criteria

- [P0205](plan-0205-proto-heartbeat-store-degraded.md) merged (proto field `store_degraded = 9` exists)
- [P0209](plan-0209-fuse-circuit-breaker.md) merged (`CircuitBreaker::is_open()` exists)

## Tasks

### T1 — `feat(worker):` `build_heartbeat_request` — new param

At [`rio-worker/src/runtime.rs:77-124`](../../rio-worker/src/runtime.rs), `build_heartbeat_request` gains a `store_degraded: bool` parameter. Replace `store_degraded: false` at `:124` (the explicit placeholder P0205 added) with `store_degraded`.

```rust
// r[impl worker.heartbeat.store-degraded]
pub fn build_heartbeat_request(
    // ... existing params ...
    store_degraded: bool,
) -> HeartbeatRequest {
    HeartbeatRequest {
        // ... existing fields ...
        store_degraded,  // was: false (P0205 placeholder)
    }
}
```

### T2 — `feat(worker):` heartbeat loop reads circuit state

In the heartbeat loop caller: read `fuse_fs.circuit().is_open()` and pass to `build_heartbeat_request`. This needs a handle to the circuit breaker threaded from FUSE mount init → heartbeat loop. Check [`rio-worker/src/main.rs`](../../rio-worker/src/main.rs) wiring — likely the cleanest path is `Arc<CircuitBreaker>` cloned at mount time and passed into the heartbeat task, OR `Arc<NixStoreFs>` if the whole FS handle is already shared.

### T3 — `test(worker):` fix 4 test call sites

At `runtime.rs:704/734/752/765` (the 4 existing test call sites for `build_heartbeat_request`): pass `false` for the new parameter.

Add one new test: construct a mock circuit that reports `is_open() == true` → assert `HeartbeatRequest.store_degraded == true`.

### T4 — `docs:` delete deferral blockquotes

At [`docs/src/challenges.md:100`](../../docs/src/challenges.md) and [`docs/src/failure-modes.md:52`](../../docs/src/failure-modes.md): **DELETE the "Phase 4 deferral" blockquotes entirely.** P0204 turned them into forward-refs to `r[worker.fuse.circuit-breaker]`; now that the behavior is implemented, remove the deferral prose.

## Exit criteria

- `/nbr .#ci` green
- One test with mock circuit reporting open → `HeartbeatRequest.store_degraded == true`
- Deferral blockquotes removed from `challenges.md:100` and `failure-modes.md:52`

## Tracey

References existing markers:
- `r[worker.heartbeat.store-degraded]` — T1 implements (runtime.rs set site). The `r[verify]` lands in P0211 (scheduler consumes the field — scheduler verifies worker contract).

## Files

```json files
[
  {"path": "rio-worker/src/runtime.rs", "action": "MODIFY", "note": "T1: build_heartbeat_request +store_degraded param; T3: 4 test call sites + new mock-open test; r[impl worker.heartbeat.store-degraded]"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "T2: thread Arc<CircuitBreaker> (or Arc<NixStoreFs>) from FUSE mount init → heartbeat loop"},
  {"path": "docs/src/challenges.md", "action": "MODIFY", "note": "T4: delete Phase-4-deferral blockquote at :100"},
  {"path": "docs/src/failure-modes.md", "action": "MODIFY", "note": "T4: delete Phase-4-deferral blockquote at :52"}
]
```

```
rio-worker/src/
├── runtime.rs                     # T1+T3: param + r[impl]
└── main.rs                        # T2: Arc threading
docs/src/
├── challenges.md                  # T4: delete deferral
└── failure-modes.md               # T4: delete deferral
```

## Dependencies

```json deps
{"deps": [205, 209], "soft_deps": [], "note": "Hard deps on BOTH: proto field (P0205) + CircuitBreaker::is_open() (P0209)."}
```

**Depends on:**
- [P0205](plan-0205-proto-heartbeat-store-degraded.md) — `HeartbeatRequest.store_degraded` field doesn't exist before proto regen
- [P0209](plan-0209-fuse-circuit-breaker.md) — `CircuitBreaker::is_open()` doesn't exist

**Conflicts with:** `runtime.rs` is disjoint from other wave-2 plans. [`main.rs`](../../rio-worker/src/main.rs) is the top collision file (35 prior hits) but only P0210 touches it in 4b.
