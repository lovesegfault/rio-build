# Plan 0288: Bloom saturation gauge + configurable expected-items

**Retro P0089 finding.** Worker FUSE-cache bloom filter is "never deleted from — evicted paths stay as stale positives. Only restart clears it. 50k expected items at 1% FPR ≈ 60 KB." **Zero saturation observability was ever added — and the documented indirect signals point the WRONG way.**

Workers now deploy as StatefulSet pods ([`rio-controller/src/reconcilers/workerpool/builders.rs:331-337`](../../rio-controller/src/reconcilers/workerpool/builders.rs)) — stable identity, long-lived. Low-ordinal workers can run indefinitely under autoscaler scale-down.

**The failure is silent and existing metrics diagnose the OPPOSITE:**

| Metric | Doc says | Under saturation actually shows |
|---|---|---|
| `rio_worker_prefetch_total{result="already_cached"}` | "high = scheduler bloom filter stale" (heartbeat LAG, bloom MISSING paths) | **Decreases.** FPR rises → bloom claims paths worker DOESN'T have → scheduler SKIPS hint ([`dispatch.rs:530`](../../rio-scheduler/src/actor/dispatch.rs) `.filter(|p| !bloom.maybe_contains(p))`) |
| `rio_scheduler_prefetch_paths_sent_total` | "High avg = workers cold" | **Low avg — indistinguishable from "locality scoring is working great."** |
| Locality term at `W_LOCALITY=0.7` ([`assignment.rs:153`](../../rio-scheduler/src/actor/assignment.rs)) | (dominates scoring) | `count_missing()` undercounts → `transfer_cost` → 0 → scoring silently becomes pure `W_LOAD=0.3`. No metric reports "locality contributing 0." |

**User decision: gauge + configurable const.** `fill_ratio()` method, `rio_worker_bloom_fill_ratio` gauge, `worker.toml bloom_expected_items`. Alert threshold ~0.5 in `observability.md` (at k=7, fill≥0.5 → FPR climbs past 1% nonlinearly).

Note: [P0296](plan-0296-ephemeral-builders-opt-in.md) (ephemeral builders) obviates this for `ephemeral=true` pools (fresh pod = fresh bloom). STS pools still need the gauge.

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync.md) merged (phase4b fan-out root)

## Tasks

### T1 — `feat(common):` BloomFilter::fill_ratio()

MODIFY [`rio-common/src/bloom.rs`](../../rio-common/src/bloom.rs) — alongside `num_bits()`/`hash_count()`/`byte_len()`:

```rust
// r[impl obs.metric.bloom-fill-ratio]
/// Fraction of bits set. 0.0 = empty, 1.0 = all bits set (every query
/// returns "maybe"). popcount over ~60KB = microseconds. At k=7 hash
/// functions, fill_ratio ≥ 0.5 → FPR climbs past the configured 1%
/// nonlinearly; alert there.
pub fn fill_ratio(&self) -> f64 {
    let ones: u64 = self.bits.iter().map(|b| b.count_ones() as u64).sum();
    ones as f64 / self.num_bits as f64
}
```

Add a unit test: empty filter → 0.0; insert N items into a filter sized for N at 1% FPR → fill should land near the theoretical `1 - e^(-k*n/m)` (for k=7, m sized optimally, ≈ 0.5). Tolerance ±0.05.

### T2 — `feat(worker):` emit gauge in the 10s heartbeat loop

MODIFY [`rio-worker/src/runtime.rs`](../../rio-worker/src/runtime.rs) at [`:96-106`](../../rio-worker/src/runtime.rs) — the bloom snapshot is already cloned every 10s for the heartbeat. Read `fill_ratio()` off the same snapshot BEFORE releasing the read lock:

```rust
let (local_paths, bloom_fill) = bloom.map(|b| {
    let snapshot = b.read().unwrap_or_else(|e| e.into_inner()).clone();
    let fill = snapshot.fill_ratio();
    let (data, hash_count, num_bits, version) = snapshot.to_wire();
    (
        rio_proto::types::BloomFilter { data, hash_count, num_bits, /* ... */ },
        fill,
    )
}).unzip();
if let Some(fill) = bloom_fill {
    metrics::gauge!("rio_worker_bloom_fill_ratio").set(fill);
}
```

Register the gauge in `rio-worker`'s metric init (same spot as `rio_worker_fuse_cache_size_bytes`).

### T3 — `feat(worker):` bloom_expected_items config field

MODIFY [`rio-worker/src/config.rs`](../../rio-worker/src/config.rs) — add `bloom_expected_items: Option<usize>` to the `Config` struct at `:18`. Default `None` → fall back to the current compile-time `50_000`.

MODIFY [`rio-worker/src/fuse/cache.rs`](../../rio-worker/src/fuse/cache.rs) at [`:208`](../../rio-worker/src/fuse/cache.rs) — thread the config through `Cache::new()`:

```rust
// Default stays 50k for backwards compat. Operators with larger
// caches (or long-lived low-ordinal STS workers that churn past
// 50k inserts) bump this via worker.toml. Oversizing is cheap
// (~1.2 bytes/item at 1% FPR — 500k items = ~600KB filter).
const BLOOM_EXPECTED_ITEMS_DEFAULT: usize = 50_000;
// ...
let expected = cfg.bloom_expected_items.unwrap_or(BLOOM_EXPECTED_ITEMS_DEFAULT);
```

### T4 — `docs:` metric row + alert threshold

