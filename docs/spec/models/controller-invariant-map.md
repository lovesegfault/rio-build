# Controller reconcile-protocol invariant ↔ spec-rule map

Working artifact for the controller-formal campaign's Phase 0, Stage A
(spec audit). Maps the design's five invariant families (F1–F5) over the
two modeled reconcile loops — the L1 Pool reconciler's same-tick coherence
protocol and the L10 NodeClaim-pool reconciler's mirror lifecycle — onto
the `ctrl.*` rule set in `docs/spec/components/controller.typ`,
cross-referenced against the per-tick decision sites and the lease/⊥ edges
the protocol inventory catalogs. The evidence base is the controller
protocol inventory (`controller-inventory.md`, summarized in the
controller-formal design §2–§3); the executable counterpart of this map is
the pair of Stage-B models (`spawnCoherence.qnt` for L1,
`nodeclaimLifecycle.qnt` for L10) which do not exist yet — Stage A is
deliberately model-free.

This is the post-audit state: every Stage-A invariant maps onto at least
one rule whose normative sentence states it (the GAP rows below were
closed by new `#r()` rules whose bodies are the design's invariant
statements, including the pre-committed verbatim adoption of the
nodeclaim_pool per-field lease-edge polarity table), every place the code
does not do what a rule says it MUST is a CONTRADICTION row (recorded with
its adjudication; no production code was changed in this audit), and the
map closes with the pre-registered expected-as-built-falsifications list
that Stage B's exit gate is judged against.

Two existing rules were amended and version-bumped in this audit
(`ctrl.nodeclaim.placeable-gate+5`, `ctrl.nodeclaim.budget.per-class+2`);
their previously-existing `r[impl]`/`r[verify]` annotations were re-pointed
in the same change after verifying the code already implements the added
clauses — the amendments make deliberate as-built behavior normative, they
do not change behavior.

## The decision sites (the columns of every row below)

The unit of action is one tick. Model J (L1) and Model N (L10) each get a
tick body; the environment owns everything between ticks plus the
per-read snapshot staleness inside a tick.

### L1 Pool reconciler — `reconcilers/pool/{jobs,job}.rs`, one tick per Pool

| # | Step | Where |
|---|---|---|
| J1 | `GetSpawnIntents` poll (filtered); failure ⇒ `intents=[]`, `scheduler_err=Some` | `jobs.rs::reconcile` → `queued_for_pool` |
| J2 | Placeable-gate retain (3-valued: crd-absent / present-unarmed / armed) × pool kind | `jobs.rs::reconcile`, `nodeclaim_pool::PlaceableGate::retain` |
| J3 | Job LIST by pool label + census (after J1 — the I-183 ordering) | `jobs.rs::reconcile`, `job.rs::job_census` |
| J4 | `reap_stale_for_intents`: terminal-collision, selector-drift, orphan-pending arms | `jobs.rs::reap_stale_for_intents` |
| J5 | Headroom arithmetic (freed slots subtracted before the clamp) + existing-name skip set | `job.rs::JobCensus::headroom`, `jobs.rs::reconcile` |
| J6 | `MintExecutorTokens` for the to-spawn slice | `jobs.rs::reconcile` |
| J7 | `spawn_for_each` / `try_spawn_job` (deterministic name, 409 = dedupe) | `job.rs::spawn_for_each`, `job.rs::try_spawn_job`, `pod.rs::job_name` |
| J8 | `AckSpawnedIntents{spawned}` (spawned-this-tick ∪ already-Pending, minus reaped) | `jobs.rs::reconcile` |
| J9 | `reap_excess_pending` (grace, live pod-phase re-check, foreground delete; skipped when `queued_known=None`) | `job.rs::reap_excess_pending` |
| J10 | `reap_orphan_running` behind the 3-arm `orphan_reap_gate` over `ListExecutors` | `job.rs::reap_orphan_running`, `job.rs::orphan_reap_gate` |
| J11 | Termination reports (pod reasons; Job DeadlineExceeded) | `job.rs::report_terminated_pods`, `job.rs::report_deadline_exceeded_jobs` |
| J12 | Status patch (`SchedulerUnreachable` condition) | `job.rs::patch_job_pool_status` |

