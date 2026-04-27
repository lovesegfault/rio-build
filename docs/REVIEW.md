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

The same rule applies to **row-subset filters**: any filter that
produces a row subset (`idx`, `retain`, the `p_bar` collinearity drop
at ingest.rs `cs_f`/`w_f`) creates a granularity axis. Every aggregate
computed *before* the filter and stored as describing the *post-filter*
fit is suspect. r5 bug_023: `n_eff`/`n_distinct_c`/`sum_w` were
computed on the full ring but `als_fit`/`sigma_resid` ran on the
`idx`-filtered subset, so `z_q()` overstated df → under-widened CI →
`c*` undersized (anti-conservative).

## Witness-flag completeness

A `for { if X { continue } if Y { continue } … }` loop where the
post-loop logic depends on **which** `continue` fired is a finding.
Convert the body to `-> Result<T, RejectReason>` and fold reasons
explicitly.

**Why:** open-coded witness flags must be set at every `continue`, and
each new gate must remember to set one. r2 bug_039 added
`any_lambda_gated` / `any_envelope_gated` to `solve_full`'s 5-`continue`
per-cell loop and instrumented 2 of them; r3 merged_bug_019 found the
other 3 (`c_lo > cap_c`, mem/disk ceiling, `smallest_fitting=None`)
left both flags unset, mislabelling `DiskCeiling` as
`InterruptRunaway`. Same shape as r1 merged_bug_013 ("registered ≠
emitted") and r2 merged_bug_006 ("3/4 reasons dead"): an enum domain is
declared, N sites must populate it, M<N do.

**Structural close:** extract the loop body as `fn evaluate_one(...) ->
Result<Ok, RejectReason>` where `RejectReason` is an exhaustive enum
with one variant per gate. The loop becomes `match evaluate_one(...) {
Ok(x) => oks.push(x), Err(r) => rejects.push(r) }`; the post-loop fold
is a `fn classify(rejects: &[RejectReason]) -> Label`. A new gate = new
variant = `classify`'s match is non-exhaustive = compile error.
**`matches!()` is NOT compiler-checked** — it desugars to `_ => false`.
Use an explicit `match` with no `_` arm. Pair
with a table-driven test (`const FIXTURES: &[(&[RejectReason], Label)]`)
that asserts each `Label` variant is reachable AND the converse (any
`{non-target}` reject → never `{target}`). Emit-site existence tests
(r2 CR-2) don't catch "fires when it shouldn't"; the fixture table
does.
