# Plan 997443701: rio_worker_upload_references_count histogram — missing bucket config

Same bug class [P0321](plan-0321-build-graph-edges-histogram-buckets.md) fixed for `rio_scheduler_build_graph_edges`, discovered by reviewer at the same time P0321 merged. `rio_worker_upload_references_count` is a count-type histogram at [`upload.rs:196`](../../rio-worker/src/upload.rs) recording `references.len()` — the number of store-path references scanned out of each built output before upload. Described at [`lib.rs:144-149`](../../rio-worker/src/lib.rs) ("distribution of dependency fan-out per built path"), introduced by [`9165dc23`](https://github.com/search?q=9165dc23&type=commits).

The metric has no entry in [`HISTOGRAM_BUCKET_MAP`](../../rio-common/src/observability.rs) at [`observability.rs:292-311`](../../rio-common/src/observability.rs). It gets the default `[0.005..10.0]` buckets. Reference counts range 0–hundreds (a typical glibc derivation has ~40 refs; a compiler toolchain can hit 200+). **Every output with >10 references lands in `+Inf`.** The p99 is unusable, same as the `build_graph_edges` case.

P0321 landed the structural guard — [`assert_histograms_have_buckets`](../../rio-test-support/src/metrics.rs) at [`metrics.rs:328`](../../rio-test-support/src/metrics.rs) — but only wired it in [`rio-scheduler/tests/metrics_registered.rs:101`](../../rio-scheduler/tests/metrics_registered.rs). The worker crate never got the test; had it existed, this would have failed CI the moment [`9dcdb0d4`](https://github.com/search?q=9dcdb0d4&type=commits) added the `describe_histogram!` call. The broader sweep (controller/store/gateway) is [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T33; this plan wires the worker test specifically because it catches the live bug.

Also missing: the metric has no row in the [`observability.md:154-173`](../../docs/src/observability.md) Worker Metrics table, nor in the Histogram Buckets table at [`:199-206`](../../docs/src/observability.md). T4 backfills both.

## Entry criteria

- [P0321](plan-0321-build-graph-edges-histogram-buckets.md) merged (`HISTOGRAM_BUCKET_MAP` table + `assert_histograms_have_buckets` helper exist) — **DONE**

## Tasks

### T1 — `fix(common):` add REFERENCES_COUNT_BUCKETS + 7th HISTOGRAM_BUCKET_MAP entry

MODIFY [`rio-common/src/observability.rs`](../../rio-common/src/observability.rs) — add a new const after [`GRAPH_EDGES_BUCKETS`](../../rio-common/src/observability.rs) at `:269`:

```rust
/// Histogram bucket boundaries for `rio_worker_upload_references_count`.
///
/// Reference COUNT (not seconds) per output upload — references.len()
/// after NAR scan. Typical leaf derivation: 0-5 refs. glibc-class: ~40.
/// Toolchains: 100-300. Default Prometheus buckets [0.005..10.0] are
/// useless here — every output with >10 refs lands in +Inf. Second
/// count-type histogram after GRAPH_EDGES_BUCKETS above.
const REFERENCES_COUNT_BUCKETS: &[f64] = &[1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0];
```

Then add a 7th entry to [`HISTOGRAM_BUCKET_MAP`](../../rio-common/src/observability.rs) after `:310`:

```rust
    ("rio_worker_upload_references_count", REFERENCES_COUNT_BUCKETS),
```

**Bucket rationale:** `1.0` catches leaf outputs (single self-ref only); `5-10` catches typical small packages; `25-50` catches glibc/coreutils class; `100-250` catches compiler toolchains; `500` catches pathological wide closures. The existing `GRAPH_EDGES_BUCKETS` goes to 20K because it's a whole-DAG query; references-per-output is per-path, rarely exceeds a few hundred.

### T2 — `test(worker):` all_histograms_have_bucket_config — catches the bug

MODIFY [`rio-worker/tests/metrics_registered.rs`](../../rio-worker/tests/metrics_registered.rs) — import the helper and add a third test after `:42`:

```rust
use rio_test_support::metrics::{
    assert_emitted_metrics_described, assert_histograms_have_buckets,
    assert_spec_metrics_described,
};

// ...existing tests...

// r[verify obs.metric.worker]
// Catches rio_worker_upload_references_count shipping with no
// HISTOGRAM_BUCKET_MAP entry (P997443701 — same bug class as P0321's
// build_graph_edges). Every sample with >10 refs was landing in +Inf.
#[test]
fn all_histograms_have_bucket_config() {
    use rio_common::observability::HISTOGRAM_BUCKET_MAP;

    // rio_worker_fuse_fetch_duration_seconds: gRPC GetPath + stream
    // drain — sub-second typical, [0.005..10.0] fits.
    const DEFAULT_BUCKETS_OK: &[&str] = &["rio_worker_fuse_fetch_duration_seconds"];

    assert_histograms_have_buckets(
        rio_worker::describe_metrics,
        HISTOGRAM_BUCKET_MAP,
        DEFAULT_BUCKETS_OK,
        "rio-worker",
    );
}
```

**CHECK AT DISPATCH:** worker has three `describe_histogram!` calls at [`lib.rs:65,81,144`](../../rio-worker/src/lib.rs) — `build_duration_seconds` (already in map), `fuse_fetch_duration_seconds` (sub-second → default-OK), `upload_references_count` (T1 adds). If a fourth histogram has landed between this plan's write and dispatch, add it to either the map or the exempt list.

### T3 — `test(worker):` add upload_references_count to WORKER_METRICS const

MODIFY [`rio-worker/tests/metrics_registered.rs:6-23`](../../rio-worker/tests/metrics_registered.rs) — the `WORKER_METRICS` const mirrors the observability.md Worker Metrics table, and `rio_worker_upload_references_count` is missing from both. Add it after `:18`:

```rust
    "rio_worker_upload_references_count",
```

Also add `rio_worker_stale_assignments_rejected_total` and `rio_worker_upload_skipped_idempotent_total` if they're in the spec table but not the const (quick sync — `grep rio_worker_ observability.md` vs the const at dispatch).

The `all_emitted_metrics_are_described` test at `:35` has a hardcoded `15` for expected count — if T3 adds rows and the build.rs emission-scanner already saw them, bump the count to match `wc -l` of `emitted_metrics.txt`. If not, the test is counting emit-sites not spec-metrics and stays 15.

### T4 — `docs(obs):` add upload_references_count rows to Worker Metrics + Histogram Buckets tables

MODIFY [`docs/src/observability.md`](../../docs/src/observability.md) — two row inserts.

**Worker Metrics table** at `:154-173` — insert after `:168` (`rio_worker_upload_bytes_total`):

```markdown
| `rio_worker_upload_references_count` | Histogram | Reference count per output upload (`references.len()` after NAR scan). Distribution of dependency fan-out. Zero-heavy = mostly leaves; high p99 = wide transitive closures. Buckets: `[1, 5, 10, 25, 50, 100, 250, 500]`. |
```

**Histogram Buckets table** at `:199-206` — insert after `:204` (`assignment_latency_seconds`):

```markdown
| `rio_worker_upload_references_count` | `[1, 5, 10, 25, 50, 100, 250, 500]` (count) |
```

Note the `(count)` suffix to distinguish from the `(seconds unless noted)` column header.

## Exit criteria

- `/nbr .#ci` green
- `grep -c 'REFERENCES_COUNT_BUCKETS' rio-common/src/observability.rs` ≥ 2 (T1: const defined + referenced in map)
- `grep 'rio_worker_upload_references_count' rio-common/src/observability.rs` → ≥1 hit in `HISTOGRAM_BUCKET_MAP` (T1)
- `cargo nextest run -p rio-worker all_histograms_have_bucket_config` → passes (T2)
- **Mutation criterion:** comment out the `("rio_worker_upload_references_count", ...)` row in `HISTOGRAM_BUCKET_MAP` → T2's test fails with `"no HISTOGRAM_BUCKET_MAP entry"`. Restore → passes. (Proves the test catches the original bug.)
- `grep 'rio_worker_upload_references_count' rio-worker/tests/metrics_registered.rs` → ≥1 hit (T3: in WORKER_METRICS const)
- `grep -c 'rio_worker_upload_references_count' docs/src/observability.md` ≥ 2 (T4: Worker Metrics row + Histogram Buckets row)
- **Precondition self-check:** T2's `assert_histograms_have_buckets` already asserts `!histograms.is_empty()` internally at [`metrics.rs:338`](../../rio-test-support/src/metrics.rs) — a broken recorder that collects zero vacuously passes nothing

## Tracey

References existing markers:
- `r[obs.metric.worker]` — T4 adds the spec-table row; T2 adds `r[verify obs.metric.worker]` on the bucket-coverage test; T3 keeps the WORKER_METRICS const synced with the spec table under the same marker

No new markers. Bucket config is an implementation detail of making the spec'd histogram useful — the existing `r[obs.metric.worker]` at [`observability.md:153`](../../docs/src/observability.md) covers the worker metric table.

## Files

```json files
[
  {"path": "rio-common/src/observability.rs", "action": "MODIFY", "note": "T1: +REFERENCES_COUNT_BUCKETS const after :269, +7th HISTOGRAM_BUCKET_MAP entry after :310"},
  {"path": "rio-worker/tests/metrics_registered.rs", "action": "MODIFY", "note": "T2: +all_histograms_have_bucket_config test after :42; T3: +upload_references_count to WORKER_METRICS const at :18"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T4: +upload_references_count row to Worker Metrics table after :168, +row to Histogram Buckets table after :204"}
]
```

```
rio-common/src/
└── observability.rs              # T1: +REFERENCES_COUNT_BUCKETS + 7th map entry
rio-worker/tests/
└── metrics_registered.rs         # T2+T3: bucket-coverage test + const sync
docs/src/
└── observability.md              # T4: spec table rows
```

## Dependencies

```json deps
{"deps": [321], "soft_deps": [311], "note": "P0321 DONE — HISTOGRAM_BUCKET_MAP table at observability.rs:292 + assert_histograms_have_buckets helper at metrics.rs:328 both exist. Self-contained ~25-line fix mirroring P0321's T1+T3. soft_dep P0311: T33 there does the broader assert_histograms_have_buckets sweep across controller/store/gateway — this plan's T2 covers worker specifically because that's where the live bug is. If P0311 dispatches first and does a full 4-crate sweep including worker, T2 here becomes a no-op (check at dispatch: grep assert_histograms_have_buckets rio-worker/tests/); the T1+T3+T4 fix itself stays valid regardless."}
```

**Depends on:** [P0321](plan-0321-build-graph-edges-histogram-buckets.md) — **DONE**. The `HISTOGRAM_BUCKET_MAP` table and `assert_histograms_have_buckets` helper both landed there.

**Conflicts with:** [`observability.rs`](../../rio-common/src/observability.rs) not in top-50 collisions; T1 is a pure append after existing consts. [`observability.md`](../../docs/src/observability.md) count=22 — T4 inserts two table rows, purely additive; [P0304](plan-0304-trivial-batch-p0222-harness.md)-T26/T50/T83 also touch it (different sections, non-overlapping). [P0295](plan-0295-doc-rot-batch-sweep.md)-T52 adds the `build_graph_edges` row to the same Histogram Buckets table — adjacent row inserts, rebase-clean either order. [`metrics_registered.rs`](../../rio-worker/tests/metrics_registered.rs) is worker-only, no other UNIMPL plan touches it.
