# Plan 0419: CA-compare slow-store test — shrink 30s wall-clock

rev-p311 perf finding at [`rio-scheduler/src/actor/tests/completion.rs:339`](../../rio-scheduler/src/actor/tests/completion.rs). `ca_cutoff_compare_slow_store_doesnt_block_completion` — [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)'s regression guard for the `DEFAULT_GRPC_TIMEOUT` wrapper at [`actor/completion.rs:411-428`](../../rio-scheduler/src/actor/completion.rs) — pays **~30s wall-clock on every nextest run**. The test arms `MockStore.content_lookup_hang` then waits for the actor's internal `DEFAULT_GRPC_TIMEOUT = 30s` (at [`rio-common/src/grpc.rs:15`](../../rio-common/src/grpc.rs)) to fire.

The doc-comment at [`:332-337`](../../rio-scheduler/src/actor/tests/completion.rs) says "doesn't use `start_paused`: real TCP between actor and MockStore … pausing time wouldn't help (and would break the PG pool acquisition per lang-gotchas). This test pays the 30s wall-clock cost — acceptable for a once-per-CI regression guard." **Correct reasoning, wrong conclusion** — 30s × every-CI-run compounds. The test is load-bearing (proves the timeout wrapper survives refactors) but the timeout constant isn't load-bearing at 30s specifically.

Two fixes, take **both** (complementary):

1. **`cfg(test)` shorter timeout:** `DEFAULT_GRPC_TIMEOUT` at [`grpc.rs:15`](../../rio-common/src/grpc.rs) becomes `Duration::from_secs(if cfg!(test) { 3 } else { 30 })` — same wrapper-exists proof, 10× faster. The test's outer 60s guard shrinks to 10s.
2. **nextest slow-group assignment:** [`.config/nextest.toml:73`](../../.config/nextest.toml) filter `test(/^tests::/)` misses `actor::tests::completion::*` paths — they fall into the DEFAULT group with 30s×3 slow-timeout. Not a correctness bug (timeouts generous enough) but group semantics are mismatched. Widen the filter. Tracked as [P0304-T195](plan-0304-trivial-batch-p0222-harness.md) — that batch handles the config fix; this plan handles the test-code fix.

The `cfg(test)` gate affects **all** `DEFAULT_GRPC_TIMEOUT` callsites (5 files, 8 uses). All are metadata-RPC wrappers; none have tests that depend on the 30s value specifically (other slow-timeout tests either use `tokio::time::pause()` + paused clock, or set up their own `tokio::time::timeout` with explicit durations). Grep at dispatch to verify.

## Entry criteria

- [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) merged (**DONE** — the test at [`completion.rs:339`](../../rio-scheduler/src/actor/tests/completion.rs) exists)

## Tasks

### T1 — `perf(common):` DEFAULT_GRPC_TIMEOUT cfg(test) 3s override

MODIFY [`rio-common/src/grpc.rs`](../../rio-common/src/grpc.rs) at [`:15`](../../rio-common/src/grpc.rs):

