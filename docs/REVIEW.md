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
  transition; the caller is a `match` with no `_` arm. When the
  extracted function's state grows a second field with the same
  lifecycle (r8: `pinned_explore_a` alongside `pinned_explore`),
  **bundle them** (`struct Pin{h, prev_a}`) and make the function
  `Pin → Pin`. A sibling field whose transition lives at the CALLER
  while the original's lives in the extracted fn is the r17 bug_001
  shape: the sum-type return describes one field's transitions and the
  caller reconstructs the other's from it — lossily, because the
  sum-type wasn't designed to carry the second field's inputs. The
  sum-type pins ONE transition. If the invariant is over a **trajectory**
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
- **Precondition → postcondition** (r8 bug_003 → `h_explore_pool`): a
  function with a documented precondition that callers must maintain
  ("`x ⊆ S`", "sorted", "non-empty") is a finding when the only caller
  doesn't maintain it. Close: the function that DOCUMENTS the
  precondition also CONSTRUCTS its input — `pub fn build_x() -> X`;
  caller passes `build_x()`. Precondition becomes postcondition; a
  future caller cannot construct an input the doc doesn't describe.
  r8 bug_003: `resolve_h_explore` claimed `pool ⊆ H\A\{cheapest}` at
  three doc sites; the caller's else-branch built `H\A`.
