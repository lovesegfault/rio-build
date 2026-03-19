# Plan 994607701: Extract metrics_registered test bodies + build.rs grep — finish P0330's job

Consolidator finding (mc-31 window). [P0328](plan-0328-metrics-registered-bidirectional.md) fanned out the bidirectional emit↔describe check to 5 crates, producing **578 lines** across [`rio-{controller,gateway,scheduler,store,worker}/tests/metrics_registered.rs`](../../rio-scheduler/tests/metrics_registered.rs). [P0330](plan-0330-test-recorder-extraction-test-support.md) then extracted the `DescribedNames` recorder into [`rio-test-support/src/metrics.rs`](../../rio-test-support/src/metrics.rs) — but stopped there. The **test bodies** and the **build-script grep** remain duplicated.

**P0328's own rationale at [`:136-138`](plan-0328-metrics-registered-bidirectional.md) is now STALE.** It says "Duplicate (5× ~40 lines each). Chosen. […] Put in `rio-test-support`. Rejected: `rio-test-support` is for shared runtime test infrastructure, not build scripts." P0330 already disproved the rejection premise — `DescribedNames` landed in `rio-test-support` and nothing broke. The remaining duplication is **~380 lines**: the two test functions are near-identical across crates (only the `SPEC_METRICS` const and the `emitted.len() >= N` threshold differ), and 4 of the 5 crate-level `build.rs` files are **verbatim copies** of the stripped version ([`rio-controller/build.rs:3`](../../rio-controller/build.rs): "See rio-scheduler/build.rs for full rationale — this is a verbatim copy").