```rust
/// Default timeout for metadata gRPC calls (QueryPathInfo, FindMissingPaths, etc.).
///
/// Should be long enough for a round trip under load, short enough that a
/// stuck server doesn't hang callers indefinitely.
///
/// Under `cfg(test)`: 3s. Tests that probe timeout-wrapper-exists
/// (e.g. `ca_cutoff_compare_slow_store_doesnt_block_completion` at
/// rio-scheduler/src/actor/tests/completion.rs) use a hung MockStore
/// and wait for THIS constant to fire. 30s × every-CI-run compounds;
/// 3s proves the same wrapper-exists invariant.
///
/// NOTE: `cfg(test)` is per-crate — this only affects tests that
/// compile rio-common in test configuration (i.e. `cargo test -p
/// rio-common`). Cross-crate callers (rio-scheduler tests using
/// MockStore over real TCP) still see 30s UNLESS rio-scheduler
/// re-exports a test-gated constant OR the timeout is plumbed as a
/// parameter. See T2 for the plumbing approach.
#[cfg(not(test))]
pub const DEFAULT_GRPC_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
pub const DEFAULT_GRPC_TIMEOUT: Duration = Duration::from_secs(3);
```

**Wait — this doesn't work.** `cfg(test)` is per-crate; `rio-scheduler`'s test build still links against `rio-common` built **without** `cfg(test)`. The constant at `completion.rs:412` will still be 30s.

### T2 — `perf(scheduler):` plumb content_lookup timeout through actor config

SUPERSEDES T1 (which is a wrong approach; keep the doc-comment reasoning above as a DON'T-example). MODIFY [`rio-scheduler/src/actor/completion.rs`](../../rio-scheduler/src/actor/completion.rs) at the two `DEFAULT_GRPC_TIMEOUT` sites ([`:122`](../../rio-scheduler/src/actor/completion.rs) and [`:412`](../../rio-scheduler/src/actor/completion.rs)). Replace the const with `self.grpc_timeout` (or a getter) plumbed from actor construction.

The actor's `ActorState` (or whatever struct holds the store_client) gets a new field `grpc_timeout: Duration` defaulting to `rio_common::grpc::DEFAULT_GRPC_TIMEOUT`. Tests that need the 30s→3s override pass `Duration::from_secs(3)` at actor spawn.

**Verify at dispatch** where actor-config lives. Two likely anchors:
- `setup_actor_with_store` at [`rio-scheduler/src/actor/tests/mod.rs`](../../rio-scheduler/src/actor/tests/mod.rs) (or wherever the test helper is) — add a `.with_grpc_timeout(Duration::from_secs(3))` builder call
- `ActorState` / `spawn_with_leader` at [`rio-scheduler/src/actor/handle.rs`](../../rio-scheduler/src/actor/handle.rs) — add the field + pass-through (P0304-T179 proposes a `DeployConfig` bundle for exactly this kind of knob; if T179 dispatched, add `grpc_timeout` to the bundle instead of as a bare arg)

MODIFY [`rio-scheduler/src/actor/tests/completion.rs`](../../rio-scheduler/src/actor/tests/completion.rs) at `ca_cutoff_compare_slow_store_doesnt_block_completion`:
- [`:348`](../../rio-scheduler/src/actor/tests/completion.rs) `setup_actor_with_store(...)` → pass the 3s override
- [`:384-389`](../../rio-scheduler/src/actor/tests/completion.rs) outer guard `Duration::from_secs(60)` → `Duration::from_secs(10)` (3s internal + margin)
- [`:332-337`](../../rio-scheduler/src/actor/tests/completion.rs) doc-comment — update "pays the 30s wall-clock cost" to "3s under test override; production uses `DEFAULT_GRPC_TIMEOUT=30s`"

Also update [`rio-scheduler/src/actor/merge.rs:556,582`](../../rio-scheduler/src/actor/merge.rs) — same pattern, replace const with plumbed field.

### T3 — `test(scheduler):` mutation — grpc_timeout removal fails the test fast

The regression guard proves the timeout wrapper EXISTS. Verify it still fails when the wrapper is removed, at the **shorter** timeout:

1. Comment out the `tokio::time::timeout(self.grpc_timeout, ...)` wrapper at `completion.rs:411` (leaving bare `store_client.content_lookup(req).await`)
2. Run `cargo nextest run -p rio-scheduler ca_cutoff_compare_slow_store_doesnt_block_completion`
3. Test fails at the **10s** outer guard (not 60s) with `"actor blocked past 10s — grpc_timeout wrapper removed?"`
4. Restore the wrapper

This proves the 3s override didn't neuter the regression guard.

## Exit criteria

- `/nixbuild .#ci` green (or clause-4c nextest-standalone)
- `time cargo nextest run -p rio-scheduler ca_cutoff_compare_slow_store_doesnt_block_completion` → wall-clock ≤5s (was ~30s)
- `grep 'DEFAULT_GRPC_TIMEOUT' rio-scheduler/src/actor/completion.rs rio-scheduler/src/actor/merge.rs` → 0 hits (all plumbed)
- `grep 'grpc_timeout' rio-scheduler/src/actor/completion.rs` → ≥2 hits (`:122` + `:412` sites migrated)
- `grep 'Duration::from_secs(60)' rio-scheduler/src/actor/tests/completion.rs` → 0 hits at the slow-store test (outer guard shrunk to 10s)
- `grep '#\[cfg(test)\].*DEFAULT_GRPC_TIMEOUT' rio-common/src/grpc.rs` → 0 hits (T1's wrong-approach NOT taken; no cfg-test const)
- **Mutation (T3):** wrapper-removal → test fails within 10s, not 60s. Document in commit body.
- `rio_common::grpc::DEFAULT_GRPC_TIMEOUT` still 30s in production (`grep 'from_secs(30)' rio-common/src/grpc.rs` → ≥1 hit, unchanged)

## Tracey

References existing markers:
- `r[sched.ca.cutoff-compare]` at [`scheduler.md:282`](../../docs/src/components/scheduler.md) — the test's `r[verify]` at [`completion.rs:319`](../../rio-scheduler/src/actor/tests/completion.rs) stays; this plan shrinks the wall-clock cost, not the coverage.

No new markers. Test-infrastructure perf, not spec-behavior change.

## Files

```json files
[
  {"path": "rio-common/src/grpc.rs", "action": "MODIFY", "note": "T2: DEFAULT_GRPC_TIMEOUT stays 30s; doc-comment notes test-override happens via actor-config plumbing not cfg(test). Low-traffic (count<10)"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T2: :122+:412 DEFAULT_GRPC_TIMEOUT → self.grpc_timeout (HOT count=31 — 2-line edits at specific sites, low conflict)"},
  {"path": "rio-scheduler/src/actor/merge.rs", "action": "MODIFY", "note": "T2: :556+:582 DEFAULT_GRPC_TIMEOUT → self.grpc_timeout (count~10)"},
  {"path": "rio-scheduler/src/actor/handle.rs", "action": "MODIFY", "note": "T2: +grpc_timeout field to ActorState/spawn_with_leader (or DeployConfig bundle if P0304-T179 landed). count~15"},
  {"path": "rio-scheduler/src/actor/tests/mod.rs", "action": "MODIFY", "note": "T2: setup_actor_with_store — default grpc_timeout=3s for tests (or add builder method)"},
  {"path": "rio-scheduler/src/actor/tests/completion.rs", "action": "MODIFY", "note": "T2: :332-337 doc-comment update, :384-389 outer guard 60s→10s. T3: mutation-verify in commit body"}
]
```

```
rio-common/src/grpc.rs                          # T2: doc-comment (no cfg(test))
rio-scheduler/src/actor/
├── completion.rs                               # T2: :122 + :412 → self.grpc_timeout
├── merge.rs                                    # T2: :556 + :582 → self.grpc_timeout
├── handle.rs                                   # T2: +grpc_timeout field/bundle
└── tests/
    ├── mod.rs                                  # T2: setup helper default 3s
    └── completion.rs                           # T2: outer guard 10s + doc-comment
```

## Dependencies

```json deps
{"deps": [311], "soft_deps": [304], "note": "P0311 DONE — ca_cutoff_compare_slow_store_doesnt_block_completion test exists (discovered_from=311). Soft-dep P0304-T179 (DeployConfig bundle — if that dispatches first, grpc_timeout becomes a bundle field not a bare spawn_with_leader arg; sequence-independent, both orders compile). Soft-dep P0304-T148/T157/T169/T192 (all touch completion.rs at different sections — T148@:309-333 outcome-label, T157@:295-304 malformed-label, T169@:68,84,511 MAX_CASCADE rename, T192@:77,363 doc-comment; this plan at :122+:412; all non-overlapping hunks)."}
```

**Depends on:** [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) — the slow-store test exists.

**Conflicts with:** `rio-scheduler/src/actor/completion.rs` count=31 (HOT) — this plan edits `:122` + `:412` (timeout const → field). [P0304](plan-0304-trivial-batch-p0222-harness.md) T148 edits `:309-333`, T157 edits `:295-304`, T169 edits `:68,:84,:511`, T192 edits `:77,:363` — **all non-overlapping**. `rio-scheduler/src/actor/handle.rs` — P0304-T179 proposes `DeployConfig` bundle at `spawn_with_leader`; if T179 dispatches first, add `grpc_timeout` to the bundle. `rio-scheduler/src/actor/merge.rs` — low-traffic, no in-flight plans touch `:556+`.
