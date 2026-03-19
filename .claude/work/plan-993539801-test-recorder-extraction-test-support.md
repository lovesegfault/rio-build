# Plan 993539801: Extract metrics::Recorder test impls → rio-test-support

Consolidator finding (md5=9a596cb2, mc-31 window). The `metrics::Recorder` trait has two test-only implementations — `DescribedNames` and `CountingRecorder` — that have been copy-pasted into multiple crates with **drift already setting in**. Five byte-identical `DescribedNames` copies at 26L each; three `CountingRecorder` copies with method-name drift (`keys()` vs `all_keys()`, `key()` vs `counter_key()`) and feature-subset divergence (gauge tracking present in one, stripped from the other two).

The **existing breadcrumbs know this**:
- [`helpers.rs:338-339`](../../rio-scheduler/src/actor/tests/helpers.rs) — "Mirrors `rio-gateway/tests/ssh_hardening.rs` — a follow-up could lift both into `rio-test-support`."
- [`gc_schedule.rs:229`](../../rio-controller/src/reconcilers/gc_schedule.rs) — "Mirrors `rio-scheduler/src/actor/tests/helpers.rs`." ([P0212](plan-0212-gc-automation-cron.md) added this copy.)

The consolidator counted **2× CountingRecorder** but `grep -rn CountingRecorder rio-*/` finds **3×** — [`ssh_hardening.rs:87`](../../rio-gateway/tests/ssh_hardening.rs) is the original that helpers.rs mirrored. All three migrate.

**Sequence before [P0328](plan-0328-metrics-registered-bidirectional.md).** P0328 T3 replicates `metrics_registered.rs` structure to 4 crates (it currently believes those files are `action: "NEW"` — they already exist, that's P0328's own drift). If P0328 lands first, it either adds a SIXTH `DescribedNames` copy (to its build.rs pattern) or does its own extraction, fragmenting this work. Landing the extraction first means P0328 imports from one place.

All five target crates already have `rio-test-support = { workspace = true }` in `[dev-dependencies]`. The extraction is a `pub mod metrics;` + one import-line per callsite.

## Tasks

### T1 — `feat(test-support):` new metrics module — superset CountingRecorder + DescribedNames

NEW [`rio-test-support/src/metrics.rs`](../../rio-test-support/src/metrics.rs). Extract the **superset** from [`helpers.rs:327-446`](../../rio-scheduler/src/actor/tests/helpers.rs) — it's the only copy with gauge tracking (`gauges: Mutex<HashSet<String>>`, `gauge_touched()`, `gauge_names()`).

```rust
//! Test-only `metrics::Recorder` implementations.
//!
//! Two recorders for two assertion shapes:
//!
//! - [`DescribedNames`] — captures `describe_*!` macro calls. For
//!   "every spec'd metric has a describe call" checks (the
//!   `metrics_registered.rs` pattern). `register_*` return noop.
//!
//! - [`CountingRecorder`] — captures `counter!().increment()` deltas
//!   keyed by `name{sorted,labels}`. For "this code path fired this
//!   metric" behavioral assertions. Gauge touch-set for absence checks.
//!
//! Both pair with `metrics::with_local_recorder` (sync closure) or
//! `metrics::set_default_local_recorder` (guard-scoped, visible across
//! `.await` on a current-thread tokio runtime — `#[tokio::test]` default).
//!
//! Extracted from 5× byte-identical DescribedNames copies
//! (rio-{controller,gateway,scheduler,store,worker}/tests/metrics_registered.rs)
//! and 3× drifting CountingRecorder copies (scheduler/src/actor/tests/helpers.rs
//! canonical; controller/src/reconcilers/gc_schedule.rs + gateway/tests/ssh_hardening.rs
//! stripped subsets). P0212 left the breadcrumb at gc_schedule.rs:229.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};

// ... DescribedNames — lift from scheduler/tests/metrics_registered.rs:65-90 verbatim,
//     make pub, add .names() accessor (currently `.0.clone()` then `.lock()` — awkward).