**Sequence BEFORE [P0321](plan-0321-build-graph-edges-histogram-buckets.md).** P0321 is frontier-eligible and its T3 references [`metrics_registered.rs:93`](../../rio-scheduler/tests/metrics_registered.rs) by line — that ref goes stale the moment anything above it moves. P0321 also wants a **6th crate arm** (its `HISTOGRAM_BUCKET_MAP` check is a new test shape — "every `describe_histogram!` has a bucket config" — that's the "type-aware variant" the consolidator flagged). Landing the extraction first means P0321 adds one helper call instead of a 6th 100-line file.

## Entry criteria

- [P0328](plan-0328-metrics-registered-bidirectional.md) merged — DONE. All 5 `metrics_registered.rs` files exist.
- [P0330](plan-0330-test-recorder-extraction-test-support.md) merged — DONE. `rio-test-support/src/metrics.rs` exists with `DescribedNames`.

## Tasks

### T1 — `feat(test-support):` extract `assert_spec_metrics_described` + `assert_emitted_metrics_described`

MODIFY [`rio-test-support/src/metrics.rs`](../../rio-test-support/src/metrics.rs) — append after the `CountingRecorder` block (after `:171`):

```rust
// ===========================================================================
// Assertion helpers — extracted from 5× metrics_registered.rs test bodies
// ===========================================================================

/// Assert that every name in `spec_metrics` appears in the set of
/// `describe_*!` calls fired by `describe_fn`.
///
/// Spec→describe direction: catches "spec'd in observability.md but
/// the `describe_metrics()` fn forgot to mention it" — the metric
/// scrapes with no `# HELP` line, Grafana tooltips empty.
///
/// `describe_fn` is the crate's `pub fn describe_metrics()` — passed
/// as a fn pointer so this helper stays crate-agnostic. `crate_name`
/// is for the error message only.
pub fn assert_spec_metrics_described(
    spec_metrics: &[&str],
    describe_fn: fn(),
    crate_name: &str,
) {
    let recorder = DescribedNames::default();
    metrics::with_local_recorder(&recorder, describe_fn);
    let described = recorder.names();

    let missing: Vec<_> = spec_metrics
        .iter()
        .filter(|name| !described.contains(&(**name).to_string()))
        .collect();

    assert!(
        missing.is_empty(),
        "spec'd metrics missing from {crate_name}::describe_metrics(): {missing:?}\n\
         \n\
         described:\n{described:#?}"
    );
}

/// Assert that every name in `emitted_metrics` (one per line — the
/// `include_str!(OUT_DIR/emitted_metrics.txt)` output) appears in the
/// set of `describe_*!` calls fired by `describe_fn`.
///
/// Emit→describe direction: catches "someone added
/// `metrics::counter!("new_thing")` deep in a handler but forgot both
/// the `describe_*!` AND the observability.md row" — P0214's
/// `rio_scheduler_build_timeouts_total` did exactly this and sailed
/// through the spec→describe check (which only knows what's IN the
/// spec list).
///
/// `min_emitted` is a precondition self-check: if the build-script
/// grep returns near-zero, either the crate genuinely has no metrics
/// (implausible for any crate large enough to need this check) or the
/// regex broke (e.g., someone imported the macros unqualified). Fail
/// loudly instead of passing vacuously. Pick `min_emitted` at ~75% of
/// the crate's current count so normal churn doesn't trip it but a
/// broken regex does.
pub fn assert_emitted_metrics_described(
    emitted_metrics: &str,
    min_emitted: usize,
    describe_fn: fn(),
    crate_name: &str,
) {
    let emitted: Vec<&str> = emitted_metrics.lines().filter(|l| !l.is_empty()).collect();

    assert!(
        emitted.len() >= min_emitted,
        "EMITTED_METRICS has only {} entries (threshold {min_emitted}) — \
         build-script grep likely broke (check build.rs regex vs. src/ \
         macro call style)",
        emitted.len()
    );

    let recorder = DescribedNames::default();
    metrics::with_local_recorder(&recorder, describe_fn);
    let described = recorder.names();

    let undescribed: Vec<_> = emitted
        .iter()
        .filter(|name| !described.contains(&(**name).to_string()))
        .collect();

    assert!(
        undescribed.is_empty(),
        "metrics emitted in {crate_name}/src/ but NOT in describe_metrics():\n  {undescribed:#?}\n\
         \n\
         Add describe_counter!/describe_gauge!/describe_histogram! to \
         {crate_name}/src/lib.rs::describe_metrics() AND a row to \
         docs/src/observability.md."
    );
}
```

Then each crate's `metrics_registered.rs` collapses to:

```rust
use rio_test_support::metrics::{assert_spec_metrics_described, assert_emitted_metrics_described};

const SCHEDULER_METRICS: &[&str] = &[ /* ... crate-specific, unchanged ... */ ];
const EMITTED_METRICS: &str = include_str!(concat!(env!("OUT_DIR"), "/emitted_metrics.txt"));

// r[verify obs.metric.scheduler]
#[test]
fn all_spec_metrics_have_describe_call() {
    assert_spec_metrics_described(SCHEDULER_METRICS, rio_scheduler::describe_metrics, "rio-scheduler");
}

// r[verify obs.metric.scheduler]
#[test]
fn all_emitted_metrics_are_described() {
    assert_emitted_metrics_described(EMITTED_METRICS, 30, rio_scheduler::describe_metrics, "rio-scheduler");
}
```

The scheduler file goes from 146 → ~55 lines (the `SCHEDULER_METRICS` const is 38 of those). Controller/gateway/store/worker similarly.

**Keep the file-level `//!` doc-comment in the scheduler copy only** (it's the canonical one with the full "Why a custom recorder" rationale at `:10-16`). The other 4 crates point to it: `//! See rio-scheduler/tests/metrics_registered.rs for rationale.` — same convention the `build.rs` copies already use.

### T2 — `refactor(test-support):` extract build-script grep → `rio-test-support/src/metrics_grep.rs`

The 4× 41-line `build.rs` copies ([`rio-controller/build.rs`](../../rio-controller/build.rs), [`rio-gateway/build.rs`](../../rio-gateway/build.rs), [`rio-store/build.rs`](../../rio-store/build.rs), [`rio-worker/build.rs`](../../rio-worker/build.rs)) plus the 64-line canonical ([`rio-scheduler/build.rs`](../../rio-scheduler/build.rs)) are functionally identical — same regex, same `grep_metrics()` recursion, same output path. Only the doc-comments differ.

**P0328's objection was correct in one respect:** build scripts can't depend on workspace crates cleanly via `[build-dependencies]` when the crate is `publish = false` and in the same workspace. But `rio-test-support` doesn't need to be a `build-dependency` — the grep function can be a **plain `.rs` file that build scripts `include!()`**:

NEW [`rio-test-support/src/metrics_grep.rs`](../../rio-test-support/src/metrics_grep.rs) — the `grep_metrics()` function + `emit_grep_output()` wrapper, NOT behind `pub mod` (this file is `include!()`-only, not a compiled module):

```rust
// This file is `include!()`-ed by crate-level build.rs files, NOT
// compiled as a rio-test-support module. See rio-scheduler/build.rs
// for full rationale on why this is a build-time source grep rather
// than a runtime recorder hook.
//
// The `#[allow(dead_code)]` on the file guard below prevents clippy
// complaining when rio-test-support itself is built (the include!
// consumers are in OTHER crates' build scripts).

#[allow(dead_code)]
fn emit_metrics_grep(manifest_dir: &str, out_dir: &str) {
    use std::{collections::BTreeSet, fs, path::Path};

    let src = Path::new(manifest_dir).join("src");
    let mut names = BTreeSet::new();

    // `\bmetrics::` required (matches this codebase's convention; avoids
    // matching `describe_counter!`); `[a-z0-9_]+` (digits for s3 names);
    // `\s*` handles multi-line (rustfmt breaks after the paren).
    let re = regex::Regex::new(
        r#"\bmetrics::(?:counter|gauge|histogram)!\s*\(\s*"([a-z0-9_]+)""#
    ).unwrap();

    fn walk(dir: &Path, re: &regex::Regex, out: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, re, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                for cap in re.captures_iter(&text) {
                    out.insert(cap[1].to_string());
                }
            }
        }
    }
    walk(&src, &re, &mut names);

    fs::write(
        Path::new(out_dir).join("emitted_metrics.txt"),
        names.into_iter().collect::<Vec<_>>().join("\n"),
    ).unwrap();

    println!("cargo:rerun-if-changed=src");
}
```

Each crate's `build.rs` becomes:

```rust
//! See rio-scheduler/build.rs rationale comment + rio-test-support/src/metrics_grep.rs