MODIFY [`docs/src/observability.md`](../../docs/src/observability.md) — add a row to the `r[obs.metric.worker]` table at [`:143`](../../docs/src/observability.md):

```markdown
| `rio_worker_bloom_fill_ratio` | Gauge | Fraction of bloom filter bits set. Alert ≥ 0.5: at k=7, FPR climbs past 1% nonlinearly. Saturation is SILENT — `prefetch_total{result="already_cached"}` DECREASES under saturation (scheduler skips hints it thinks worker has), indistinguishable from healthy locality. Long-lived STS workers churn past `bloom_expected_items` via eviction; the filter never shrinks. Fix: bump `worker.toml bloom_expected_items` or restart the pod. |
```

Also fix the existing `rio_worker_prefetch_total` docstring at [`:156`](../../docs/src/observability.md) — the "Sustained high `already_cached` = scheduler bloom filter stale" claim is about heartbeat lag; append: "(Note: SATURATION produces the OPPOSITE signal — see `rio_worker_bloom_fill_ratio`.)"

### T5 — `test:` VM smoke — gauge present and reasonable

MODIFY [`nix/tests/scenarios/observability.nix`](../../nix/tests/scenarios/observability.nix) — in the existing worker-metrics scrape, add an assertion:

```python
# r[verify obs.metric.bloom-fill-ratio]
# Gauge present and in [0.0, 1.0]. After a single build it should be
# >0.0 (paths were inserted) and well below 0.5 (we're nowhere near
# 50k items in a VM test).
fill_line = [l for l in metrics.splitlines() if 'rio_worker_bloom_fill_ratio' in l and not l.startswith('#')]
assert fill_line, "bloom_fill_ratio gauge missing"
fill = float(fill_line[0].split()[-1])
assert 0.0 < fill < 0.5, f"unexpected fill ratio: {fill}"
```

## Exit criteria

- `/nbr .#ci` green
- `grep fill_ratio rio-common/src/bloom.rs` → ≥1 hit (method + test)
- `grep rio_worker_bloom_fill_ratio rio-worker/` → ≥1 hit
- `grep bloom_expected_items rio-worker/src/config.rs` → ≥1 hit
- Metric row in `observability.md` worker table

## Tracey

References existing markers:
- `r[obs.metric.worker]` — T4 adds a row to this table

Adds new markers to component specs:
- `r[obs.metric.bloom-fill-ratio]` → `docs/src/observability.md` (see ## Spec additions below)

## Spec additions

New standalone marker in [`docs/src/observability.md`](../../docs/src/observability.md), inserted as a separate paragraph after the `r[obs.metric.worker]` table (before `r[obs.metric.transfer-volume]`):

```markdown
r[obs.metric.bloom-fill-ratio]

The worker emits `rio_worker_bloom_fill_ratio` (gauge, 0.0–1.0) every heartbeat
tick (10s). Alert threshold 0.5 — at k=7 hash functions, fill ≥ 0.5 means FPR
has climbed past the configured 1% nonlinearly. Saturation causes scheduler
locality scoring to silently degrade (`count_missing()` undercounts →
`W_LOCALITY` term → 0) with NO direct symptom in existing metrics. The filter
never shrinks (evicted paths stay as stale positives); only restart clears it.
Operators bump `bloom_expected_items` in `worker.toml` for long-lived pools.
```

## Files

```json files
[
  {"path": "rio-common/src/bloom.rs", "action": "MODIFY", "note": "T1: fill_ratio() method + unit test"},
  {"path": "rio-worker/src/runtime.rs", "action": "MODIFY", "note": "T2: emit gauge in 10s heartbeat loop at :96-106"},
  {"path": "rio-worker/src/config.rs", "action": "MODIFY", "note": "T3: bloom_expected_items Option<usize> field"},
  {"path": "rio-worker/src/fuse/cache.rs", "action": "MODIFY", "note": "T3: thread config through, default 50k"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T4: metric row + fix prefetch_total docstring + spec addition r[obs.metric.bloom-fill-ratio]"},
  {"path": "nix/tests/scenarios/observability.nix", "action": "MODIFY", "note": "T5: gauge-present VM assertion"}
]
```

```
rio-common/src/bloom.rs               # T1: fill_ratio()
rio-worker/src/
├── runtime.rs                        # T2: emit gauge
├── config.rs                         # T3: config field
└── fuse/cache.rs                     # T3: thread config
docs/src/observability.md             # T4: row + spec addition
nix/tests/scenarios/observability.nix # T5: VM assertion
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [], "note": "retro P0089 — discovered_from=89. Zero saturation observability; existing indirect signals point WRONG way (saturation DECREASES already_cached, looks like 'locality working'). popcount over 60KB = µs. Workers ARE long-lived (STS, low-ordinal never restarts). Alert ~0.5 (k=7 nonlinear). P0296 obviates for ephemeral pools; STS pools still need this."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync.md) — phase4b fan-out root.
**Conflicts with:** [`observability.md`](../../docs/src/observability.md) also touched by [P0287](plan-0287-trace-linkage-submitbuild-metadata.md) (different section — tracing vs metrics). [`observability.nix`](../../nix/tests/scenarios/observability.nix) also touched by P0287 (different subtest). [`runtime.rs`](../../rio-worker/src/runtime.rs) also touched by [P0296](plan-0296-ephemeral-builders-opt-in.md) (single-shot exit path) — different function.