// ... CountingRecorder — lift from helpers.rs:327-446 verbatim, s/pub(crate)/pub/.
//     Keep the doc comment about set_default_local_recorder + current-thread runtime.
//     Keep gauge tracking (controller copy doesn't have it; scheduler tests USE it
//     via gauge_touched() — grep confirms: actor/tests/worker.rs).
```

Method-name canonicalization — use the `helpers.rs` names:

| Canonical (helpers.rs) | Controller drift (gc_schedule.rs) | Gateway (ssh_hardening.rs) |
|---|---|---|
| `counter_key()` | `key()` | `counter_key()` |
| `get()` | `get()` | `get()` |
| `all_keys()` | `keys()` | *(missing)* |
| `gauge_touched()` | *(missing)* | *(missing)* |
| `gauge_names()` | *(missing)* | *(missing)* |

The `DescribedNames.0` public-field access pattern (`recorder.0.clone()`) is awkward — add a `pub fn names(&self) -> Vec<String>` accessor. Callsites can migrate in T3 or keep `.0.clone()` if the field stays `pub` (it should — existing tests rely on it).

MODIFY [`rio-test-support/src/lib.rs`](../../rio-test-support/src/lib.rs) — add `pub mod metrics;` after `:13`. Update the module list in the `//!` header at `:3-7`.

MODIFY [`rio-test-support/Cargo.toml`](../../rio-test-support/Cargo.toml) — add `metrics = { workspace = true }` under `[dependencies]`. Grep confirms it's currently absent.

### T2 — `refactor(scheduler):` migrate helpers.rs canonical → import

MODIFY [`rio-scheduler/src/actor/tests/helpers.rs`](../../rio-scheduler/src/actor/tests/helpers.rs). **Delete** the `CountingRecorder` block at `:323-446` (banner comment through `impl Recorder`). Replace with:

```rust
// Re-export for test modules. Canonical impl moved to rio-test-support
// (P993539801) — was 3× copied with method-name drift before that.
pub(crate) use rio_test_support::metrics::CountingRecorder;
```

Callsites in `actor/tests/{misc,worker}.rs` already import from `helpers` — the re-export keeps them working unchanged. Check at dispatch whether `pub(crate) use` through a `mod tests` boundary needs adjustment; worst case, callsites switch to `use rio_test_support::metrics::CountingRecorder` directly (also fine — fewer indirections).

### T3 — `refactor(gateway,controller):` migrate stripped copies → import

MODIFY [`rio-controller/src/reconcilers/gc_schedule.rs`](../../rio-controller/src/reconcilers/gc_schedule.rs). **Delete** `:226-308` (the `CountingRecorder` + `impl` inside `mod tests`). Add at the top of `mod tests`:

```rust
use rio_test_support::metrics::CountingRecorder;
```

Method-name drift: this copy uses `.keys()` not `.all_keys()`. Grep for callsites (`:327`, `:387` construct; grep `recorder.keys()` for actual calls) and rename → `.all_keys()`. This is the drift catching real code — if anyone added gauge assertions here since [P0212](plan-0212-gc-automation-cron.md), they'd have needed to re-add gauge tracking. They didn't; the superset import gives it for free.

MODIFY [`rio-gateway/tests/ssh_hardening.rs`](../../rio-gateway/tests/ssh_hardening.rs). **Delete** `:80-137` (struct + both impls). Add import. This copy already uses `counter_key()` — no method-rename needed. No `all_keys()` call — it only uses `.get()`.

### T4 — `refactor:` migrate 5× DescribedNames → import

MODIFY all five `tests/metrics_registered.rs` files:

| File | DescribedNames block | import replaces |
|---|---|---|
| [`rio-scheduler/tests/metrics_registered.rs`](../../rio-scheduler/tests/metrics_registered.rs) | `:65-90` | `use rio_test_support::metrics::DescribedNames;` |
| [`rio-controller/tests/metrics_registered.rs`](../../rio-controller/tests/metrics_registered.rs) | `:37-` (26L) | same |
| [`rio-gateway/tests/metrics_registered.rs`](../../rio-gateway/tests/metrics_registered.rs) | ~26L | same |
| [`rio-store/tests/metrics_registered.rs`](../../rio-store/tests/metrics_registered.rs) | ~26L | same |
| [`rio-worker/tests/metrics_registered.rs`](../../rio-worker/tests/metrics_registered.rs) | ~26L | same |