include!("../rio-test-support/src/metrics_grep.rs");

fn main() {
    emit_metrics_grep(env!("CARGO_MANIFEST_DIR"), &std::env::var("OUT_DIR").unwrap());
}
```

**Keep [`rio-scheduler/build.rs`](../../rio-scheduler/build.rs)'s full rationale doc-comment** (the "Why a build script and not a runtime recorder hook" block at `:8-15`) — move it above the `include!()`. Scheduler stays canonical; the other 4 shrink to ~8 lines each.

**Cargo.toml impact:** each crate's `[build-dependencies]` already has `regex` (for the current inline grep). `include!()` doesn't add any dependency — the `regex` import happens at the `include!` expansion site. No Cargo.toml changes.

### T3 — `refactor:` migrate 5 crates to use T1+T2 helpers

MODIFY all five:

| File | Before | After | Delta |
|---|---|---|---|
| [`rio-scheduler/tests/metrics_registered.rs`](../../rio-scheduler/tests/metrics_registered.rs) | 146L | ~55L | -91 |
| [`rio-store/tests/metrics_registered.rs`](../../rio-store/tests/metrics_registered.rs) | 118L | ~40L | -78 |
| [`rio-worker/tests/metrics_registered.rs`](../../rio-worker/tests/metrics_registered.rs) | 111L | ~35L | -76 |
| [`rio-gateway/tests/metrics_registered.rs`](../../rio-gateway/tests/metrics_registered.rs) | 103L | ~30L | -73 |
| [`rio-controller/tests/metrics_registered.rs`](../../rio-controller/tests/metrics_registered.rs) | 100L | ~25L | -75 |
| [`rio-scheduler/build.rs`](../../rio-scheduler/build.rs) | 64L | ~20L | -44 |
| [`rio-controller/build.rs`](../../rio-controller/build.rs) | 41L | ~8L | -33 |
| [`rio-gateway/build.rs`](../../rio-gateway/build.rs) | 41L | ~8L | -33 |
| [`rio-store/build.rs`](../../rio-store/build.rs) | 41L | ~8L | -33 |
| [`rio-worker/build.rs`](../../rio-worker/build.rs) | 41L | ~8L | -33 |

Net: ~-570 lines, +~90 in rio-test-support. **The `r[verify obs.metric.*]` annotations stay in each crate's test file** — they're crate-specific markers, and [P0304](plan-0304-trivial-batch-p0222-harness.md) T31 is adding 4 of these crates to tracey's `test_include` glob specifically to see them.

**Preserve exact test function names** (`all_spec_metrics_have_describe_call`, `all_emitted_metrics_are_described`) — [P0328](plan-0328-metrics-registered-bidirectional.md)'s exit criterion at `:149` references them by name for `cargo nextest run -p <crate> all_emitted_metrics_are_described`.

### T4 — `docs(plan):` fix P0328:136-138 stale rationale + P0321:89 line-ref

MODIFY [`.claude/work/plan-0328-metrics-registered-bidirectional.md`](plan-0328-metrics-registered-bidirectional.md) at `:136-138` — the "DRY consideration" block. P0328 is DONE so this is **archaeology only** (status=DONE docs are reference, not dispatch), but the stale prose will confuse anyone tracing the decision:

> **Historical note (P994607701 closed this):** The duplication below was the P0328-time decision. [P0330](plan-0330-test-recorder-extraction-test-support.md) extracted `DescribedNames`; [P994607701](plan-994607701-metrics-harness-extract-test-bodies.md) extracted the test bodies + build-script grep. The "rejected" bullet's premise proved wrong — `include!()` sidesteps the build-dependency issue.

MODIFY [`.claude/work/plan-0321-build-graph-edges-histogram-buckets.md`](plan-0321-build-graph-edges-histogram-buckets.md) at `:89` — it references `metrics_registered.rs:93`, which shifts post-T3. Two options at dispatch:

- If P0321 has NOT dispatched: update `:89` to reference the new line (post-T3 it'll be ~line 20). Add a note that P0321-T3's "type-aware" histogram check should land as a THIRD helper in `rio-test-support/src/metrics.rs` (`assert_histograms_have_buckets(spec_metrics, bucket_map, exempt)`) rather than inline in the scheduler file.
- If P0321 HAS dispatched (check `onibus dag render | grep 321` at dispatch): skip the P0321 edit, leave a `TODO(P0321)` comment in this plan's impl commit message.

## Exit criteria

- `/nbr .#ci` green
- `wc -l rio-*/tests/metrics_registered.rs | tail -1` — total ≤ 200 (was 578; ~65% reduction)
- `wc -l rio-{controller,gateway,store,worker}/build.rs` — each ≤ 15 lines (was 41 each)
- `grep 'assert_spec_metrics_described\|assert_emitted_metrics_described' rio-test-support/src/metrics.rs` → 2 hits (T1: both helpers defined)
- `grep -c 'assert_.*_metrics_described' rio-*/tests/metrics_registered.rs` — sum ≥ 10 (T3: 2 per crate × 5 crates)
- **Mutation check:** delete one entry from `rio-scheduler/src/lib.rs` `describe_metrics()` body (e.g., comment out `describe_counter!("rio_scheduler_builds_total", ...)`) → `cargo nextest run -p rio-scheduler metrics_registered` FAILS with `rio_scheduler_builds_total` in the failure message. Then revert. If the helper extraction broke the assertion, this catches it.
- `grep 'include!' rio-controller/build.rs rio-gateway/build.rs rio-store/build.rs rio-worker/build.rs rio-scheduler/build.rs` → 5 hits (T2: all use include!)
- `test -f rio-test-support/src/metrics_grep.rs` (T2: file exists)
- `grep 'pub mod metrics_grep' rio-test-support/src/lib.rs` → 0 hits (T2: the file is `include!`-only, NOT a module — adding it as a module would make clippy warn on dead code)
- `nix develop -c tracey query rule obs.metric.scheduler` → verify site count unchanged (T3 preserves the `r[verify ...]` annotations; [P0304](plan-0304-trivial-batch-p0222-harness.md) T31 must have landed for this to show >0 — check `onibus dag render | grep 304` at dispatch)

