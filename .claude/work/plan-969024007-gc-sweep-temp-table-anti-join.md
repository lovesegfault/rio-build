# Plan 969024007: GC sweep O(N²) wire bytes — temp-table anti-join replaces per-path full-array bind

Reviewer perf finding at [`rio-store/src/gc/sweep.rs:196`](../../rio-store/src/gc/sweep.rs). The sweep re-check binds the FULL `unreachable` `Vec<bytea>` as `$2` inside the per-path nested loop. For a sweep of N paths, this sends N copies of an N-element `bytea[]` over the wire — O(N²) bytes. A 10k-path sweep sends ~3GB of `$2` traffic. Additionally, `<> ALL(array_param)` is an unindexed linear scan on the PG side.

The binding to `&unreachable` (not `&batch`) at `:196` is INTENTIONAL per the code comment at `:180-182` — cycles may span `SWEEP_BATCH_SIZE=100` boundaries, so the re-check must see the full unreachable set. [P0441](plan-0441-store-gc-drain-toctou-sweep-gaps.md) added this for correctness; this plan keeps the correctness while fixing the performance.

**Fix:** populate a temp table `pg_temp.sweep_unreachable(path_hash bytea PRIMARY KEY)` ONCE at sweep start, then anti-join via `NOT EXISTS (SELECT 1 FROM pg_temp.sweep_unreachable WHERE path_hash = r.referrer)`. This is hash-joinable (temp table gets a hash index), O(N) wire bytes (one `COPY` or batched `INSERT` at start), and O(1) per-path lookup.

## Entry criteria

- [P0441](plan-0441-store-gc-drain-toctou-sweep-gaps.md) merged (introduced the `&unreachable` bind for cycle-spanning correctness)

## Tasks

### T1 — `perf(store):` sweep.rs — replace per-path array bind with temp-table anti-join

At [`sweep.rs`](../../rio-store/src/gc/sweep.rs):

1. At sweep start (before the batch loop), `CREATE TEMP TABLE sweep_unreachable (path_hash bytea PRIMARY KEY) ON COMMIT DROP` and populate it from `unreachable` via `COPY FROM STDIN (FORMAT BINARY)` or batched `INSERT ... SELECT unnest($1::bytea[])`.
2. Replace the `:196` `.bind(&unreachable)` + `<> ALL($2)` with `NOT EXISTS (SELECT 1 FROM sweep_unreachable WHERE path_hash = <referrer_col>)`.
3. The temp table is session-scoped and `ON COMMIT DROP` — no cleanup needed.

Keep the `:180-182` comment explaining WHY the check must cover the full unreachable set (not just the current batch) — the temp-table approach preserves this semantics.

### T2 — `test(store):` sweep correctness preserved — cycle-spanning + external-referrer

The existing `sweep_reclaims_two_cycle` test at `:635` covers the positive case. This plan must NOT break it. Additionally verify the temp-table approach handles the same edge cases the array-bind did — run the existing tests and confirm green. (The new cross-batch-boundary + external-referrer tests are owned by P0311 T-appends from this same followup batch; coordinate.)

### T3 — `perf(store):` optional — measure wire-byte reduction

If there's an existing perf-bench harness for GC sweep, add a `sweep_10k_paths` bench comparing wire bytes before/after. Not gate-blocking; documentary.

## Exit criteria

- `/nbr .#ci` green
- `cargo nextest run -p rio-store sweep_` → all existing sweep tests pass (correctness preserved)
- `grep 'CREATE TEMP TABLE sweep_unreachable\|pg_temp.sweep_unreachable' rio-store/src/gc/sweep.rs` → ≥1 hit
- `grep '.bind(&unreachable)' rio-store/src/gc/sweep.rs` → 0 hits at the per-path re-check site (moved to one-time temp-table populate)
- `grep 'NOT EXISTS.*sweep_unreachable' rio-store/src/gc/sweep.rs` → ≥1 hit

## Tracey

References existing markers:
- `r[store.gc.sweep-cycle-reclaim]` — T1 preserves the cycle-reclaim semantics while changing the implementation strategy
- `r[store.gc.two-phase]` — the sweep phase's reference re-check is the behavior being optimized

## Files

```json files
[
  {"path": "rio-store/src/gc/sweep.rs", "action": "MODIFY", "note": "T1: :196 array-bind → temp-table anti-join; temp-table CREATE+populate at sweep start. T2: existing tests must stay green"}
]
```

```
rio-store/src/gc/
└── sweep.rs               # T1: temp-table anti-join
```

## Dependencies

```json deps
{"deps": [441], "soft_deps": [311], "note": "P0441 introduced the &unreachable bind for cycle-spanning correctness. Soft-dep P0311: test-gap T-appends from this batch add cross-batch-boundary + external-referrer tests to the same file — coordinate so those tests land against the temp-table impl, not the array-bind impl."}
```

**Depends on:** [P0441](plan-0441-store-gc-drain-toctou-sweep-gaps.md) — the `&unreachable` full-set bind this plan optimizes.
**Conflicts with:** `sweep.rs` is not in collisions top-30. P0311 batch-append T-items target `:635` (test additions); this plan edits `:180-200` (impl). Non-overlapping, but the new tests should land AFTER this plan so they exercise the temp-table path.
