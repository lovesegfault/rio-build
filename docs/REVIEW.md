# Review rules

Recurrence patterns that bug-hunt rounds keep finding. Each rule names
a shape, the bugs it produced, and the structural close. When a diff
matches the shape, the reviewer checks the close was applied or that
the author explicitly opted out (with the threat-model clause named).

## Nth-strike type-check

**Nth-strike (N≥3) on the same invariant ⟹ restructure so the
invariant is compiler-checked, NOT add a review rule.** Review rules
catch first-strike; by third strike the rule existed, was followed,
and still broke.

**Why:** five of ten r6 bugs were second-order regressions in r5's own
fixes. Each fix was reviewed against the rule it was closing; none
re-enumerated adjacent invariants the change perturbed. r6 bug_012:
R5B8's plan listed pre-/post-filter `n_eff` consumers; the dispatch
gates appeared in NEITHER list. r6 bug_021: R5B6's `cap_c.max(1.0)`
floor carried a "spot-only" comment at solve.rs:837-840; the
implication for `all(λ-adjacent)` over a mixed-cap vec was not drawn.
r6 bugs 004+011: r5-validation closed Opt2's success over-stickiness;
failure over-stickiness (infeasible/ICE) was not modeled. The review
rule was correct AND followed AND insufficient — the invariant lives
in too many heads.

**Structural close — template by strike cluster:**

- **Newtype split** (r6 bug_012 → `RingNEff`/`FitDf`): two readers
  want different semantics from one field → split into two
  newtype-wrapped fields so mixing is a type error. The next reader
  cannot re-conflate.
- **Extract function + sum-type return** (r6 bugs 004+011 →
  `resolve_h_explore` → `HExploreOutcome`): a state-machine open-coded
  across N locations with implicit ordering dependencies → one
  function whose return enum names every outcome. Unit tests pin each
  transition; the caller is a `match` with no `_` arm. The sum-type
  pins ONE transition. If the invariant is over a **trajectory**
  (rotation covers a set, retry-ladder terminates, backoff is
  monotone), the unit test MUST drive ≥N transitions and assert the
  trajectory property directly — `seen == pool`,
  `attempts.is_sorted()`, `Σ ≤ budget`. A single-transition test
  (`next ≠ prev`) at the degenerate domain size (`|pool|=2`,
  `retries=1`) is the §Stability-tests vacuity shape: it passes
  against every non-identity function. r7 mb_001:
  `resolve_besteffort_rotates` asserted `next ≠ h_tried` at
  `|pool|=2`; the 2-cycle bug satisfies it.
- **Partition at type level** (r6 bug_021 → `spot_rejects` /
  `od_rejects`): a predicate semantically defined over a subset reads
  a mixed collection → partition the collection at the source so the
  predicate's input IS the subset. A future change to the other
  partition's reachable variants cannot break it.
- **Canonicalize** (r7 bugs 032+034): ≥2 open-coded copies of one
  transform (fold, model curve, hash, rotation) → callers get a
  `pub fn`, not a recipe. Reviewer asks: does an existing fn do this?
  r7 bug_034: `counter_map` existed; fresh `.collect()` written
  anyway. r7 bug_032: `DurationFit::t_at` existed; gate_b open-coded
  `s+p/c+q·c`. Directive becomes "call X", not "implement X-shaped
  thing" — deletes the degree of freedom (seed-consumption,
  fold-vs-collect, clamp-or-not) the recipe could get wrong.

A new review rule MAY accompany the type-check (so first-strike on a
*different* invariant is caught) — but the type-check is the close,
not the rule.

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

**Parallel maps at different key granularity.** Two maps that
debounce/memoize the same logical event MUST share the same key tuple,
OR the coarser map's predicate is provably invariant under the dropped
dimensions — invariance stated at the declaration AND asserted by a
test `pred(k, finer1) == pred(k, finer2)`. r6 bug_003:
`infeasible_static_fh` was added (R5B3) keyed `mkh`-only with an
*untested* invariance comment at solve.rs:1072-1074, alongside its
sibling `MemoEntry.last_infeasible_fh` keyed `(mkh, ovr)`. The dropped
dimension `override_.tier` changes the debounced predicate (emit
decision) without changing the coarser key → two overrides on one
`mkh` race for one suppress slot.

## Semantic field change

Changing a stored field's **semantics**, **key granularity**, or
**reachable-value-set**: the commit body states `rg -c <field>` total;
each hit is classified `{wants-old, wants-new, indifferent}`; the
reviewer checks Σ classifications == total. When ≥1 reader wants-old
AND ≥1 reader wants-new ⟹ MUST split into two fields (newtype) —
comment-lists don't survive the next change.

**Why:** r6 bug_012: R5B8 changed `FittedParams.n_eff` from pre-filter
(ring cardinality) to post-filter (`z_q` df). The plan's
consumer-enumeration comments at ingest.rs:197-201/243-248 listed
pre-filter consumers and post-filter consumers — and missed the two
dispatch gates at snapshot.rs:778 + solve.rs:413, which appear in
NEITHER list and want pre-filter. `rg -c n_eff` was ≈49 hits / 11
files; the commit body listed ~14. The Σ-check would have surfaced the
gap as 35 unclassified hits.

**Structural close:** the Σ-check catches "missed a reader"; the
`{wants-old, wants-new}` partition catches "readers disagree". When
they disagree, the field split (`RingNEff` / `FitDf`) makes the
disagreement a type — every future reader picks one explicitly, and
`rg n_eff` no longer matches both populations.

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

The rule applies to **writes** as well as gates: a `HashMap::iter()`
loop where which-element-first determines what's *written to a shared
slot* is the same finding. r6 bug_004: `pinned_explore` is stored at
`(mkh, ovr)` granularity but its initial value was `pool.choose(&mut
rng)` with `rng` seeded from per-loop-element `drv_hash` — whichever
heads-drv `dag.iter_nodes()` reached first wrote the shared slot. Same
DAG, different process restart → different first-writer → different
pin → ε_h Job churn. Close: seed the *value* from the *storage key*
(`mkh ^ ovr`), so the write is a pure function of the slot it writes
to.

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

## Model-formula reimplementation

Any consumer that evaluates `T(c)`, `M(c)`, or another `sla::types`
model curve MUST call the canonical method (`DurationFit::t_at`,
`MemFit::at`). Open-coding `s + p/c [+ q·c]` outside `types.rs` is a
finding. Cross-crate Rust consumers reconstruct via
`duration_fit_from_status(&SlaStatusResponse)` and call the method.

**Exception:** test/harness fixtures that SEED synthetic data with a
known curve are stating ground truth, not measuring against the model
— open-coding there is fine.

**Non-Rust measure-sites** (Python/TS) cannot call `t_at`; the first
such site is a finding to add a `t_ref_at` field to
`SlaStatusResponse`, NOT to open-code.

**Why:** r7 bug_032: gate_b open-coded the un-clamped Amdahl form;
`DurationFit::t_at` clamps `c.min(p̄)` for Capped/Usl. For samples at
`c > p̄` the open-coded formula under-predicts by `c/p̄`, projecting
as spurious per-h spread → false-FAIL of the §13a GO gate.
