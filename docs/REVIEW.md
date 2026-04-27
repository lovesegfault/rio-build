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

## HashMap iteration order as a stable choice

A `for (k, v) in hashmap { if cond { continue/break/return } }` where
**which element is seen first** changes the output is a finding. The
loop is making a decision (admission, selection, truncation) that
depends on `HashMap::iter()` order — undefined and unstable across
restarts/rehashes.

**Why:** r5 bug_025's forecast budget gate ran greedy first-fit during
`dag.iter_nodes()` (HashMap-backed): same DAG state → different
admitted subset across restarts → §13b NodeClaim FFD never saw the
dropped large drv. A post-loop sort can't resurrect what the in-loop
gate already dropped. Same shape recurred in `reap_excess_pending`
ordering and ε_h `pool.choose()` over unsorted `h_all`
(snapshot.rs:728-729 fixed with explicit sort).

**Structural close:** collect → sort by a deterministic key → decide.
The sort key SHOULD match the downstream consumer's order (here: §13b
FFD's `(priority, c*) desc`) so the admitted subset is what the
consumer wanted first. Add a stable tiebreak (e.g. `drv_hash` asc) so
ties don't re-introduce nondeterminism.

## Stability tests perturb within the noise band

Every `solve_relevant_hash` / `inputs_gen` stability assertion MUST
perturb inputs within the documented noise band — `alu ±20%` (bench
reproducibility), λ exposure +600s at steady rate (EMA state diverges,
quotient converges), price ±1% (spot tick) — NOT bit-identical
re-inserts. Seed ≥`FLEET_MEDIAN_MIN_TENANTS` distinct
`submitting_tenant` rows first, or the per-dim trust gate pins
`factor=[1.0;K]` and the perturbation is a no-op regardless of what
the hash covers.

**Why:** r5 merged_bug_018: `solve_relevant_hash` hashed bit-exact f64
EMA *state* (diverging `(num,den)` sums, raw bench medians) instead of
the converging *signal* `solve_full` reads. The contract test
`inputs_gen_stable_across_noop_refresh` re-inserted `'{"alu":1.4}'`
bit-identical to its seed, so the diverging-state hash didn't move
either — the test passed against the bug it existed to catch. The fix
quantizes (`(lambda_for·1e6).round()`, `(factor·100).round()`,
`(price·1e4).round()`); the test now inserts `1.401` (same bucket as
`1.4`, different bits) and asserts UNCHANGED, then `1.5` (different
bucket) and asserts CHANGED.

**Structural close:** a stability test is "noise → no change; signal →
change". Bit-identical re-insert exercises neither — it's "nothing →
no change", which any hash satisfies. The noise half MUST use a value
that differs in storage representation but lands in the same quantum;
the signal half MUST cross a quantum boundary.
