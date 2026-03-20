# Plan 0394: Derive `SPEC_METRICS` consts from observability.md at build time

[P0328](plan-0328-metrics-registered-bidirectional.md) established the bidirectional metrics check: `EMITTED_METRICS` (build.rs-grepped from `metrics::counter!(...)` sites) ⊆ `describe_*!(...)` set, and `SPEC_METRICS` (hand-maintained const per crate) ⊆ `describe_*!(...)` set. The second direction catches forgotten describes; the first catches stale spec-lists.

[P0367](plan-0367-normalized-name-post-impl-hardening.md) review found **three omissions** in [`GATEWAY_METRICS`](../../rio-gateway/tests/metrics_registered.rs) at `:6-15`:

1. `rio_gateway_auth_degraded_total` — P0367 added to [`observability.md:75`](../../docs/src/observability.md) + [`lib.rs:70`](../../rio-gateway/src/lib.rs), not to the const
2. `rio_gateway_quota_rejections_total` — pre-existing at `observability.md:74` + `lib.rs:76`, never in the const
3. `rio_gateway_jwt_mint_degraded_total` — at `lib.rs:66` only; missing from BOTH the obs.md table AND the const (doc-bug tracked at [P0295](plan-0295-doc-rot-batch-sweep.md)-T90)

Pattern: every metric addition requires three synchronized edits (obs.md table row, `describe_*!` call, `SPEC_METRICS` const entry). The const is the one developers forget — it's in a separate `tests/` file and the sibling tests still pass when a new metric is missing (the emit→describe check covers the new emit site; the spec→describe check stays blind).