### L10 NodeClaim-pool reconciler — `reconcilers/nodeclaim_pool/`, one tick

| # | Step | Where |
|---|---|---|
| N1 | `GetSpawnIntents` poll (unfiltered); ⊥ increments the streak counter, ≥5 switches mode | `mod.rs::reconcile_once` |
| N2 | Pool-coverage filter (fail-open on Pool LIST error) | `mod.rs::pool_covers` + the retain in `reconcile_once` |
| N3 | NodeClaim LIST (`requested` filled from the pod-requested cache) | `mod.rs::list_live_nodeclaims` |
| N4 | Kube-only observations: idle→busy edges, `observe_registered` boot samples (recency-gated) | `mod.rs::kube_only_observations`, `sketch.rs::observe_registered`, `consolidate.rs::observe_idle_to_busy` |
| N5 | FFD simulate (placeable / unplaced) | `ffd.rs::simulate` |
| N6 | Publish the placeable set (Registered-placed intent ids only) | `mod.rs::reconcile_once` (the `placeable_tx.send_replace` site) |
| N7 | `health::reap_unhealthy` (incl. `dead_nodes`, capped) + reap-name removal from `inflight_created` + `health::detect_vanished` (ICE) | `health.rs`, `mod.rs::reconcile_once` |
| N8 | `cover_deficit` (per-class/global budgets, per-tick cap, round-robin, masked cells skipped) | `mod.rs::cover_deficit`, `cover.rs::class_budget` |
| N9 | `report_unfulfillable` (`unfulfillable_cells`/`registered_cells`/`observed_instance_types`/`bound_intents`, deduped) | `mod.rs::report_unfulfillable` |
| N10 | `consolidate::reap_idle` (idle-now ∧ not reserved ∧ not terminating ∧ past threshold) | `consolidate.rs::reap_idle` |
| N11 | Sketch persist to PG (gated by the reload latch) | `sketch.rs`, `mod.rs::reconcile_once` |

### Edges (the environment's actions between ticks plus the mode switches)

| # | Edge | Where |
|---|---|---|
| E-acq | Lease acquire: unconditional `prev_idle` clear, PG reload latch, Ok-arm clears (`recorded_boot`, `inflight_created`), the per-field polarity table | `mod.rs::run` (the `reload_pending` block) |
| E-loss | Lease loss: gate unarm before the next consumer tick | `mod.rs::run` (the `hooks.lose` branch) |
| E-bot | ⊥-streak ≥ 5: switch to `consolidate_only` (kube-only reads, reap + prune, no create / republish / ack) | `mod.rs::reconcile_once` → `consolidate_only` |
| E-restart | Process restart: all in-memory state to defaults, gate unarmed until the first FFD tick | `mod.rs::NodeClaimPoolReconciler::new`, `placeable_channel` |

## The invariant ↔ rule map

Verdict legend: **COVERS** — the rule's normative sentence states the
invariant (or the load-bearing piece of it). **PARTIAL** — the rule states
a piece; the missing piece is named. **GAP** — no rule stated it; closed
by a new `#r()` rule in this audit. **CONTRADICTION** — the code does not
do what the rule says it MUST; recorded in the contradiction table below.

### F1 — Same-tick coherence (Model J)

#### `SingleJobPerIntent` (I1)
*Per pool, at most one non-terminal Job per intent; respawn is idempotent.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `ctrl.pool.spawn-once` *(new)* | **COVERS** | Was a GAP. Deterministic name = f(pool, kind, intent_id); 409 is dedupe (skip, no ack), never a rename; create-once semantics; a spawn error never aborts the tick. The mechanism existed only as code comments (`SpawnOutcome::NameCollision`, `try_spawn_job`) and the G-A fix history. |
| `ctrl.pool.reconcile` | PARTIAL | States the 1:1 spawn shape ("spawn one Job per intent") but not the identity/dedupe mechanism that makes re-execution safe. |
| — | known bound | Cross-pool duplicates for one intent (two Pools with overlapping kind/systems/features) are possible and out of model (1 pool); bounded by the excess reap. Recorded here, not proven. |

