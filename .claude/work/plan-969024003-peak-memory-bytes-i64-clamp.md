# Plan 969024003: peak_memory_bytes u64→i64 cast unclamped — apply mark.rs pattern

Bughunter defensive-gap finding at [`rio-scheduler/src/actor/completion.rs:1001`](../../rio-scheduler/src/actor/completion.rs). `peak_memory_bytes` (a `u64` from the worker's cgroup reading) is cast to `i64` for the PostgreSQL `BIGINT` column without clamping. A misbehaving worker reporting `u64::MAX` (or any value > `i64::MAX`) produces a negative `i64`, corrupting `build_sample` data consumed by `CutoffRebalancer`.

The safe pattern already exists at [`rio-store/src/gc/mark.rs:143`](../../rio-store/src/gc/mark.rs): `.min(i64::MAX as u64) as i64`. Apply the same here. Low-priority defensive gap — a well-behaved worker never reports values this large (physical RAM ceilings are well below 2^63 bytes), but the clamp costs nothing and prevents silent data corruption if a worker bug or malicious input ever produces a pathological value.

## Tasks

### T1 — `fix(scheduler):` completion.rs peak_memory_bytes — clamp before i64 cast

At [`completion.rs:1001`](../../rio-scheduler/src/actor/completion.rs), change:

```rust
// before
peak_memory_bytes as i64
// after
peak_memory_bytes.min(i64::MAX as u64) as i64
```

Grep for other `u64 → i64` casts in the same file — `completion.rs` writes several metrics to PG `BIGINT` columns. Apply the same clamp to any sibling unclamped casts found (e.g., `wall_time_ms`, `cpu_time_ms` if they're `u64`).

### T2 — `test(scheduler):` clamp test — u64::MAX produces i64::MAX not negative

Add a small unit test asserting the clamp: feed `u64::MAX` through the `build_sample` write path (or the clamp expression directly if the write path is hard to unit-test), assert the stored value is `i64::MAX`, not negative. If there's an existing `build_sample` test module, add there; otherwise a one-line `assert_eq!` in an inline `#[cfg(test)]` block is sufficient.

## Exit criteria

- `/nbr .#ci` green
- `grep '\.min(i64::MAX as u64) as i64' rio-scheduler/src/actor/completion.rs` → ≥1 hit
- Unit test `peak_memory_clamps_to_i64_max` (or similar) passes with `u64::MAX` input

## Tracey

No direct tracey marker — this is a defensive data-integrity fix below spec granularity. The `build_sample` table feeds `r[sched.rebalancer.sita-e]` but the clamp itself isn't spec-level behavior.

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T1: :1001 .min(i64::MAX as u64) clamp + sibling casts. T2: inline clamp test. HOT count=33"}
]
```

```
rio-scheduler/src/actor/
└── completion.rs          # T1: clamp; T2: test
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [969024001], "note": "No hard deps — the :1001 cast is stable code. Soft-dep P969024001 (same file, different hunk :1243 vs :1001; non-overlapping but both edit completion.rs HOT count=33)."}
```

**Depends on:** none — single-line defensive change.
**Conflicts with:** [`completion.rs`](../../rio-scheduler/src/actor/completion.rs) count=33 — [P969024001](plan-969024001-poison-expiry-keep-going-hang.md) edits `:1243-1258`, this plan edits `:1001`. Non-overlapping hunks; rebase-clean either order.