`EMITTED_METRICS` is already build-time derived via [`metrics_grep.rs`](../../rio-test-support/src/metrics_grep.rs) (shared `include!()` into each crate's `build.rs` per [`rio-scheduler/build.rs:21`](../../rio-scheduler/build.rs)). Apply the same pattern: derive `SPEC_METRICS` from the observability.md table.

## Tasks

### T1 — `feat(test-support):` spec-metrics-grep helper for build scripts

MODIFY [`rio-test-support/src/metrics_grep.rs`](../../rio-test-support/src/metrics_grep.rs) — add a second function alongside `emit_metrics_grep`:

```rust
/// Grep the observability.md table for metric names with a given
/// prefix. Writes `spec_metrics.txt` (newline-separated) to OUT_DIR.
///
/// Table rows are `| rio_component_metric_name | Type | Desc |` —
/// extract column 1 where it starts with `prefix`. Blank-first-column
/// rows (like the r[obs.metric.transfer-volume] section header) skip.
#[allow(dead_code)]
fn emit_spec_metrics_grep(obs_md_path: &str, out_dir: &str, prefix: &str) {
    let obs_md = std::fs::read_to_string(obs_md_path)
        .unwrap_or_else(|e| panic!("read {obs_md_path}: {e}"));
    let mut names: Vec<&str> = obs_md
        .lines()
        .filter_map(|l| {
            // `| `rio_gateway_foo` | Counter | ...`  or  `| rio_gateway_foo | ...`
            let l = l.trim_start_matches('|').trim();
            let first = l.split('|').next()?.trim().trim_matches('`');
            (first.starts_with(prefix)
                && first.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
                .then_some(first)
        })
        .collect();
    names.sort();
    names.dedup();
    let out = format!("{out_dir}/spec_metrics.txt");
    std::fs::write(&out, names.join("\n"))
        .unwrap_or_else(|e| panic!("write {out}: {e}"));
    // Re-run if obs.md changes — CRITICAL for drift detection.
    println!("cargo:rerun-if-changed={obs_md_path}");
}
```

The `is_ascii_alphanumeric || '_'` filter excludes prose mentions (e.g., `rio_gateway_bytes_total{direction}` with a label example gets rejected by the `{` — trim the name before the first `{` if that's a problem; check at dispatch against the actual obs.md table format). Sort+dedup handles tables that repeat a metric name (unlikely but defensive).

### T2 — `feat(gateway):` build.rs — derive GATEWAY_METRICS

MODIFY [`rio-gateway/build.rs`](../../rio-gateway/build.rs) (or NEW if absent — scheduler has one at `build.rs:1-28`; gateway may not). Include the shared helper and call both:

```rust
include!("../rio-test-support/src/metrics_grep.rs");

fn main() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let out = std::env::var("OUT_DIR").unwrap();
    emit_metrics_grep(manifest, &out);
    emit_spec_metrics_grep(
        &format!("{manifest}/../docs/src/observability.md"),
        &out,
        "rio_gateway_",
    );
}
```

Relative path to `docs/src/observability.md` works in both cargo-workspace and crane builds (crane's src filter includes `docs/` — verify at dispatch with `nix eval .#packages.x86_64-linux.rio.src` and grep for observability.md).

### T3 — `refactor(gateway):` metrics_registered.rs — const → include_str

MODIFY [`rio-gateway/tests/metrics_registered.rs`](../../rio-gateway/tests/metrics_registered.rs) at `:6-15`. Replace the hand-maintained const:

```rust
/// Metric names from observability.md's Gateway Metrics table.
/// Derived at build time via build.rs → spec_metrics.txt.
const SPEC_METRICS_RAW: &str =
    include_str!(concat!(env!("OUT_DIR"), "/spec_metrics.txt"));

// r[verify obs.metric.gateway]
#[test]
fn all_spec_metrics_have_describe_call() {
    let spec_metrics: Vec<&str> = SPEC_METRICS_RAW.lines()
        .filter(|l| !l.is_empty())
        .collect();
    // Floor-check: obs.md's Gateway Metrics table has ≥8 rows.
    // If the build.rs grep path breaks (obs.md moved, table
    // reformatted), spec_metrics drops to 0 → test passes
    // vacuously. This guard catches that class.
    assert!(
        spec_metrics.len() >= 8,
        "spec_metrics.txt has only {} entries — build.rs grep broken?",
        spec_metrics.len()
    );
    assert_spec_metrics_described(
        &spec_metrics,
        rio_gateway::describe_metrics,
        "rio-gateway",
    );
}
```

The floor-check is the proves-nothing guard (same pattern as [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T51's helm-lint null-filter). The `assert_spec_metrics_described` signature at [`metrics.rs:238`](../../rio-test-support/src/metrics.rs) takes `&[&str]` — `Vec<&str>` derefs to that.

### T4 — `refactor(scheduler,store,worker,controller):` apply same pattern × 4 crates

MODIFY each of:
- `rio-scheduler/build.rs` + `tests/metrics_registered.rs` (prefix `rio_scheduler_`)
- `rio-store/build.rs` + `tests/metrics_registered.rs` (prefix `rio_store_`)
- `rio-worker/build.rs` + `tests/metrics_registered.rs` (prefix `rio_worker_`)
- `rio-controller/build.rs` + `tests/metrics_registered.rs` (prefix `rio_controller_`)

Same pattern as T2+T3. Some crates may not have a `build.rs` yet — NEW in that case. The scheduler's existing `build.rs` at [`build.rs:23-28`](../../rio-scheduler/build.rs) already calls `emit_metrics_grep`; add the `emit_spec_metrics_grep` call alongside.

Floor-check constants per crate (re-verify at dispatch): gateway ≥8, scheduler ≥20, store ≥12, worker ≥10, controller ≥6. Use conservative bounds — the guard is against zero-or-near-zero, not exact-count.

### T5 — `test(test-support):` spec-grep self-test

MODIFY [`rio-test-support/src/metrics_grep.rs`](../../rio-test-support/src/metrics_grep.rs) — add a `#[cfg(test)]` test at the bottom (test-support is a lib crate, `cargo test -p rio-test-support` runs it):

```rust
#[cfg(test)]
mod spec_grep_tests {
    #[test]
    fn grep_extracts_table_column_one() {
        let obs_md = "\
## Gateway Metrics
| Metric | Type | Description |
|---|---|---|
| `rio_gateway_foo_total` | Counter | desc |
| `rio_gateway_bar_seconds` | Histogram | desc |
| rio_scheduler_baz | Counter | wrong prefix (excluded) |
prose mention of rio_gateway_inline (excluded — not table row)
";
        // ... call the grep logic on this string (refactor to
        //     extract a pure fn grep_spec_names(&str, prefix) -> Vec<&str>
        //     so it's testable without OUT_DIR writes) ...
        assert_eq!(names, &["rio_gateway_bar_seconds", "rio_gateway_foo_total"]);
    }
}
```

Refactor `emit_spec_metrics_grep` to a pure `grep_spec_names(src: &str, prefix: &str) -> Vec<String>` wrapped by the `emit_*` fn that does the file I/O. Makes T5 testable and keeps the build.rs contract unchanged.

## Exit criteria

- `grep 'GATEWAY_METRICS.*&\[&str\]\|SCHEDULER_METRICS.*&\[&str\]' rio-*/tests/metrics_registered.rs` → 0 hits (hand-maintained consts removed)
- `grep 'SPEC_METRICS_RAW\|spec_metrics.txt' rio-*/tests/metrics_registered.rs` → ≥5 hits (one per crate)
- `grep 'emit_spec_metrics_grep' rio-*/build.rs` → ≥5 hits
- `grep 'rerun-if-changed.*observability.md' rio-*/build.rs` → 0 hits (the helper prints it, not each build.rs) — OR verify via `cargo build -vv 2>&1 | grep rerun-if-changed`
- `cargo nextest run -p rio-test-support grep_extracts_table_column_one` → pass
- `cargo nextest run all_spec_metrics_have_describe_call` → ≥5 passed (one per crate)
- Mutation: delete `rio_gateway_quota_rejections_total` from `lib.rs:76` describe → gateway's `all_spec_metrics_have_describe_call` fails (proves the derived const now includes it)
- `/nbr .#ci` green

## Tracey

References existing markers:
- `r[obs.metric.gateway]`, `r[obs.metric.scheduler]`, `r[obs.metric.store]`, `r[obs.metric.worker]`, `r[obs.metric.controller]` — T3+T4 keep the existing `r[verify]` annotations on `all_spec_metrics_have_describe_call` (annotation unchanged; implementation under the annotation changes from hand-const to derived)

No new markers. This is tooling parity with `EMITTED_METRICS` — implementation detail of how the spec→describe check sources its spec-list.

## Files

```json files
[
  {"path": "rio-test-support/src/metrics_grep.rs", "action": "MODIFY", "note": "T1+T5: +emit_spec_metrics_grep helper + grep_spec_names pure fn + self-test"},
  {"path": "rio-gateway/build.rs", "action": "MODIFY", "note": "T2: +emit_spec_metrics_grep call (NEW file if absent)"},
  {"path": "rio-gateway/tests/metrics_registered.rs", "action": "MODIFY", "note": "T3: GATEWAY_METRICS const → SPEC_METRICS_RAW include_str + ≥8 floor-check"},
  {"path": "rio-scheduler/build.rs", "action": "MODIFY", "note": "T4: +emit_spec_metrics_grep call (has emit_metrics_grep at :23-28)"},
  {"path": "rio-scheduler/tests/metrics_registered.rs", "action": "MODIFY", "note": "T4: SCHEDULER_METRICS const :25 → derived + floor-check"},
  {"path": "rio-store/build.rs", "action": "MODIFY", "note": "T4: +emit_spec_metrics_grep call (NEW if absent)"},
  {"path": "rio-store/tests/metrics_registered.rs", "action": "MODIFY", "note": "T4: STORE_METRICS const → derived + floor-check"},
  {"path": "rio-worker/build.rs", "action": "MODIFY", "note": "T4: +emit_spec_metrics_grep call (NEW if absent)"},
  {"path": "rio-worker/tests/metrics_registered.rs", "action": "MODIFY", "note": "T4: WORKER_METRICS const → derived + floor-check"},
  {"path": "rio-controller/build.rs", "action": "MODIFY", "note": "T4: +emit_spec_metrics_grep call (NEW if absent)"},
  {"path": "rio-controller/tests/metrics_registered.rs", "action": "MODIFY", "note": "T4: CONTROLLER_METRICS const → derived + floor-check"}
]
```

```
rio-test-support/src/
└── metrics_grep.rs              # T1+T5: spec-grep helper
rio-{gateway,scheduler,store,worker,controller}/
├── build.rs                     # T2/T4: emit_spec_metrics_grep
└── tests/metrics_registered.rs  # T3/T4: const → include_str
```

## Dependencies

```json deps
{"deps": [328, 367], "soft_deps": [304, 311, 295, 321], "note": "discovered_from=367 (rev-p367 feature). P0328 (DONE) established the bidirectional metrics check + metrics_grep.rs pattern this plan extends. P0367 (DONE) is the review that surfaced the 3-omission drift pattern. Soft-dep P0304-T149 (fixes the 3 GATEWAY_METRICS omissions manually — STOPGAP; this plan SUPERSEDES it. If P0304-T149 lands first, this plan deletes the freshly-added entries along with the const; if this plan lands first, P0304-T149 is moot — check at P0304 dispatch). Soft-dep P0311-T33/T35 (add all_histograms_have_bucket_config tests to same metrics_registered.rs files — both additive to different test-fns, non-overlapping). Soft-dep P0295-T91 (adds rio_gateway_jwt_mint_degraded_total + rio_scheduler_ca_hash_compares_total to obs.md tables — once landed, T3+T4's derived consts pick them up automatically; that's the point of this plan. Sequence: P0295-T91 FIRST preferred so the derived list includes them from day one). Soft-dep P0321 (HISTOGRAM_BUCKET_MAP — unrelated to SPEC_METRICS but touches metrics_registered.rs for the bucket-coverage test; additive). build.rs files are low-traffic (some NEW). metrics_registered.rs × 5 crates — P0304-T83/T101/T47, P0295-T57, P0311-T33 all touch them; all additive to different consts/test-fns; T3+T4 here replaces the top-of-file const only."}
```

**Depends on:** [P0328](plan-0328-metrics-registered-bidirectional.md) — `emit_metrics_grep` + `assert_spec_metrics_described` exist. [P0367](plan-0367-normalized-name-post-impl-hardening.md) — the drift pattern that motivates this.

**Conflicts with:** `metrics_registered.rs` × 5 files — many plans add entries to the hand-maintained consts ([P0304](plan-0304-trivial-batch-p0222-harness.md)-T47/T83/T101, [P0295](plan-0295-doc-rot-batch-sweep.md)-T57). This plan DELETES those consts. Sequence AFTER all const-add tasks, or coordinate: if a const-add task is gated on a future plan, that plan's metric goes directly to obs.md and T3+T4's grep picks it up automatically. [`metrics_grep.rs`](../../rio-test-support/src/metrics_grep.rs) — low-traffic, additive.
