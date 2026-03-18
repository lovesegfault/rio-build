# Plan 0205: proto — pin `HeartbeatRequest.store_degraded = 9`

Wave-0 serial prologue (~30min). Pins proto field 9 before any consumer touches [`types.proto`](../../rio-proto/proto/types.proto) — that file has **27 prior collisions** and this is the **only** proto edit in all of 4b. One proto line + one changelog line + one default-roundtrip test + one call-site fix.

The field carries the FUSE circuit-breaker state (P0209) from worker heartbeat to scheduler. Wire-compatible: default `false`, old workers don't send it, old schedulers read `false`. The doc-comment references `r[worker.heartbeat.store-degraded]` — the marker P0204 seeds in [`worker.md`](../../docs/src/components/worker.md).

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync-markers.md) merged (doc-comment references `r[worker.heartbeat.store-degraded]`)

## Tasks

### T1 — `feat(proto):` add `HeartbeatRequest.store_degraded` field 9

Insert at [`types.proto:368`](../../rio-proto/proto/types.proto) (after `size_class = 8;`, before closing `}`):

```proto
  // FUSE circuit breaker open — worker cannot fetch from store.
  // Scheduler treats like `draining`: `has_capacity()` returns false.
  // Cleared when breaker closes/half-opens. Default false — wire-compatible
  // (old workers don't send it, scheduler reads false).
  // Spec: r[worker.heartbeat.store-degraded]
  bool store_degraded = 9;
```

If [`docs/src/components/proto.md`](../../docs/src/components/proto.md) has a changelog section, add a one-line entry: `HeartbeatRequest: +store_degraded (field 9, bool, default false) — phase 4b`.

### T2 — `test(proto):` default-false roundtrip

Add to `rio-proto/tests/` (or the existing roundtrip test module):

```rust
#[test]
fn heartbeat_request_store_degraded_default_false() {
    let req = HeartbeatRequest::default();
    let bytes = req.encode_to_vec();
    let decoded = HeartbeatRequest::decode(&*bytes).unwrap();
    assert_eq!(decoded.store_degraded, false);
}
```

### T3 — `fix(worker):` explicit `store_degraded: false` at struct literal

At [`rio-worker/src/runtime.rs:124`](../../rio-worker/src/runtime.rs) the `HeartbeatRequest` struct literal needs the new field. Add `store_degraded: false,` **explicitly** — prefer explicit over `..Default::default()` for a heartbeat-critical struct. P0210 later replaces `false` with the real `circuit.is_open()` value; this line is the placeholder it rebases against.

## Exit criteria

- `/nbr .#ci` green (includes proto regen + workspace build)
- Field 9 in `HeartbeatRequest` is occupied; `types.proto` has no gap at 9
- `HeartbeatRequest::default().store_degraded == false` (roundtrip test passes)

## Tracey

None — proto is plumbing. The marker `r[worker.heartbeat.store-degraded]` exists (P0204 seeds it) but `r[impl]` lands in P0210 (worker sets the field from real circuit state) and `r[verify]` lands in P0211 (scheduler consumes it).

## Files

```json files
[
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "T1: add bool store_degraded = 9 after size_class = 8"},
  {"path": "rio-proto/tests/roundtrip.rs", "action": "MODIFY", "note": "T2: default-false roundtrip (or NEW file if no roundtrip.rs)"},
  {"path": "rio-worker/src/runtime.rs", "action": "MODIFY", "note": "T3: line 124 add store_degraded: false to struct literal"},
  {"path": "docs/src/components/proto.md", "action": "MODIFY", "note": "T1: changelog entry if section exists"}
]
```

```
rio-proto/
├── proto/types.proto              # T1: +field 9
└── tests/roundtrip.rs             # T2: default roundtrip
rio-worker/src/runtime.rs          # T3: 1-line explicit false
docs/src/components/proto.md       # T1: changelog
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [], "note": "ONLY types.proto touch in 4b — HARD SERIAL before P0210 and P0211"}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync-markers.md) — the proto doc-comment references `r[worker.heartbeat.store-degraded]`, which doesn't exist until P0204 seeds it.

**Conflicts with:** `types.proto` has 27 prior collisions — this is the cleanest 4b gets by being the SINGLE writer. `runtime.rs:124` is a 1-liner; P0210's larger edit replaces `false` with the real value at the same line — clean rebase, not a conflict.

**Serialization: HARD.** MUST merge before P0210 (needs the proto field to plumb into) and P0211 (reads `req.store_degraded` which doesn't exist before proto regen).