## Tracey

No new markers. No marker changes. The five `r[verify obs.metric.{scheduler,gateway,store,worker,controller}]` annotations at each crate's `metrics_registered.rs` stay exactly where they are — T3 preserves them. The extraction is mechanical; the spec contract is unchanged.

References existing markers (preserved by T3, not implemented by this plan):
- `r[obs.metric.scheduler]` — [`observability.md:79`](../../docs/src/observability.md)
- `r[obs.metric.gateway]` — [`observability.md:63`](../../docs/src/observability.md)
- `r[obs.metric.store]` — [`observability.md:126`](../../docs/src/observability.md)
- `r[obs.metric.worker]` — [`observability.md:150`](../../docs/src/observability.md)
- `r[obs.metric.controller]` — [`observability.md:183`](../../docs/src/observability.md)

## Files

```json files
[
  {"path": "rio-test-support/src/metrics.rs", "action": "MODIFY", "note": "T1: +assert_spec_metrics_described +assert_emitted_metrics_described after :171"},
  {"path": "rio-test-support/src/metrics_grep.rs", "action": "NEW", "note": "T2: include!()-only emit_metrics_grep() — NOT a pub mod"},
  {"path": "rio-scheduler/tests/metrics_registered.rs", "action": "MODIFY", "note": "T3: collapse to helper calls; keep full //! rationale + SCHEDULER_METRICS + r[verify]"},
  {"path": "rio-controller/tests/metrics_registered.rs", "action": "MODIFY", "note": "T3: collapse to helper calls"},
  {"path": "rio-gateway/tests/metrics_registered.rs", "action": "MODIFY", "note": "T3: collapse to helper calls"},
  {"path": "rio-store/tests/metrics_registered.rs", "action": "MODIFY", "note": "T3: collapse to helper calls"},
  {"path": "rio-worker/tests/metrics_registered.rs", "action": "MODIFY", "note": "T3: collapse to helper calls"},
  {"path": "rio-scheduler/build.rs", "action": "MODIFY", "note": "T2/T3: include!() + keep rationale doc-comment (canonical)"},
  {"path": "rio-controller/build.rs", "action": "MODIFY", "note": "T2/T3: include!() — 41→~8 lines"},
  {"path": "rio-gateway/build.rs", "action": "MODIFY", "note": "T2/T3: include!() — 41→~8 lines"},
  {"path": "rio-store/build.rs", "action": "MODIFY", "note": "T2/T3: include!() — 41→~8 lines"},
  {"path": "rio-worker/build.rs", "action": "MODIFY", "note": "T2/T3: include!() — 41→~8 lines"},
  {"path": ".claude/work/plan-0328-metrics-registered-bidirectional.md", "action": "MODIFY", "note": "T4: historical note at :136-138 — stale DRY rationale"},
  {"path": ".claude/work/plan-0321-build-graph-edges-histogram-buckets.md", "action": "MODIFY", "note": "T4: update :89 line-ref + note re type-aware helper (conditional on P0321 dispatch state)"}
]
```