The `use` lines at the top of each file (`use metrics::{Counter, Gauge, ...}`) become dead after the struct is deleted — clippy `--deny warnings` will force cleanup. Keep whatever the test body still imports (none — the test only uses `metrics::with_local_recorder` which is a free fn).

The `.0.clone()` field access at (e.g.) [`scheduler:96`](../../rio-scheduler/tests/metrics_registered.rs) stays working if T1 keeps the field `pub`. If T1 adds a `.names()` accessor, optionally migrate — not required.

### T5 — `docs:` note in P0328 that DescribedNames is now imported

MODIFY [`.claude/work/plan-0328-metrics-registered-bidirectional.md`](plan-0328-metrics-registered-bidirectional.md). P0328 T1's code block at `:40` says `let recorder = DescribedNames::default();` — correct, but the plan should know it's an import now, not a local struct. P0328 T3's Files fence says the four non-scheduler `metrics_registered.rs` files are `action: "NEW"` — they exist (this plan's T4 touches them). Add a one-line note to P0328's Entry criteria:

> - [P993539801](plan-993539801-test-recorder-extraction-test-support.md) merged — `DescribedNames` is `use rio_test_support::metrics::DescribedNames`, NOT a local struct. T3's `metrics_registered.rs` files already exist (as `MODIFY` not `NEW`).

## Exit criteria

