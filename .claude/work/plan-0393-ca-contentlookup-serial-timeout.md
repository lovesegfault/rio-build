# Plan 0393: CA ContentLookup — serial timeout + breaker integration

[P0251](plan-0251-ca-cutoff-compare.md) shipped the CA hash-compare hook at [`completion.rs:289-337`](../../rio-scheduler/src/actor/completion.rs). For each output of a completed CA derivation, it fires a `ContentLookup` RPC with a `DEFAULT_GRPC_TIMEOUT` (30s per [`grpc.rs:15`](../../rio-common/src/grpc.rs)) wrapped timeout. Review found three compounding latency issues:

1. **Serial 30s×N worst case:** the loop at `:294-333` awaits each RPC before sending the next. A multi-output CA derivation (e.g., `dev`/`out`/`doc`/`man` — 4 outputs) with a slow/dead store = up to **120s** inside the actor event loop. The comment at [`merge.rs:542-545`](../../rio-scheduler/src/actor/merge.rs) ("synchronous call inside the single-threaded actor event loop") applies here too: no heartbeats, completions, or dispatches are processed until the compare loop returns.

2. **No short-circuit on miss:** `all_matched &= matched` at `:327` continues looking up remaining outputs after a miss. The AND-fold semantics mean one miss → `all_matched = false` regardless of remaining results. Continuing the loop still records per-output metrics (useful) but burns timeout budget on a result that can't change.

3. **`cache_breaker` not integrated:** [`merge.rs:561-586`](../../rio-scheduler/src/actor/merge.rs) records success/failure against `CacheCheckBreaker` for `FindMissingPaths`. The completion-hook `ContentLookup` hits the same store but doesn't consult or update the breaker — so a tripped breaker doesn't skip ContentLookup (wasted timeout), and ContentLookup failures don't contribute to tripping the breaker (slower detection of store outages).

