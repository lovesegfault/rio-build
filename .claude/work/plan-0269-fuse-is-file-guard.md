# Plan 0269: fuse/ops.rs is_file guard (trivial TODO close)

Closes `TODO(phase5)` at [`fuse/ops.rs:361`](../../rio-worker/src/fuse/ops.rs). ~10 lines. Noise reduction: skip passthrough for directories.

## Entry criteria

- [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) merged (frontier root)

## Tasks

### T1 — `fix(worker):` add is_file guard before open_backing

MODIFY [`rio-worker/src/fuse/ops.rs`](../../rio-worker/src/fuse/ops.rs) at `:361`:

```rust
// Before: TODO(phase5): is_file guard
// After:
if !file.metadata()?.is_file() {
    return Err(libc::EISDIR);  // or skip passthrough, caller handles
}
```

## Exit criteria

- `/nbr .#ci` green
- `rg 'TODO\(phase5\)' rio-worker/src/fuse/ops.rs` → 0

## Tracey

none — trivial guard.

## Files

```json files
[
  {"path": "rio-worker/src/fuse/ops.rs", "action": "MODIFY", "note": "T1: is_file guard at :361 (~10 lines)"}
]
```

```
rio-worker/src/fuse/
└── ops.rs                        # T1: guard at :361
```

## Dependencies

```json deps
{"deps": [245], "soft_deps": [], "note": "Trivial ~10 lines. No collision."}
```

**Depends on:** [P0245](plan-0245-prologue-phase5-markers-gt-verify.md).
**Conflicts with:** none.