#### `CeilingRespected` (I4)
*Post-spawn active-minus-freed never exceeds the ceiling; spawns this tick ≤ headroom.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `ctrl.pool.ephemeral+1` | **COVERS** (the ceiling) | "active Jobs < `spec.maxConcurrent` (or unconditionally when unset)"; `maxConcurrent` is a concurrent-Job ceiling, not a standing set. |
| `ctrl.pool.tick-ordering` *(new)* | **COVERS** (the arithmetic) | Freed slots subtracted from active before the clamp so an over-committed pool (operator lowered the ceiling mid-flight) cannot overshoot; reaped names excluded from the skip set so freed slots are spendable the same tick. Previously only the `JobCensus::headroom` doc comment. |

#### `ReapSafety` (I3)
*No excess- or orphan-reap deletes a Job whose pod is doing work.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `ctrl.ephemeral.reap-excess-pending+3` | **COVERS** (the Pending half) | `ready == 0`, creation-age grace, and the live (non-informer) pod-phase re-check before every DELETE. Also carries its own fail-closed posture for a failed poll. |
| `ctrl.ephemeral.reap-orphan-running+3` | **COVERS** (the Running half) | Absent-or-idle executor under a trusted listing (3-arm gate: error / young leader / empty list ⇒ no reap), 5-minute grace exceeding the worker idle-exit. |
| — | documented bound | The ground-truth residual: a pod that is busy but never registered (or whose registration the scheduler lost) past the orphan grace is reapable — the listing is the best available authority. Stays a documented bound of the checked property, not folded into it. |

#### `OrphanBounded` (I2, safety form)
*An abandoned Job (intent gone / executor gone) is deleted by the end of the next eligible tick; everything else is bounded by deadline or TTL.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `ctrl.ephemeral.reap-excess-pending+3` | **COVERS** (orphan-by-intent + residual excess) | The orphan-by-intent-first then oldest-first residual order is normative. |
| `ctrl.ephemeral.reap-orphan-running+3` | **COVERS** (Running orphans) | |
| `ctrl.ephemeral.intent-deadline` | **COVERS** (the deadline backstop) | `activeDeadlineSeconds` from the intent, floored at 180. |
| `ctrl.pool.ephemeral+1` | **COVERS** (terminal TTL) | `ttlSecondsAfterFinished: 600`. |
| `ctrl.pool.tick-ordering` *(new)* | PARTIAL (the stale-intent arms) | Mandates running `reap_stale_for_intents` over the full intent set before spawn, and `ctrl.pool.spawn-once` names the terminal-collision arm as the respawn unblock; the selector-drift arm's trigger (the `rio.build/intent-selector` fingerprint vs the scheduler's re-solve) is still code-defined detail with no rule of its own. Named gap, deliberately left for Stage B to decide whether the model needs it stated normatively (Model J carries the fingerprint as state). |

#### `TickOrdering` (the I-183 family)
*Poll before LIST; reap before spawn with reaped names excluded and freed slots usable; destructive arms act on this tick's snapshots.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `ctrl.pool.tick-ordering` *(new)* | **COVERS** | Was a GAP — the inventory's ordering items 1, 2 and the live re-check relation. In Stage B these become constraints relating which per-read snapshots later reads may see given earlier ones. |

### F2 — Failure polarity (Models J + N)