- `/nbr .#ci` green
- `grep -rn 'struct DescribedNames\|struct CountingRecorder' rio-*/src rio-*/tests` → **exactly 2 hits, both in** `rio-test-support/src/metrics.rs` (no copies remain elsewhere)
- `grep -c 'use rio_test_support::metrics::' rio-*/tests/metrics_registered.rs rio-*/src/actor/tests/helpers.rs rio-*/src/reconcilers/gc_schedule.rs rio-*/tests/ssh_hardening.rs` → ≥7 hits (5 DescribedNames imports + scheduler helpers re-export + controller + gateway CountingRecorder imports; re-export may not match the pattern — accept ≥6)
- `cargo nextest run -p rio-scheduler test_misclass_detection_on_slow_completion` → pass (proves helpers.rs re-export works end-to-end; this test uses CountingRecorder via `logs_contain` NOT the recorder directly — **re-check at dispatch**; if that test doesn't exercise it, use `actor/tests/worker.rs:446` instead)
- `cargo nextest run -p rio-gateway test_handle_session_error_increments_metric` → pass (ssh_hardening.rs migration)
- `cargo nextest run -p rio-controller` — all gc_schedule tests pass (controller migration, including `.keys()` → `.all_keys()` rename)
- `cargo nextest run` — all 5 `all_spec_metrics_have_describe_call` tests pass (T4 migration)
- `grep 'metrics = ' rio-test-support/Cargo.toml` → 1 hit under `[dependencies]` (T1 dep add)
- Net LOC delta **negative** — extraction removes ~5×26 + 3×~80 = ~370L of duplication, adds ~150L to test-support. `git diff --stat` shows red > green.

## Tracey

No new markers. This is test-infrastructure consolidation — no spec behavior changes. The existing `r[verify obs.metric.{scheduler,gateway,store,worker,controller}]` annotations in the five `metrics_registered.rs` files stay (T4 deletes the struct, keeps the test fn and its annotation). [P0328](plan-0328-metrics-registered-bidirectional.md) will add `r[verify obs.metric.*]` to its new emit→describe tests; those import `DescribedNames` from here but the marker belongs on the assertion, not the recorder.

## Files

```json files
[
  {"path": "rio-test-support/src/metrics.rs", "action": "NEW", "note": "T1: DescribedNames + CountingRecorder — superset from helpers.rs (counters + gauges)"},
  {"path": "rio-test-support/src/lib.rs", "action": "MODIFY", "note": "T1: +pub mod metrics; update //! header module list"},
  {"path": "rio-test-support/Cargo.toml", "action": "MODIFY", "note": "T1: +metrics workspace dep under [dependencies]"},
  {"path": "rio-scheduler/src/actor/tests/helpers.rs", "action": "MODIFY", "note": "T2: delete :323-446 CountingRecorder block → pub(crate) use rio_test_support::metrics::CountingRecorder re-export"},
  {"path": "rio-controller/src/reconcilers/gc_schedule.rs", "action": "MODIFY", "note": "T3: delete :226-308 in mod tests → import; rename .keys() → .all_keys() at callsites"},
  {"path": "rio-gateway/tests/ssh_hardening.rs", "action": "MODIFY", "note": "T3: delete :80-137 → import (no method-rename needed)"},
  {"path": "rio-scheduler/tests/metrics_registered.rs", "action": "MODIFY", "note": "T4: delete :65-90 DescribedNames → import; drop dead use lines"},
  {"path": "rio-controller/tests/metrics_registered.rs", "action": "MODIFY", "note": "T4: delete DescribedNames block starting :37 → import"},
  {"path": "rio-gateway/tests/metrics_registered.rs", "action": "MODIFY", "note": "T4: delete DescribedNames block → import"},
  {"path": "rio-store/tests/metrics_registered.rs", "action": "MODIFY", "note": "T4: delete DescribedNames block → import"},
  {"path": "rio-worker/tests/metrics_registered.rs", "action": "MODIFY", "note": "T4: delete DescribedNames block → import"},
  {"path": ".claude/work/plan-0328-metrics-registered-bidirectional.md", "action": "MODIFY", "note": "T5: Entry-criteria note — DescribedNames is imported; metrics_registered.rs files are MODIFY not NEW"}
]
```

```
rio-test-support/
├── Cargo.toml                              # T1: +metrics dep
└── src/
    ├── lib.rs                              # T1: +pub mod metrics
    └── metrics.rs                          # T1: NEW — extracted superset
rio-scheduler/
├── src/actor/tests/helpers.rs              # T2: delete → re-export
└── tests/metrics_registered.rs             # T4: delete → import
rio-controller/
├── src/reconcilers/gc_schedule.rs          # T3: delete → import + .all_keys()
└── tests/metrics_registered.rs             # T4: delete → import
rio-gateway/tests/
├── ssh_hardening.rs                        # T3: delete → import
└── metrics_registered.rs                   # T4: delete → import
rio-store/tests/metrics_registered.rs       # T4: delete → import
rio-worker/tests/metrics_registered.rs      # T4: delete → import
.claude/work/plan-0328-*.md                 # T5: Entry-criteria note
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [328, 212], "note": "No hard deps — all source files exist, all five crates already have rio-test-support in dev-deps. Soft-dep P0328 in the INVERSE direction: P0328 should add this plan to ITS deps so it sequences AFTER (T5 here writes that breadcrumb). Coordinator prio=55 — same tier as P0328's 55, but P0328's deps=[214] already includes a DONE plan so both are frontier; collision-filter on the 5 metrics_registered.rs files will serialize them. 3/5 DescribedNames copies touched this window (consolidator observed during mc-31) — the extraction stops that drift axis permanently. discovered_from=consolidator (no plan number — cadence agent). P0212 (DONE) soft-reference: it left the gc_schedule.rs:229 breadcrumb that pointed here. The CountingRecorder THIRD copy at ssh_hardening.rs was not in the consolidator's count (said 2×) — found via helpers.rs:338 breadcrumb grep."}
```

**Depends on:** none — pure extraction.
**Sequence before:** [P0328](plan-0328-metrics-registered-bidirectional.md). P0328 T3 touches all 5 `metrics_registered.rs` files AND believes 4 of them are `action: "NEW"`. Landing this first means P0328 imports from one place instead of copying a sixth time or doing its own extraction. T5 writes the breadcrumb into P0328's Entry criteria.
**Conflicts with:** all 5 `metrics_registered.rs` files — no other UNIMPL plan touches them except P0328. [`gc_schedule.rs`](../../rio-controller/src/reconcilers/gc_schedule.rs) not in collisions top-30. [`helpers.rs`](../../rio-scheduler/src/actor/tests/helpers.rs) not in top-30. [`ssh_hardening.rs`](../../rio-gateway/tests/ssh_hardening.rs) not in top-30. Low conflict risk. [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T12 (to be appended in this same docs run) will ALSO use `CountingRecorder` in `completion.rs` tests — soft-dep: if T12 lands first, it uses the helpers.rs re-export (works either way); if this lands first, T12 imports from test-support directly.
