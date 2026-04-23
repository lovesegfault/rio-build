# Review rules

Recurrence patterns that bug-hunt rounds keep finding. Each rule names
a shape, the bugs it produced, and the structural close. When a diff
matches the shape, the reviewer checks the close was applied or that
the author explicitly opted out (with the threat-model clause named).

## Granularity coupling

Converting `T` → `Option<T>` (or `[T; K]` → `[Option<T>; K]`, or any
refinement that introduces a new presence/cardinality axis) means
**every gate, threshold, and aggregate** that consumed the old type at
row granularity MUST be re-stated at the new granularity, or
explicitly opt out with a comment naming the threat-model clause it
would otherwise weaken.

**Why:** the type change creates a new cardinality axis that the
existing gates don't cover. r2 bug_037 made `HwPerfSampleRow.factor`
`[Option<f64>; K]` (per-dim presence) so a `bench_needed=false` row
contributes nothing to membw/ioseq medians instead of a `1.0`
placeholder. Correct — but `cross_tenant_median`'s `min_tenants` gate
still counted **rows** (`by_tenant.len()`), so one tenant writing K=3
satisfied the row count while being the sole `Some(membw)`
contributor: `r[sched.sla.threat.hw-median-of-medians]` ("two
colluding tenants cannot capture") violated by one (r3 bug_013). The
r2 test `placeholder_free_membw_median` asserted the vulnerable
behaviour as the feature.

**Checklist when introducing per-field/per-dim `Option`:**

- [ ] Every `len()` / count gate that fed the old aggregate: does it
      now need to count `Some`-per-axis?
- [ ] Every downstream RPC that reports the count: does its payload
      need a per-axis breakdown? (bug_013: `HwClassSampledResponse`
      `u32` → `[u32; K]`.)
- [ ] Every test that asserts "one observation drives the aggregate":
      is that still the intended threat-model posture?
- [ ] Every content-hash / `solve_relevant_hash` that covers the
      field: does it hash the per-axis trust booleans?