All three are perf, not correctness: the hook already treats RPC-fail/timeout as `matched=false` (safe — downstream builds run when they could've been skipped). But a 5-output CA derivation with a dead store blocks the actor for 150s — long enough for worker heartbeat timeouts to fire and mark workers dead.

## Entry criteria

- [P0251](plan-0251-ca-cutoff-compare.md) merged (CA compare hook at `completion.rs:289-337` exists)

## Tasks

### T1 — `perf(scheduler):` short-circuit on first miss + early breaker check

MODIFY [`rio-scheduler/src/actor/completion.rs`](../../rio-scheduler/src/actor/completion.rs) at `:289-337`. Three changes:

```rust
// r[impl sched.ca.cutoff-compare]
if let (Some(state), Some(store_client)) =
    (self.dag.node(drv_hash), self.store_client.as_ref())
    && state.is_ca
{
    // Breaker gate: if the FindMissingPaths breaker is open
    // (merge.rs:569), skip ContentLookup entirely. Same store,
    // same unavailability signal. Degrades to "no cutoff" which
    // is the safe fallback already documented at :274-276.
    if self.cache_breaker.is_open() {
        debug!(drv_hash = %drv_hash,
               "CA cutoff-compare: skipping (cache breaker open)");
        // all_matched defaults to false when we skip the loop
    } else {
        let mut all_matched = !result.built_outputs.is_empty();
        for output in &result.built_outputs {
            if output.output_hash.len() != 32 {
                // ... unchanged 32-byte guard ...
                all_matched = false;
                continue;
            }
            // ... unchanged RPC body, timeout, match/miss ...
            all_matched &= matched;
            metrics::counter!("rio_scheduler_ca_hash_compares_total",
                              "outcome" => if matched { "match" } else { "miss" })
                .increment(1);
            // Short-circuit: one miss means the derivation's
            // outputs aren't byte-identical AS A WHOLE. Remaining
            // lookups can't flip the AND-fold. Skip them — saves
            // (N-1)×30s worst case. The per-output metric for
            // skipped outputs is NOT recorded (they weren't
            // compared); add a skip counter instead.
            if !matched {
                let skipped = result.built_outputs.len()
                    - result.built_outputs.iter()
                        .position(|o| o.output_name == output.output_name)
                        .unwrap_or(0) - 1;
                if skipped > 0 {
                    metrics::counter!("rio_scheduler_ca_hash_compares_total",
                                      "outcome" => "skipped_after_miss")
                        .increment(skipped as u64);
                }
                break;
            }
        }
        if let Some(state) = self.dag.node_mut(drv_hash) {
            state.ca_output_unchanged = all_matched;
        }
    }
}
```

The breaker check needs `is_open()` to be `pub(super)` — it's currently `fn is_open(&self) -> bool` (private) at [`breaker.rs:95`](../../rio-scheduler/src/actor/breaker.rs). Widen visibility.

### T2 — `perf(scheduler):` shorter per-RPC timeout + breaker feedback

Continue MODIFY same hook. Replace `DEFAULT_GRPC_TIMEOUT` (30s) with a shorter cutoff-specific timeout:

```rust
// Content-lookup is a PK point-lookup on content_index(nar_hash)
// — sub-10ms when the store is healthy. 30s is the DEFAULT
// "unary RPC over an unreliable link" budget; ContentLookup
// doesn't need it. 2s is generous for a PK lookup + gRPC
// overhead + one retry-worth of network. If it takes >2s the
// store is in trouble and the breaker should hear about it.
const CONTENT_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);
```

On both the `Ok(Err(e))` and `Err(_elapsed)` arms, call `self.cache_breaker.record_failure()` (same pattern as [`merge.rs:569,583`](../../rio-scheduler/src/actor/merge.rs)). On `Ok(Ok(resp))`, call `record_success()`. This makes ContentLookup failures trip the breaker faster AND benefits from `FindMissingPaths` having already tripped it.

**Worst-case latency with T1+T2:** `min(N, 1+miss_idx) × 2s` instead of `N × 30s`. For a 5-output derivation with the store dead: `1 × 2s = 2s` (first lookup times out, breaker trips, subsequent completions skip).

### T3 — `perf(scheduler):` batch lookup via FindMissingPaths-style RPC (optional, best-ROI)

NEW [`rio-proto/proto/types.proto`](../../rio-proto/proto/types.proto) message + handler. The merge-time cache check at [`merge.rs:548`](../../rio-scheduler/src/actor/merge.rs) already batches: one `FindMissingPaths` call with all paths, one roundtrip. `ContentLookup` is single-hash. Add `ContentLookupBatch`:

```proto
message ContentLookupBatchRequest {
  repeated bytes content_hashes = 1;  // each 32 bytes
}
message ContentLookupBatchResponse {
  // Index-aligned with request. Empty string = miss.
  repeated string store_paths = 1;
}
```

Store-side handler at [`rio-store/src/grpc/mod.rs`](../../rio-store/src/grpc/mod.rs): `SELECT nar_hash, store_path FROM content_index WHERE nar_hash = ANY($1)` — one PG roundtrip.

Scheduler-side: replace the loop with a single batched call. 5-output derivation → 1 RPC instead of 5. Applies the 2s timeout once, not per-output.

**T3 is OPTIONAL** — T1+T2 alone drop worst-case from 150s→2s. T3 improves the happy-path (healthy store, all-match) from `N × ~10ms` to `1 × ~10ms`. Land T1+T2 first; T3 is the follow-on if profiling shows completion-hook latency matters under normal load.

### T4 — `test(scheduler):` short-circuit + breaker integration tests

MODIFY [`rio-scheduler/src/actor/tests/completion.rs`](../../rio-scheduler/src/actor/tests/completion.rs). Three tests near the existing CA-compare coverage (grep for `ca_hash_compares` or `ContentLookup`):

```rust
// r[verify sched.ca.cutoff-compare]
#[tokio::test]
async fn ca_compare_short_circuits_on_first_miss() {
    // 4-output CA completion, mock store: output[0] → miss,
    // outputs[1..3] would match but shouldn't be queried.
    // Assert: CountingRecorder shows exactly 1 ×"miss" + 3 ×"skipped_after_miss"
    //         and MockStore.content_lookup_calls == 1 (not 4).
}

#[tokio::test]
async fn ca_compare_skipped_when_breaker_open() {
    // Trip the breaker via record_failure×5 BEFORE the completion.
    // 3-output CA completion. Assert: 0 ContentLookup calls,
    // state.ca_output_unchanged == false (safe fallback),
    // CountingRecorder shows 0 ×"match"/"miss" (loop never ran).
}

#[tokio::test]
async fn ca_compare_failures_feed_breaker() {
    // Mock store: all ContentLookup → Err(Unavailable).
    // 5 completions × 1-output each. Assert: after 5 failures,
    // breaker.is_open() == true (5 is the default threshold per
    // breaker.rs — re-check at dispatch).
}
```

If T3 lands, add a fourth test asserting `content_lookup_batch` receives all hashes in one call.

## Exit criteria

- `cargo nextest run -p rio-scheduler ca_compare_short_circuits ca_compare_skipped_when_breaker ca_compare_failures_feed_breaker` → 3 passed
- `grep 'CONTENT_LOOKUP_TIMEOUT\|from_secs(2)' rio-scheduler/src/actor/completion.rs` → ≥1 hit (short timeout present)
- `grep 'cache_breaker.is_open\|cache_breaker.record_failure' rio-scheduler/src/actor/completion.rs` → ≥2 hits (breaker consulted + fed)
- `grep 'break;' rio-scheduler/src/actor/completion.rs` → ≥1 hit in the CA-compare loop body (short-circuit present)
- `grep 'skipped_after_miss' rio-scheduler/src/actor/completion.rs docs/src/observability.md` → ≥2 hits (counter label in code + obs.md table)
- `grep 'pub(super) fn is_open' rio-scheduler/src/actor/breaker.rs` → 1 hit (visibility widened)
- T4 mutation: remove the `break;` at the short-circuit → `ca_compare_short_circuits_on_first_miss` fails on `content_lookup_calls == 4` (not 1)
- T3 conditional: IF landed, `grep 'ContentLookupBatch' rio-proto/proto/types.proto rio-store/src/grpc/mod.rs` → ≥2 hits
- `/nbr .#ci` green

## Tracey

References existing markers:
- `r[sched.ca.cutoff-compare]` — T1+T2 refine the implementation at the existing `r[impl]` annotation (if present at `completion.rs:~290` — check at dispatch; P0251 T1 sketch at `:21` has it); T4 adds `r[verify]` sites
- `r[obs.metric.scheduler]` — T1 adds `skipped_after_miss` label to `rio_scheduler_ca_hash_compares_total`; doc-table row updated per [P0295](plan-0295-doc-rot-batch-sweep.md)-T90

No new markers. Breaker integration is an implementation detail of the existing compare spec; `r[sched.ca.cutoff-compare]` already says "best-effort: a store outage degrades to no cutoff".

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T1+T2: :289-337 — breaker-gate + short-circuit-on-miss + 2s timeout + record_failure/success"},
  {"path": "rio-scheduler/src/actor/breaker.rs", "action": "MODIFY", "note": "T1: :95 fn is_open → pub(super) fn is_open"},
  {"path": "rio-scheduler/src/actor/tests/completion.rs", "action": "MODIFY", "note": "T4: +3 tests (short-circuit, breaker-skip, failures-feed-breaker)"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T1: Scheduler Metrics table — rio_scheduler_ca_hash_compares_total +skipped_after_miss label"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "T3 CONDITIONAL: +ContentLookupBatchRequest/Response"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "T3 CONDITIONAL: content_lookup_batch handler — SELECT ... WHERE nar_hash = ANY($1)"}
]
```

```
rio-scheduler/src/actor/
├── completion.rs         # T1+T2: CA-compare loop refactor
├── breaker.rs            # T1: is_open pub(super)
└── tests/completion.rs   # T4: 3 tests
docs/src/observability.md # T1: +label
```

## Dependencies

```json deps
{"deps": [251], "soft_deps": [252, 295, 311, 376], "note": "discovered_from=251 (rev-p251 perf). P0251 (DONE) shipped the CA-compare hook this plan optimizes. Soft-dep P0252 (CA cutoff-propagate — consumes ca_output_unchanged; T1+T2 don't change the semantics, just the latency; P0252's find_cutoff_eligible sees identical results faster). Soft-dep P0295-T91 (adds rio_scheduler_ca_hash_compares_total to obs.md Scheduler Metrics table — T1 here adds the skipped_after_miss label to the same row; T1 may land first, T90 then adds the row with the label already present, or T90 lands first and T1 adds the label; coordinate at dispatch). Soft-dep P0311-T61 (test-gap for zero-outputs/malformed/Err/Elapsed paths of the SAME hook — T4 here covers breaker+short-circuit, P0311-T61 covers edge-input paths; both add tests to actor/tests/completion.rs, additive). Soft-dep P0376 (types.proto split — T3's ContentLookupBatch proto msg may land in scheduler.proto post-split; check at dispatch). completion.rs count=25 (HOT) — T1+T2 are localized inside the existing if-block at :289-337; no signature change. breaker.rs is low-traffic (visibility widen only). observability.md count=21 — additive label-mention in an existing table row."}
```

**Depends on:** [P0251](plan-0251-ca-cutoff-compare.md) — CA-compare hook exists at `completion.rs:289-337`.

**Conflicts with:** [`completion.rs`](../../rio-scheduler/src/actor/completion.rs) count=25 — [P0304](plan-0304-trivial-batch-p0222-harness.md)-T34 adds `emit_progress` call at `:528` (different section). [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T57 adds tests to `tests/completion.rs` for zero-outputs/malformed/Err/Elapsed; T4 here adds breaker+short-circuit tests; both additive. [`types.proto`](../../rio-proto/proto/types.proto) — T3 is conditional and additive (new messages); if [P0376](plan-0376-types-proto-domain-split.md) split lands first, target `rio-proto/proto/store.proto` instead.