- **Restructure inputs/outputs** (r25 mb_009 → `cover::sizing`):
  STRIKE-3 on a fold-then-consume shape means the SIGNATURE is wrong,
  not the body. `cover_deficit` read a `sum_deficit -> (Σc, Σm, max_m,
  max_d, max_h, min_eta)` tuple then open-coded the per-claim loop;
  three rounds, three "one tuple element computed/consumed wrong" bugs
  (r24-mb_024 no anchor → R24B0b `max_m` anchor → r25-mb_009 anchor
  breaks `max_m<mp` inverse + `max_d` doesn't stack). Close: replace
  `(fold→tuple) + (loop over tuple)` with `(map→Vec<result>)` — the
  intermediate is gone, so per-element bugs can't recur. The unit
  invariant uses the production consumer as oracle (`ffd::simulate`
  over the output asserts `unplaced.is_empty()`), NOT a hand-stated
  predicate: r25's plan-validation proposed `∀i∃k:out[k]≥footprint(i)`
  AND a sort-desc body; the FFD-oracle proptest refuted both before
  ship (the predicate permits same-k for multiple i; sort-desc fails
  because production FFD sorts by *cores*, so a mem-outlier with low
  cores lands last on a core-exhausted bin).
- **Delete the reimplementation** (r26 mb_002 → `cover::sizing` STRIKE-4):
  by N=4 the abstraction boundary is wrong. r25's STRIKE-3 close put
  `sizing` behind an FFD-oracle TEST and an `ffd_packs` IMPL predicate
  that reimplemented `simulate`'s sort+score; the reimplementation
  dropped the `ready` axis. The oracle proptest hardcoded
  `ready=Some(true)` so it never saw the divergence. Close: when the
  test oracle IS a production fn, the impl predicate IS that fn —
  delete the reimplementation and call the production code path
  directly (`ffd::sim_packs` builds synthetic LiveNodes and calls
  `ffd::simulate`). There is no second code path to diverge. The
  proptest must vary every input the production fn's behavior depends
  on (here: `ready`, `intent_id` tiebreak — see §Stability-tests
  "oracle proptest must vary every behavior-relevant input").
- **Function becomes total** (r27 mb_006 → `cover::sizing` STRIKE-5):
  by N=5 the recurring failure is the PRECONDITION ("every input ≤
  cap"), not the body — different upstream paths (`--cores` override →
  `fallback_cell` arch-only, `--capacity` `all_candidates` un-re-
  filtered) keep delivering inputs that violate it under different caps
  (r26's per-cell `min(class,global)` made `cap` smaller than the
  `debug_assert` r25's A1 had calibrated for the global). Close: stop
  relying on the precondition. The function partitions its input
  (`fits` vs `over`), emits a metric+warn for `over`
  (`intent_dropped_total{reason=exceeds_cell_cap}`), and operates on
  `fits` only — total, never panics, and the "n=|u| packs" invariant
  holds over `fits`. NOT a clamp: clamping the bin to `cap` while the
  intent's pod still requests `>cap` mints a NodeClaim the pod can
  never schedule onto — the validation's inverse-checker caught that
  loop. Upstream still SHOULD deliver the invariant (per-cell ceiling
  filter on producer paths) for correctness-of-intent; the function no
  longer requires it for correctness-of-output.

A new review rule MAY accompany the type-check (so first-strike on a
*different* invariant is caught) — but the type-check is the close,
not the rule.

## Structural-close-completion

A §Nth-strike structural close is **five parts**: (1) behavior fix;
(2) doc/spec sync; (3) sibling sweep — `rg <pattern>` for the
open-coded shape in the same crate; (4) dead-code sweep — `rg <field>`
for state the close obsoleted; (5) **arm-coverage** — when the close
touches a sum-type's producer or consumer, the falsifying test drives
**every arm** at least once. Done-gate form: `rg '<Variant>'
<test_file>` ≥ 1 per variant. **Every batch directive ends with a
`Done-gate:` line listing the literal `rg` commands** that verify
(3)+(4); reviewer runs them.

**Why:** r7 delivered (1) at 4/4 and (2-4) at 3/4 each. r8 bug_003 =
(2) failure: R7B0 added doc/spec claims the code never delivered. r8
bug_011 = (3) failure: R7B3 §Canonicalize migrated gate_b to typed
parse, missed gate_a in the same file. r8 mb_005 = (4) failure: R7B1
collapsed the read site, left the field declaration + carry-forward +
4 doc comments. r17 bug_001 = (5) failure: R8B1's 4-poll test
exercised `Hit` 4×, `Miss` 0×; the `Miss` arm's `prev_a` commit was
wrong from the commit it shipped in.

When the close adds a canonical `from_X(to_X(v))` pair (§Canonicalize
/ §Model-formula), the round-trip test MUST cover `to_X`'s full output
range — including the `None`/default/sentinel arm. r19 bug_008:
`duration_fit_from_status` documented as "inverse of `status_from_fit`";
`status_from_fit(None, ..)` → `fit_kind=""` → panic. The existing
round-trip test only exercised `Some(&fp)`.

**(2) truth-source is code, not another comment.** When the close is
"doc A contradicts doc B → sync A to B", the verification is: locate
the `r[impl …]` site (or the function body in the crate the claim's
SUBJECT names) and confirm the comment matches what that code DOES —
read-and-compare, not `rg`. When both sides are `.rs`: `//`/`///`
lines are never the truth-source. r21 bug_004: r19 synced
derivation.rs to snapshot.rs's comment; both were wrong —
jobs.rs:868-886 stamps the full list onto
`pod.spec.affinity.nodeAffinity.required…` and never touches
nodeSelector. A doc-to-doc sync propagates the lie.

**Planner-stated invariants are unverified.** A bound/invariant
written into a fix directive (e.g. "SmallVec inline 4 covers
`|tiers|×2`") is a CLAIM until grounded against the producing code.
r21 mb_003: r19 plan directive stated `|A'| ≤ |tiers|×2`;
solve.rs:903-906 builds candidates `for tier { for h { for cap }}`
with per-tier early-return → bound is `|H|×2`. The directive's bound
was copied into mod.rs:312 verbatim. Close: directive bounds cite the
producing line.

**(4) edit-adjacency.** When a close edits file F at line L, the
§SCC(4) `rg <deleted-mechanism>` MUST cover F entire (not just the
edited fn), and at minimum L±50. r21 bug_018: r19 R19B1 edited
jobs.rs:460-465 (A18 note); the stale `pending_intents`/`or_insert`/
`hw_fallback` block at :422-434 — 30 lines above — was untouched. The
done-gate `rg` was scoped to `rio-scheduler/`; the controller-side
comment was invisible.

## Verifier-one-step-removed

A verifier that confirms "the bughunter's claim X is refuted" can miss
"the fix introduces X′ one step removed" — the inverse case, the next
lifecycle phase, the sibling field, the second consumer.

**Why:** r25: 8/12 r24-regressions were behavioral bugs *introduced
by* r24's closes, each verified by a per-batch agent prompted to
refute the original claim. mb_001a "fires once" is the inverse of
"never fires"; mb_009 `max_m<mp` is the inverse of `max_m>mp`; mb_034
retry-reuse is the next-phase of self-evict; bug_039 arch is the
sibling-of capacity-type; bug_033 capacity-reject is the sibling-of
price-return; bug_040 swap-order; bug_029 N+1th writer; bug_031
wrong-predicate. r24's verifiers checked "is the diff what was asked"
and "is the original claim refuted" — neither is "did we just create
the same §-shape one step over".

**Verifier prompt addendum:** after confirming the original claim, ask
"does the fix introduce the SAME REVIEW.md §-pattern at: (a) the
inverse input case, (b) the next lifecycle transition, (c) a sibling
field/consumer of the changed one?" Each (a)/(b)/(c) gets one
walked-through example, not a yes/no. r25's plan-validation added a
dedicated one-step-removed checker alongside arch/correct/complete;
it caught both major design errors before dispatch — `cover::sizing`
core-only `n` over-asks on mem (the inverse of under-asks), and
dropping `MenuNoFit` removes the only per-cell capacity gate (the fix
replaced a bad SOURCE with no gate instead of a configured gate).

**Applies at IMPL-verifier and integration too.** r26: 5/9
r25-regressions are §one-step-removed at IMPL stage — per-batch
verifiers used "refute the original claim" not the inverse-case
checklist (r25-bug_039's `None`-arm was even noted MINOR), and
integration-time amendments (r25-A8 `live.contains` →
r26-bug_024) got no verify at all. Close: (1) per-batch IMPL-verifier
prompts include the (a)/(b)/(c) checklist above; (2) any logic
introduced at integration — conflict resolution that isn't a pure
merge, deferred amendments — is enumerated in the integration report
and gets its own verify-r{N}-integration agent before gate.

**Cross-batch invariant siblings.** r27: 5/5 r26-regressions are
§one-step-removed shapes the IMPL-verifiers (with the (a)/(b)/(c)
checklist) still missed. The pattern: the verifier probed THE FIX's
inverse, not the fix's effect on a SIBLING CLOSE's invariant. r27
mb_006 = r26-bug_020's per-cell cap broke r25-STRIKE-3's
`debug_assert!(max_c≤cap)` (calibrated for the OLD global cap). r27
bug_002 = r26-mb_022 re-keyed `node_of` to `auth_intent` but left
`tenant_of` 8 lines later on the same `running_build` the doc-comment
explains is wrong. Addendum to (c): when a batch changes a SHARED
parameter (a cap, a key-granularity, a gate-semantics), the verifier's
sibling-sweep extends to OTHER closes that documented an invariant on
the OLD parameter — `rg <param>` over `debug_assert`/doc-comments
across the crate, not just the close's own files.

## Partition-single-source

A "list X mirrors list Y so they partition the space" comment over two
open-coded YAML/const lists is a finding. The mirror is doc-stated,
not structural; one side drifts. r21 mb_002: karpenter.yaml `NotIn
[metal,…]` claimed to mirror values.yaml metal-pool `In […]`; both
hand-maintained, both missing `metal-{16,32}xl`, AND a sibling
template-loop 14 lines below had NEITHER. Close: single-source the
list (`values.yaml` key / helm helper / Rust `const`); both sides
reference it; template-side injection by a discriminating field (here:
`nodeClass`) so new entries get the partition structurally.

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

**Per-item Vec → per-group RPC.** When a per-item Vec crosses an RPC
and the consumer treats each entry as a per-group event, dedup at the
SOURCE. r24 mb_076: `health::reap_unhealthy` pushed `cell` once per
ICE'd NodeClaim (per-item); scheduler's `handle_ack_spawned_intents`
looped calling `ice.mark()` per entry (treats as per-cell event). 8
claims ICE → step 0→7 in one tick, skipping the documented `60s→120s→…`
ladder. Close: `BTreeSet` collect at the producer; the consumer's loop
is then per-group by construction.

**Model-key axes in cross-crate consumers.** A function that queries
by a subset of `ModelKey`'s axes (e.g. `(pname, system)` without
`tenant`) and then looks up a per-ModelKey cache is a finding. Close:
the helper takes `&ModelKey` so the call site must bind every axis.
r8 mb_001: gate_b's candidate SQL grouped `(pname, system)`; `sla
status` defaulted `tenant=""`; `cached()` is per-tenant exact-match →
every candidate `has_fit=false` → vacuous PASS on multi-tenant prod.

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

## Mode-invariant

A "mode X ⇒ behavior Y only" doc-contract enforced by the ABSENCE of a
writer is a finding. The contract must be checked at the READ site
(`if mode != X { return invariant_value }`), not by hoping no other
code path writes. r19 bug_034: `Static` = "seeds only" was true only
because `spot_price_poller` didn't run; `CostTable::load` (PG hydrate)
and `persist` (10-min refresh) both ran unconditionally, so a
Spot→Static config switch silently served months-old EMA prices
forever. Close: the type that owns the read knows the mode.

**Lease-transition edges.** Absence-of-a-writer also breaks on the
X→¬X transition. r24 mb_061: `PlaceableGate` "unarmed on standby" was
true for a FRESH standby (initial `None`) but not an EX-LEADER (still
`Some(stale)`); `on_lose()` only emitted a metric. The ex-leader's
un-gated Pool reconciler then ran `reap_excess_pending` against a
frozen set and mass-deleted the new leader's Jobs. Close: the
`on_acquire`/`on_lose` hooks list every piece of in-memory state and
reset/reload each. Done-gate: a test that flips `is_leader()`
true→false→true and asserts each state field is at its
acquire-time/lose-time value.

**Edge-reload latches on Ok only.** The reload pattern is `if
flag.load() { match load() { Ok(s) => { state = s; flag.store(false)
} Err => { warn; /* flag stays set; retry next tick */ } } }` — NOT
`flag.swap(false)` before the load attempt. r25 bug_040: controller
`reload.swap(false)` then `if let Ok` — `Err` is silently dropped,
flag cleared, no retry, and the next tick `persist()`s the stale
standby snapshot over the previous leader's accumulated state. The
same PR's `poller_tick_prelude` (cost.rs) did it correctly with a
regression test; the controller-side mirror 100 lines away did not. On
`Err`, the tick body runs (degraded service beats no service —
`continue` would halt provisioning entirely), but `persist()` is gated
on `!flag.load()` so degraded-mode never overwrites PG. r25 bug_029:
new `cost_table` writer added without joining the `was_leader`
protocol — its writes during the acquire→reload window were clobbered.
Done-gate for new shared-state writers: `rg` the state's edge-reload
doc-comment for the writer enumeration; if the new writer isn't there,
it doesn't participate.

## Override-coherence

**Grep-hook:** a field overwrite on a solve/fit return struct AFTER
the call, where a comment says "solve doesn't see X" / "X is overlaid
here", is a finding. **Why:** dimensions are coupled in this codebase:
`(mem ↔ node_affinity via menu-fit)`, `(cores ↔ tier via c*)`.
Overlaying ONE post-hoc creates an internally-inconsistent intent. r19
bug_033: `forced_mem` overlaid AFTER `solve_full` returned
`node_affinity` menu-checked at fit-mem → pod `requests.memory=200Gi`
with affinity over cells topping at 96Gi. Close: thread the override
INTO the solve, OR route to the override-aware path (the gate).

**F-tail cross-consumer sweep.** When N parallel batches each add an
override field/gate, integration validation includes a cross-consumer
sweep — per-batch tests verify their OWN consumer; the integration
done-gate verifies the OTHERS. r24 mb_053: F2 added `p*_secs`/
`capacity` to `ResolvedTarget`; the new ad-hoc-tier arm in `intent_for`
read them; three sibling consumers (`explain`, SlaPrediction, the
`Some(memo)` capacity arm) open-coding the same ResolvedTarget→tier
projection did not. Close: per-batch verifier `rg <new-field>` over all
consuming crates; the integration done-gate names one VM-test smoke per
affected fixture path.

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

**1-of-N approximation.** Recording `vec.first()` of an N-set into a
slot whose later consumer assumes "the one" is a finding when N>1 is
reachable. The comment will say "the X" — singular — revealing the
|N|=1 mental model. r19 bug_030: `dispatched_cells` stored `cells[0]`;
the pod's affinity is OR-of-N; comment said "the cell this pod was
spawned for". Close: store the full set; the consumer either gates on
`len()==1` or gets the actual element from a precise signal.

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
ties don't re-introduce nondeterminism. When the close adds a
deterministic key to one sort, the §SCC(3) sibling-sweep is `rg
'sort.*_by'` over the same `Vec` — a downstream re-sort with a
non-superset key destroys the upstream's tiebreak. r19 bug_001:
bug_025's forecast sort got `(prio, c*, hash)`; the outer `(ready,
prio)` re-sort 30 lines below discards `(c*, hash)`. Close: the
downstream sort key is a SUPERSET of the upstream's.

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

**Integer-grid alignment is vacuous.** A test of a windowed/stepped
algorithm whose fixture is integer-aligned with the step grid passes
against a degenerate finite-difference. r24 mb_065: `na_lambda(t,
dt=1.0)` was `H(t+1)−H(t)`, non-zero only when an event lands in
`(t,t+1]`; the test's `(21..=30).map(f64::from)` perfectly aligned
every 1s window. With production floats `[25.3, 47.1, 80.9]` every
window starting at `floor=20` was empty → `consolidate_after = floor`
regardless of arrival rate. Close: stepped-algorithm tests use
non-integer-aligned fixtures; the assertion is `> floor`, not `==
expected`.

**Same-value-to-both-sides is vacuous.** A round-trip test
`assert_eq!(f(x, p), g(x, p))` where the test PASSES the SAME `p` to
both sides cannot detect input-source divergence — it verifies the
formula, not that production callers agree on inputs. r25 mb_035:
`footprint_matches_stamped_requests` called `intent_pod_footprint(&i,
pod::fuse_cache_bytes(&pool))` and compared against the stamped pod
(which also reads `pool`). Production FFD reads `cfg.fuse_cache_bytes`
from controller.toml — a different config path that the test never
touched. Close: the test reads `p` from each side's PRODUCTION source;
if those sources can't be exercised in a unit test, that IS the signal
to single-source.

**Oracle proptest must vary every behavior-relevant input.** r26
mb_002: `sizing_random_intents_ffd_oracle` calls production
`ffd::simulate` as oracle but every fixture had `ready=Some(true)` —
the one input axis the impl-under-test diverged on. An oracle proptest
is vacuous on any input the generator doesn't vary. Done-gate: for
each input field of the oracle fn that affects its output, the
generator varies it; the test's doc lists which fields are
deliberately fixed and why.

**Oracle ≠ impl-predicate.** r27 bug_001: r26's STRIKE-4 close routed
the test oracle through `sim_packs` — the IMPL predicate. With
unconstrained budget `sizing()` returns at the first `n` where
`sim_packs(.., n)==true`, then `oracle_places_all` re-runs `sim_packs`
on the same `(u, n)`: `f(x)==f(x)`. After "delete the
reimplementation; production fn IS impl" the remaining degree of
freedom is the synthetic ENV the predicate builds (LiveNodes, sketch
state, eta-neutralization). Close: the oracle constructs that env
INDEPENDENTLY (different code path, same intent — `ffd::tests::node`
+ direct `simulate`, not `sim_packs`). A regression in either env
construction is then detectable by the other.

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

## Simulator-shares-accounting

A simulator that claims to "predict what scheduler X will do" MUST
share X's accounting code, not reimplement it. r24 Cluster B:
`ffd::simulate` (mod doc: "predicts what the real scheduler will do")
fit-checked raw `i.disk_bytes` while `apply_intent_resources` stamped
`pod_ephemeral_request(disk_bytes, headroom, fuse_cache)` — a unit
mismatch across one normalization boundary. Four divergences in one
round (raw disk, terminal-phase pods counted, intent's own bound pod
blocks self, Fetcher gated on builder capacity) all stem from "FFD
reimplements the pod-request triple". Close: extract
`intent_pod_footprint(i) -> (cores, mem, ephemeral)`; FFD,
`apply_intent_resources`, and `cover_deficit` all call it. Done-gate:
`rg '<raw-field>' <simulator-file>` = 0 (simulator never reads raw)
PLUS a round-trip test `parse(stamp(i)) == footprint(i)`.

**A free parameter defeats the share.** Extracting `f(x, p)` where
production callers pass DIFFERENT `p` is the same divergence with
extra ceremony. r25 mb_035: `intent_pod_footprint(i, fuse)` was the
r24 close above; `apply_intent_resources` passed
`pod::fuse_cache_bytes(pool)` (per-Pool CRD), FFD/cover passed
`cfg.fuse_cache_bytes` (controller.toml scalar). The chart supports
per-pool overrides; controller.toml doesn't see them. The
`footprint_matches_stamped_requests` round-trip test passed because it
substituted the SAME `fuse` into both sides — see §Stability-tests.
Close: either `f(x)` (parameter resolved inside from a single source),
OR a boot-time assertion that all production sources of `p` agree.

## Deletion-field-enumeration

When deleting a template/loop that produced N output fields, the
replacement carries an explicit checklist of all N. r24 mb_002: B3
deleted the band-loop NodePool template (stamped: labels, node-role,
hw-band/storage, **nodeClassRef-per-storage**, **taints**,
requirements, instance-size partition). `cover::build_nodeclaim`
carried 5 of 7; nodeClass and taints dropped. The replacement's
doc-comment enumerated what WAS carried, not what the deleted template
HAD — so the gap was invisible at review. Close: before deleting,
`rg --context 3 <loop-body>` and list every output field; the
replacement's doc-comment is THAT list with ✓/✗ per field. Done-gate:
a unit test asserts every ✓ field on the replacement's output.

## Threat-model surface review

Any new field crossing the worker→scheduler (or builder→scheduler)
boundary MUST be checked against the "worker is NOT trusted" model
BEFORE landing — specifically: is the field's value used in (a)
authorization, (b) cross-tenant resource decisions, (c) destructive
cluster actions? If yes to any, the value MUST come from the
controller (kube-sourced) or be in the HMAC-signed `ExecutorClaims`,
NOT from the worker's request payload.

**Why:** r25 bug_021: R24B6b added `HeartbeatRequest.node_name` and
`detect_hung_nodes` grouped by it → drives NodeClaim deletion
(cross-tenant + destructive). Two colluding tenants could forge
`node_name=<victim>` on ≥4 executors then go silent, causing the
controller to delete the victim's NodeClaim and drain other tenants'
in-flight builds. Neither implementer nor verifier checked the field
against `ExecutorClaims`' "worker is NOT trusted" doc 50 lines away.

**Close:** new worker-supplied fields default to "untrusted; route from
controller". The controller already has the kube-authoritative
`intent_id → spec.nodeName` binding via `PodRequestedCache`; ship that
in `AckSpawnedIntents` instead of trusting the worker. Done-gate: `rg
<new-field> rio-auth/src/hmac.rs` — if it's not in claims, the
reviewer asks "what destructive action reads this?"

**Indirection key matters too.** r26 mb_022: R25B1 made the
`authoritative_binding` map's VALUE controller-authoritative, but
`node_of(running_build)` keyed the lookup on a worker-mutable field —
`reconcile_running_build`'s `(None, Some(hb))` arm sets
`running_build` from the heartbeat unconditionally. A
controller-authoritative `map[K]=V` is only as trusted as `K`. For
destructive cross-tenant actions, the lookup key must be HMAC-attested
(`auth_intent`, set once from token, never mutated by heartbeat) or
kube-sourced.

**Enumerate every field read at the boundary.** r27 bug_002: second
consecutive round the indirection-key shape hit `detect_hung_nodes`
(r26 `node_of`, r27 the sibling `tenant_of` 8 lines below the doc that
explains why `running_build` is wrong). Hardening one read of an
untrusted field is not the close; the close is that EVERY read in the
boundary fn's body is enumerated as either (a) attested/authoritative
key, or (b) opt-in-only — forging the value increases scrutiny on the
forger (`.is_none()` gate, presence-check). Done-gate: `rg
'<untrusted-field>' <fn-body>` with each match annotated. r27's:
`running_build` in `detect_hung_nodes` matches only the `.is_none()`
idle gate.

## Simplex-bound

A "worst-case across all pnames" denorm using `f(UNIFORM)` where `f`
is linear in `α ∈ Δ^{K-1}` is a finding — `min_α f(α)` is at a vertex,
not the centroid. `min_α dot(α, v) = min_d v[d]`; `max_α dot(α, v) =
max_d v[d]`. (Generalizes — non-normative: min of concave / max of
convex over Δ is at a vertex.)

**Why:** r8 bug_012: `housekeeping::tick_scan_dag`'s backstop denormed
ref-seconds via `est / min_factor(UNIFORM)`. `dot(α, f[h])` is linear
in α, so `min_factor(UNIFORM) ≥ min_h min_d f[h][d]` — under-budgets
vertex-α builds on anisotropic hw by up to `mean(f)/min(f)` →
premature cancel → poison loop.

**Structural close:** add `min_factor_any_alpha() = min_h min_d
f[h][d]` (the true simplex-min, hoisted constant) and call it. The
per-key `min_factor(f.alpha)` is correct as-is — it has the actual α;
only the "any pname" bound needs the vertex.
