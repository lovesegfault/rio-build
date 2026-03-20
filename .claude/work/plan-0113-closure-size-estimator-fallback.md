# Plan 0113: Closure-size-as-proxy estimator fallback

## Design

The scheduler's resource estimator uses `build_history` EMA for known derivations. Unknown derivations (first build of a new package) fell back to a hardcoded default — wrong for anything nontrivial. Two commits added a closure-size proxy: if no history exists, estimate memory/duration from the sum of input NAR sizes.

`e9c313e` added `DerivationNode.input_srcs_nar_size` to the proto — a `u64` populated by the gateway during DAG reconstruction. The gateway's `translate.rs` grew a batch lookup: after parsing all `.drv` ATerm, issue one `QueryPathInfo` batch for all input sources, sum `nar_size` per derivation. Batching because a 500-node DAG × per-path round-trip would be 500 sequential gRPC calls; batch is one.

`f343eb9` wired the fallback in `estimator.rs`. The fallback chain: EMA history → closure-size DURATION proxy → hardcoded default. The proxy uses `CLOSURE_BYTES_PER_SEC_PROXY` (~10MB/s for Rust/C++ builds) and `CLOSURE_PROXY_MIN_SECS` (5s floor) to estimate duration from input NAR size. Memory estimation remains the hardcoded default — the shipped proxy is duration-only. Both constants documented as rough heuristics — they'll be wrong by 10× for pathological builds, but "closure size correlates with build cost" is better than "everything takes 60s."

`state/derivation.rs` threads `input_srcs_nar_size` through `DerivationState`; `critical_path.rs` uses it in path-length estimation for unknown derivations.

## Files

```json files
[
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "DerivationNode.input_srcs_nar_size u64 field"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "batch QueryPathInfo for all input sources; sum nar_size per derivation"},
  {"path": "rio-scheduler/src/estimator.rs", "action": "MODIFY", "note": "fallback chain: EMA → closure-size DURATION proxy → hardcoded; CLOSURE_BYTES_PER_SEC_PROXY, CLOSURE_PROXY_MIN_SECS constants"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "input_srcs_nar_size threaded through DerivationState"},
  {"path": "rio-scheduler/src/critical_path.rs", "action": "MODIFY", "note": "closure-size in path-length estimation for unknown drvs"},
  {"path": "rio-test-support/src/fixtures.rs", "action": "MODIFY", "note": "test builders grow input_srcs_nar_size param"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `r[sched.estimate.fallback-chain]`, `r[sched.estimate.closure-proxy]`. P0127 later fixed the docs for this (`b10a1f0` — "estimation fallback" in `scheduler.md` was stale).

## Entry

- Depends on P0096: phase 2c complete — `estimator.rs` and `build_history` EMA existed.
- Depends on P0103: `test_store_path()` helper used in the new tests.

## Exit

Merged as `e9c313e..f343eb9` (2 commits). `.#ci` green at merge. Estimator tests for each fallback tier (EMA hit, closure proxy, hardcoded). Gateway `translate.rs` test: batch populate produces correct per-derivation sums.