```
rio-test-support/src/
├── metrics.rs               # T1: +2 assertion helpers (~90L)
└── metrics_grep.rs          # T2: NEW — include!()-only grep fn
rio-scheduler/
├── tests/metrics_registered.rs   # T3: 146→~55L
└── build.rs                      # T3: 64→~20L (keeps rationale)
rio-{controller,gateway,store,worker}/
├── tests/metrics_registered.rs   # T3: ~105→~30L each
└── build.rs                      # T3: 41→~8L each
.claude/work/
├── plan-0328-...            # T4: historical note
└── plan-0321-...            # T4: line-ref fix (conditional)
```

## Dependencies

```json deps
{"deps": [328, 330], "soft_deps": [321, 304], "note": "SEQUENCE BEFORE P0321 (frontier-eligible — its :89 ref goes stale, and its T3 type-aware histogram check should land as a 3rd helper in test-support, not inline). P0328+P0330 both DONE (files exist). Soft-dep P0304 T31: if it lands first, the tracey test_include globs pick up the r[verify] annotations at the new line numbers automatically; if this lands first, P0304 T31's file:line table at :796-802 goes stale (4 of 7 rows reference metrics_registered.rs by line). Check 304 status at dispatch. discovered_from=consolidator mc-31 window."}
```

**Depends on:** [P0328](plan-0328-metrics-registered-bidirectional.md) (DONE) — 5 files exist. [P0330](plan-0330-test-recorder-extraction-test-support.md) (DONE) — `rio-test-support/src/metrics.rs` exists.

**Sequence before:** [P0321](plan-0321-build-graph-edges-histogram-buckets.md) — frontier-eligible, references stale `:89`, wants 6th crate arm.

**Conflicts with:** None of the 5 `metrics_registered.rs` files appear in `onibus collisions top 30`. The crate-level `build.rs` files are low-traffic (last touched by P0328). `rio-test-support/src/metrics.rs` was created by P0330 (DONE) — no UNIMPL plan touches it. P0304 T31 references these files in a **documentation table** (not an edit), so textual conflict is impossible; semantic conflict is the line-number staleness noted above.