#### `DegradedTickIsSafe`
*For every combination of ⊥ inputs, the destructive/arming effects taken this tick are a subset of the per-consumer degradation matrix.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `ctrl.pool.degraded-polarity` *(new)* | **COVERS** (the matrix as a whole) | Was a GAP: each mechanism rule carried its own half but nothing stated the per-consumer (not per-RPC) principle, the Pool-coverage fail-open, the unknown-cell drop, or the 5-⊥-tick mode switch as one degradation contract. Cross-references the per-mechanism rules rather than restating them. |
| `ctrl.ephemeral.reap-excess-pending+3` | **COVERS** (its half) | Spawn fail-open / reap fail-closed for a failed poll was already normative here. |
| `ctrl.ephemeral.reap-orphan-running+3` | **COVERS** (its half) | The 3-arm fail-closed gate was already normative. |
| `ctrl.nodeclaim.anchor-bulk+5` | **COVERS** (cover's half) | `cover_deficit` skips the tick when the global ceiling is not yet loaded (fail-closed, ≤300s self-heal). |
| `ctrl.nodeclaim.consolidate-only-degraded` *(new)* | **COVERS** (the degraded mode) | Was a GAP — see F4. |
| `ctrl.pool.hw-bench-needed+2` | **COVERS** (bench gate) | RPC failure reads as 0 distinct tenants: over-bench, never under-bench. Out of model (G-D family) but listed because it is part of the same per-consumer matrix. |

#### `GateFailClosed` (I6, consumer half, split per gate configuration)

| Rule | Verdict | Audit finding |
|---|---|---|
| `ctrl.nodeclaim.placeable-gate+5` *(amended)* | **COVERS** (all four configurations) | The +4 text covered: armed ⇒ Builder spawns only inside the set; present-unarmed ⇒ fail-closed both ways; crd-absent ⇒ "gate is pass-through". The audit found two configurations where the code deliberately deviates from a strict reading of the pre-audit text (contradictions C1 and C2 below) and one unstated producer-side obligation the Stage-B J↔N assume-guarantee needs (content from the last successful FFD tick; no republish on ⊥/consolidate-only ticks; unarm on lease loss before the next consumer tick). The +5 amendment states all of it; the previously-existing impl/verify markers were re-pointed and a new impl marker was added at the lease-loss unarm site. |

### F3 — Ack/ICE protocol soundness (Models J + N, scheduler peer state)

#### `AckSoundness` (I5)
*An intent is acked `spawned` at t only if a Job for it exists at t; every `dispatched_cells` entry has a Job behind it.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `ctrl.pool.ack-spawned-soundness` *(new)* | **COVERS** | Was a GAP (the inventory's named spec hole). The chain-`spawned`-not-`to_spawn` discipline, the already-Pending re-ack (scheduler restart re-arm), and the reaped-name exclusion existed only as code comments and the G-B fix lineage (`cdc78f839`, `5815a7544`). |

#### `IceMarkSignalSoundness` (I8, controller half)

| Rule | Verdict | Audit finding |
|---|---|---|
| `ctrl.nodeclaim.ice-mark-clear` *(new)* | **COVERS** (the controller half) | Was a GAP. Mark dedup per cell per tick; marks only for launch-failed / timed-out-unregistered / vanished claims, never for the controller's own reaps (ties to F4 conservation); clears only for recency-gated Registered edges; mark-before-cover in the same tick. The clear-side ladder, heartbeat clear, and TTL are scheduler-side and carried by the cross-reference to `sched.sla.hw-class.ice-mask`, not restated. |
| `ctrl.nodeclaim.inflight-conservation` *(new)* | **COVERS** (the no-mark-for-own-reap precondition) | The reap-names-before-detect ordering is what makes the mark sound. |

#### `NoMassClearAfterFailover`
*An acquire does not re-emit Registered clears (or boot samples) for old registrations; a fresh edge after the acquire still emits.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `ctrl.nodeclaim.ice-mark-clear` *(new)* | **COVERS** | The recency-gate clause is stated against exactly the restart/acquire mass-clear scenario. |
| `ctrl.nodeclaim.lease-edge-polarity` *(new)* | **COVERS** (the enabling half) | `recorded_boot` is suppress-class and IS cleared on acquire — the recency gate is the only thing standing between that clear and a mass re-edge; the two rules are deliberately coupled. |

### F4 — Mirror lifecycle / lease-edge polarity (Model N)

#### `PolarityRespected` (per field) and `ReloadLatch`

| Rule | Verdict | Audit finding |
|---|---|---|
| `ctrl.nodeclaim.lease-edge-polarity` *(new)* | **COVERS** | Was a GAP — the design's pre-committed adoption of the per-field polarity table as normative text, transferred 1:1 from the code comment in `mod.rs::run`'s reload block: suppress (`recorded_boot`, `inflight_created` — cleared in the Ok arm), amplify (`prev_idle` — cleared unconditionally before the reload attempt; consequence: the idle basis is never earlier than the current tenure's first observation), cleanup-pending (`prev_extra_cells`, `prev_unplaced_extras` — never cleared), reload-latch (`sketches` — persist gated while pending, latch clears on Ok only), plus the discipline sentence for new fields and the loss-edge/standby clause. The code comment is left in place verbatim for now (this audit adds only the `r[impl]` marker); shrinking it to a pointer at the rule is deferred to the next substantive edit of that block so Stage A stays marker-only in Rust. |
| `ctrl.nodeclaim.lead-time-ddsketch` | PARTIAL (sketch persistence) | States the sketch content/PG format; the acquire-edge reload latch and the persist gate were unstated — now in the polarity rule's reload-latch class. |

#### `IdleBasisCurrentTenure` (I9's basis clause) and `IdleReapCorrectness` (I9)

| Rule | Verdict | Audit finding |
|---|---|---|
| `ctrl.nodeclaim.lease-edge-polarity` *(new)* | **COVERS** (the basis clause) | The amplify row's consequence sentence is the basis clause. The design's conditional amendment of `ctrl.nodeclaim.consolidate-na+6` for this clause was judged unnecessary: the clause is a lease-edge property and lives in the lease-edge rule; consolidate-na stays the NA-threshold authority. Recorded as a Stage-A decision (not an oversight). |
| `ctrl.nodeclaim.consolidate-na+6` | **COVERS** (the keep-while model) / PARTIAL (reap preconditions) | The NA break-even formula, the fitting-core term, and the floor are normative. The reap's structural preconditions (idle-now ∧ not FFD-reserved ∧ not terminating) are code-defined (`consolidate.rs::reap_idle`) and exercised by its unit tests but stated in no rule; named here, deliberately not closed — Stage B decides whether the model's `IdleReapCorrectness` encoding needs them spec-stated or whether the existing rule plus the polarity rule suffice. |

#### `NoSpuriousVanish` / `NodeClaimConservation` (I7)

| Rule | Verdict | Audit finding |
|---|---|---|
| `ctrl.nodeclaim.inflight-conservation` *(new)* | **COVERS** | Was a GAP. The four-mutator discipline (extend on create, clear on config reload, `detect_vanished` retain rules including the consolidate-only prune, reap-name removal BEFORE detect in both modes) plus the exactly-one-outcome conservation statement. Previously a field doc comment (`mod.rs`, the `inflight_created` mutator list) and two bug-round fixes. |

#### `BootRecordedOnce`

| Rule | Verdict | Audit finding |
|---|---|---|
| `ctrl.nodeclaim.lease-edge-polarity` *(new)* | **COVERS** (the dedup half) | `recorded_boot` suppress-class semantics. |
| `ctrl.nodeclaim.ice-mark-clear` *(new)* | **COVERS** (the stale-registration half) | Stale registrations are recorded without emitting a clear. |
| — | as-built deviation | The "no sample lost solely due to scheduler unreachability" half is not fully met by the as-built code: ⊥ ticks before the consolidate-only switch early-return without running the kube-only observation block (documented TODO in `reconcile_once`'s ⊥ arm), so a Registered edge inside that ≤4-tick window is lost. Pre-registered as an expected Stage-B falsification (below), not silently modeled around. |

#### `SingleEffectiveProvisioner` (I11)

| Rule | Verdict | Audit finding |
|---|---|---|
| `ctrl.nodeclaim.lease-edge-polarity` *(new)* | **COVERS** (the controller-side discipline) | The loss-edge clause: unarm the gate, and take no create/delete/ack/publish effect while not leader. |
| `ctrl.nodeclaim.placeable-gate+5` *(amended)* | **COVERS** (the unarm-before-next-consumer-tick obligation) | |
| rio-lease guarantees (imported) | out of scope here | At-most-one-believer and the observation bound are the lease campaign's verified properties; both models import them as an assume-guarantee checklist, never re-modeled. |

### F5 — Budget conservation (Model N)

#### `ProvisioningBudget` (I10)

| Rule | Verdict | Audit finding |
|---|---|---|
| `ctrl.nodeclaim.budget.per-class+2` *(amended)* | **COVERS** (per-class clamp + failed creates) | The pre-bump text stated the per-hwClass clamp and the global fallback; "failed creates consume no budget" existed only as the create-loop comment (the r40 budget fix). The +2 amendment states it; the existing impl/verify markers were re-pointed and a marker added at the Ok-arm-only accounting site. |
| `ctrl.nodeclaim.anchor-bulk+5` | **COVERS** (per-tick cap, global budget, rotation, ceilings fail-closed) | Already normative. |
| `ctrl.nodeclaim.ffd-exclude-terminating` | **COVERS** (terminating still counted) | Already normative: terminating claims are excluded from placement but still consume fleet-core budget. |

## Contradiction records

The code does not do what the rule (as written before this audit) says it
MUST. Recorded with the adjudication; production code is unchanged in
Phase 0. Both rows below were adjudicated spec-side — the design
pre-registers the as-built behavior as the intended one (F2's
GateFailClosed split), so the resolution is the `+5` amendment landed in
this same audit rather than a code fix or a carried-open row. They are
recorded here so the adjudication is on the record, not silent.

| # | Rule (pre-audit text) | What the rule said | What the code does | Adjudication |
|---|---|---|---|---|
| C1 | `ctrl.ephemeral.reap-excess-pending+3` (its unconditional MUST-delete) read together with `ctrl.nodeclaim.placeable-gate+4`'s "CRD absent ⇒ gate is pass-through" | When queued drops below Pending, the controller MUST delete the excess; the only stated skip was a failed queued poll. Under the +4 "pass-through" wording a CRD-absent cluster has no gate-derived skip, so the MUST applies whenever the scheduler is reachable. | In a CRD-absent cluster (static-node / k3s without Karpenter) `gate_armed=false` keeps `queued_known=None` every tick, so `reap_excess_pending` NEVER runs — excess Pending Jobs from a cancelled build sit until `activeDeadlineSeconds`. Deliberate: an ungated `queued` count is not safe to reap against (the post-completion Job-status lag / job-tracking-finalizer race), accepted when the CRD-absent arm was added to the gate. | Code is correct (fail-closed beats racing the finalizer); the spec was incomplete. `ctrl.nodeclaim.placeable-gate+5` now states the crd-absent posture explicitly (spawn fail-open, excess-reap fail-closed), which the excess-pending rule's skip set composes with by cross-reference. The operational cost in static-node clusters (Pending Jobs linger up to the deadline after a cancel) is now a documented consequence, not an accident. |
| C2 | `ctrl.nodeclaim.placeable-gate+4` ("An unarmed gate (no FFD tick yet) is fail-closed for both spawn and reap") | Unqualified: any pool kind, any unarmed gate ⇒ no excess-reap. | A Fetcher pool with the CRD present never consults the gate's armed state: its excess reap is keyed only on scheduler reachability, including before the first FFD publish. Sound — a Fetcher pool's `queued` is the raw scheduler count and needs no FFD gate to be authoritative. | Code is correct; the +4 sentence over-reached. `+5` scopes the unarmed-fail-closed sentence to Builder pools and states the Fetcher posture. |

Near-misses recorded for a later prose pass but not classified as
contradictions of a MUST:

- `ctrl.pool.ephemeral+1`'s opening mechanism sentence ("polls
  `AdminService.ClusterStatus` … spawns K8s Jobs when
  `queued_derivations > 0`") describes the pre-intent spawn loop; the
  as-built tick polls `GetSpawnIntents` and spawns one Job per intent
  (`ctrl.pool.reconcile` is the accurate statement, and the rest of the
  ephemeral rule — one-shot Job settings, TTL, ceiling semantics, fanout
  bounds — matches the code). Phase-1 prose amendment candidate; not
  bumped here because no Stage-B invariant is encoded from that sentence.
- The prose paragraph following it ("the active mechanism is ClusterStatus
  polling") has the same staleness; it is narrative text outside any rule
  body.

## Expected as-built falsifications (pre-registered)

A Stage-B model run that falsifies one of these is confirming a documented
as-built deviation; a falsification NOT on this list is a stop-and-report
(model-encoding bug or new defect — triaged before Stage C starts).

1. **The ⊥-tick early-return skip** (documented TODO in
   `mod.rs::reconcile_once`'s ⊥ arm): ticks 1..4 of a ⊥ streak return
   before the kube-only observation block, so `prev_idle` is not pruned on
   idle→busy edges and `observe_registered` samples in that window are
   lost (bounded by the streak ceiling: ≤ 4 ticks ≈ 40 s).
   - Expected to falsify, in the `fault-rpc` regime:
     `IdleReapCorrectness` / `IdleBasisCurrentTenure` (a `prev_idle` entry
     conflates two idle spells across an unobserved busy period — the
     falsifying trace needs the stale entry already near the threshold,
     since the 40 s skew alone is below every configured floor), and
     `BootRecordedOnce`'s no-loss-under-unreachability clause (a
     Registered edge inside the window is never recorded and never emits
     its clear).
   - If Stage B's bucketed idle-age cannot express the near-threshold
     composition for the first half, that half is recorded as a
     NOT-ENCODED bound in the model header instead — this is the only
     entry on this list permitted to downgrade rather than reproduce.

No other as-built falsification is expected: C1 and C2 are spec-text
findings whose adjudication keeps the code as-is, so the as-built model is
expected to satisfy every F1–F5 invariant in `base` and in every fault
regime except the entry above. An empty remainder is itself the claim
Stage B tests.

## Out-of-model invariants (recorded so the omission is deliberate)

- **I12 termination-report idempotence** — scheduler-side dedup
  (`recently_disconnected`); imported as an assumption from the retry
  campaign's model, which already covers the controller's
  re-report-within-TTL behavior as environment input. Controller half
  stays `ctrl.terminated.deadline-exceeded+2` + the existing tests.
- **I8's clear/TTL half** — the backoff ladder, the single-admissible-cell
  heartbeat clear, TTL expiry: scheduler-side state and code, carried by
  `sched.sla.hw-class.ice-mask` and the scheduler's own tests; Models J/N
  only treat "masked / not masked" as peer state.
- **I13 scaler self-trigger** and the whole ComponentScaler loop (L2) —
  out of scope for this campaign.
- **I14 idle-pod latency bound** — arithmetic over constants; stays spec
  prose + VM tests.
- **The `dead_nodes` consequence chain** (bound-intent misattribution →
  hung-node echo → healthy-claim reap) — consumed input only in Model N;
  its protection is the §4(a)1 gate tests named in the design, not an
  F1–F5 invariant. The model cannot complain about regressions there.
- **FFD / cover sizing arithmetic, pod construction parity, identity
  plumbing** (G-C, G-D, G-F fix families) — unit/VM tests and Kani
  candidates per the design's calibration table; the models consume
  placed/unplaced/cell as opaque outcomes.

## `ctrl.*` rules not load-bearing for any invariant above

Grouped, with the reason they stay outside the Stage-B models:

- **Pod/Job construction and placement derivation** (G-D surface):
  `ctrl.pod.arch-selector+2`, `ctrl.pod.tgps-default`,
  `ctrl.pool.kvm-device+2`, `ctrl.pool.node-affinity-from-intent`,
  `ctrl.pool.intent-tolerations`, `ctrl.pool.fetcher-affinity-from-intent+5`,
  `ctrl.pool.fetcher-tolerations`, `ctrl.pool.builder-tolerations`,
  `ctrl.pool.fetcher-hardening+2`, `ctrl.pool.hw-class-annotation`,
  `ctrl.nodeclaim.taints.hwclass`, `ctrl.nodeclaim.priority-bucket`,
  `ctrl.nodeclaim.shim-nodepool`, `ctrl.crd.*`, `ctrl.event.spec-degrade` —
  k8s object shape, not protocol state.
- **FFD/sizing internals**: `ctrl.nodeclaim.ffd-sim`,
  `ctrl.nodeclaim.anchor-bulk+5` (its sizing half),
  `ctrl.nodeclaim.lead-time-ddsketch`, `ctrl.nodeclaim.consolidate-na+6`
  (its formula half) — abstracted to opaque outcomes / bucketed
  thresholds; their budget- and polarity-relevant clauses are mapped
  above.
- **Other loops**: `ctrl.scaler.*`, `store.admin.get-load+2` (L2),
  `ctrl.gc.*` (L8), `ctrl.pool.disruption`, `ctrl.drain.*` (L3 /
  executor lifecycle), `ctrl.reconcile.owner-refs`,
  `ctrl.backoff.per-object`, `ctrl.condition.sched-unreachable`
  (observability), `ctrl.probe.named-service`,
  `ctrl.health.ready-gates-connect`, `ctrl.pool.hw-bench-needed+2`
  (listed under F2 only as matrix context), `ctrl.pool.fetcher-spawn-builtin`,
  `ctrl.terminated.deadline-exceeded+2` (the controller half of a
  scheduler-side protocol audited by the retry campaign),
  `ctrl.admin.rpc-timeout` (the 5 s bound is what makes a stalled
  scheduler a per-tick ⊥ rather than a wedged loop — an assumption the
  models inherit, not an invariant they check).

## Verify-marker status (Stage-A snapshot)

The eight new rules (`ctrl.pool.tick-ordering`, `ctrl.pool.degraded-polarity`,
`ctrl.pool.spawn-once`, `ctrl.pool.ack-spawned-soundness`,
`ctrl.nodeclaim.lease-edge-polarity`, `ctrl.nodeclaim.inflight-conservation`,
`ctrl.nodeclaim.ice-mark-clear`, `ctrl.nodeclaim.consolidate-only-degraded`)
carry `r[impl]` markers at the tick-body sites listed in the decision-site
tables and NO `r[verify]` markers yet: their verification is deliberately
deferred to the Stage-B models, whose checks are wired in `nix/quint.nix`
with the marker at the wiring point (the same discipline as the existing
quint checks). Until then they appear in `tracey query untested` — that is
the marker-first signal working as intended, not a debt to silence.

The two amended rules keep their existing verification, re-pointed to the
new versions: `ctrl.nodeclaim.placeable-gate+5` (the gate-retain unit tests
in `pool/jobs.rs` and the kwok forecast-provisioning VM wiring) and
`ctrl.nodeclaim.budget.per-class+2` (the `class_budget` unit test in
`cover.rs`, which verifies the clamp; the new failed-creates clause is
exercised by the cover-loop's accounting and gets its dedicated check in
Stage B or a unit test in Phase 2 — until then the clause is impl-marked
but its specific verification rides on the existing cover tests).
