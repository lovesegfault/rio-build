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

## Phase 0a — churn pin and re-pin protocol

Stage B builds two models of the tick bodies this audit just mapped. The
pin below freezes what "the tick bodies" means, records the in-flight work
adjacent to them, and states when Stage A must be re-validated. Pin date:
2026-05-26.

### What is pinned

Base: the campaign works on the `formal-sprint` lineage; the Stage-A
worktree branched at `1fa6a1c7d` ("docs(rio-scheduler): drop private
intra-doc links in the status-persist seam docs"), and the Stage-A audit
itself landed as `607a93f3f` adding only spec text, this map, and
`r[impl]` comment lines — no behavior-relevant change to any modeled file.
The modeled tick bodies as of `1fa6a1c7d`:

| In-scope path | Last commit touching it (at the base tip) | Last `fix(…)` commit |
|---|---|---|
| `rio-controller/src/reconcilers/pool/jobs.rs` | `2fff4e938` 2026-05-24 (fetcher FUSE-cache budget split) | `f97644a53` 2026-05-11 |
| `rio-controller/src/reconcilers/pool/job.rs` | `fba9086dc` 2026-04-23 (live pod-phase check before excess-pending DELETE) | `fba9086dc` 2026-04-23 |
| `rio-controller/src/reconcilers/nodeclaim_pool/` (all of it: `mod`, `ffd`, `cover`, `consolidate`, `health`, `sketch`) | `2fff4e938` 2026-05-24 | `7f91f1892` 2026-05-19 |
| `rio-controller/src/main.rs` (task wiring, gate channel, lease hooks) | `4fa50ea60` 2026-05-22 | `4fa50ea60` 2026-05-22 (acquire-epoch recovery fix, shared with rio-lease/rio-scheduler) |

The gate/ICE peer-state files the models treat as environment/peer
variables (not modeled, but their interfaces are assumed):

| Peer path | Last commit at the base tip |
|---|---|
| `rio-scheduler/src/sla/cost.rs` (the ICE backoff ladder) | `8026d5f2b` 2026-05-15 |
| `rio-scheduler/src/actor/mod.rs` (`dispatched_cells`, `recently_disconnected`, hung-node state) | `1a3e60eaa` 2026-05-25 — the retry campaign's Phase-1a two-installment attempt rows; an active stream, scheduler-side only |
| `rio-scheduler/src/actor/snapshot.rs` (hung-node detector, intent snapshot) | `7f7a19b8a` 2026-05-20 |
| `rio-controller/src/reconcilers/node_informer.rs` (pod-requested cache → `bound_intents`) | `e013b2044` 2026-05-05 |

Spec-rule version pins (the design's named churn check), as of Stage A:
`ctrl.nodeclaim.consolidate-na+6` (unchanged by this audit),
`ctrl.nodeclaim.placeable-gate+5` (was +4),
`ctrl.nodeclaim.budget.per-class+2` (was unversioned), plus the eight new
Stage-A rules at their initial versions. A bump of any of these by another
stream is a Stage-A re-validation trigger (below).

The last-fix dates above are the quiet-window evidence the design asked
Phase 0a to re-check rather than assume: the modeled files have taken no
fix since 2026-05-19 (and `pool/job.rs` none since 2026-04-23); the only
post-r43 change to them is the fetcher-budget feature commit.

### Inventory re-pin

The protocol inventory cites HEAD `e650f23a4`. That commit is not an
ancestor of `1fa6a1c7d` — it sits on the pre-rewrite lineage of the same
branch (merge base `ebb0270eb`, 2026-05-25). Verified for this pin:
`git diff e650f23a4..1fa6a1c7d` is empty for every in-scope controller
path above and for `docs/spec/components/controller.typ`, so the
inventory's content claims and line anchors carry over to the base tip
unchanged. The only diff in the in-scope-plus-peer set is
`rio-scheduler/src/actor/mod.rs` (the retry Phase-1a work named above),
which does not affect the controller tick bodies. The inventory's
references should be read as anchored to `1fa6a1c7d` from here on.

### In-flight work adjacent to the modeled files

- **The fetcher-budget stream** — `2fff4e938` (2026-05-24, B. Meurer, the
  branch owner): already landed at the base tip; touches the
  Simulator-shares-accounting chokepoints (`pod.rs::fuse_cache_bytes`
  selection and `jobs.rs::intent_pod_footprint`) and the nodeclaim_pool
  sizing path. No successor commits exist beyond the base tip on any of
  the 167 local branches (checked 2026-05-26). Any follow-up in this
  stream that lands before Stage B starts triggers the re-pin check
  below; ordering with this campaign is otherwise unconstrained because
  the accounting chokepoints are outside the modeled protocol state
  (G-C family, NOT-ENCODED).
- **Branches touching `rio-controller/src/reconcilers/` beyond the base
  tip** (scan of all 167 local branches, 2026-05-26): the mountd stream
  (`3ca8d4848`, `da8044d3f`, `0ee6bf1fc`) and the no-Nix-image stream
  (`4dae589e1`, `1e7ec443a`, `900f6f2af`) touch `pool/pod.rs` (and its
  tests) only — pod construction, outside the modeled tick bodies and
  outside the calibration corpus. A cluster of April-era bugfix-round
  branches (`bugfix-fix`, `bugfix-impl`, `bugfix-residuals`,
  `bugfix-wave*`; tips 2026-04-17..19) still exists locally and shows
  modeled-file commits, but those are pre-rebase duplicates of fixes
  already in the base tip under different hashes (the
  reap-full-intent-set / spawn-iterates-all-intents / re-ack lineage of
  the G-A/G-B corpus) — stale, not in-flight. The only live branch with
  a modeled-file commit beyond the base tip is `retry-phase1b` (next
  bullet). No branch carries behavior-relevant in-flight work on
  `pool/jobs.rs`, `pool/job.rs`, or `nodeclaim_pool/`.
- **Explicitly not started** (and per the design queued behind Phase 1):
  the §13e fetcher-gate follow-up (extending the placeable retain to
  Fetcher pools — would change the F2 GateFailClosed split) and the
  executor-lifecycle candidate (would promote I2/I3/I5 to
  correctness-critical and remove the ack-arming protocol). Neither has a
  branch. If either starts before Phase 1, it gets a named owner and an
  ordering against this campaign at that point, and the affected map
  sections are re-audited.
- **The executor-lifecycle campaign (#1) has started** (2026-05-26,
  Phase 0 on the formal-sprint lineage; owner: B. Meurer — also this
  campaign's owner). This is the named-owner-and-ordering entry the
  bullet above and the re-pin protocol require. Ordering: this map's
  Stage C closed before that campaign began, so the option-(a)
  precondition recorded in its design — controller Stage C closes
  before its Phase 1b touches `pool/{jobs,job}.rs` — is met; recording
  that here is NOT a discharge of this map's own constraints. Because
  that campaign starts while this one has no Phase-1 record, two
  obligations are recorded with the ordering: (i) the affected sections
  of this map (J11, the orphan-reap rows, F1/F3, the I12 out-of-model
  entry) are due a re-audit when that campaign's 1b/1d change the
  behavior they describe — carried as named exit-gate deliverables of
  its 1b and 1d; (ii) if its 0e go/no-go is reached while this campaign
  still has no Phase-1 record/close-out, the re-pin-protocol clause
  below ("executor-lifecycle replacement green-lit mid-campaign")
  applies in full: this map's F1/F3 rows become a prerequisite review
  input to that 0e decision, the Stage-B models are re-checked with the
  heartbeat-authority assumptions removed (carried by that campaign's
  planned 1c'/1d Model J/N checklist re-derivation and check re-runs),
  and this map's Stage-C calibration table gets a delta pass. Tracking
  artifact: `docs/spec/models/executor-invariant-map.md` (its Phase-0a
  cross-campaign section); the OA2 hung-node-successor ask (decision
  due at that campaign's 0e) and the jointly-owned OA6 forecast-spawn
  data query are recorded there.
- **The retry campaign** (the other active campaign on this lineage,
  same owner): its Phase-0 artifacts and Phase-1a ledger work are
  ancestors of the base tip (`99e07563f`, `b5498a904`, `7c3ea5bcf`,
  `7d58c5bdd`, `1a3e60eaa`, …), which is exactly the state this
  campaign's assume-guarantee imports (the `recently_disconnected` dedup
  assumption, the termination-report environment actions). Its Phase-1b
  stream is in flight on the `retry-phase1b` branch (12 commits beyond
  the base tip as of this pin); the only modeled-file touch is one
  comment line in `pool/jobs.rs` (`b9dc13068` re-points a cross-reference
  to the scheduler rule it bumps, `sched.termination.deadline-exceeded`)
  — a non-triggering change under the protocol below. Ordering: it may
  land before or after Stage B with no effect on this audit; if later
  commits in that stream grow a behavior-relevant change to the
  controller's deadline-report path (J11), the re-pin check catches it
  and the affected rows (J11, the I12 out-of-model entry, the
  deadline-exceeded near-miss) are re-audited as a contained delta.

### Re-pin protocol

Run the check below immediately before Stage B starts, and again before
Stage C pins its per-commit calibration corpus:

```
git log --oneline 607a93f3f..HEAD -- \
  rio-controller/src/reconcilers/pool/jobs.rs \
  rio-controller/src/reconcilers/pool/job.rs \
  rio-controller/src/reconcilers/nodeclaim_pool/
```

plus a check that the pinned rule versions above are unchanged
(`grep -o 'placeable-gate+[0-9]*\|consolidate-na+[0-9]*\|budget.per-class+[0-9]*' docs/spec/components/controller.typ | sort -u`).

Stage A MUST be re-validated before Stage B proceeds if any of:

1. any commit in that range changes the modeled files beyond comments,
   doc-comments, or tracey markers (a behavior-relevant change to a tick
   body, however small);
2. any pinned rule version changes (including the eight Stage-A rules);
3. the fetcher-gate extension or the executor-lifecycle replacement
   starts.

Re-validation means: re-run this audit over the changed decision sites
(not the whole map), update the affected verdict rows, contradiction
records, and the expected-falsifications list, and re-pin this section
with the new hashes. Changes that do NOT trigger re-validation:
comment-only or marker-only commits, `pool/pod.rs` / node_informer /
scheduler-side changes (those move the peer table at the next re-pin but
not the audit), and spec prose outside the pinned rules. If the
executor-lifecycle replacement is green-lit mid-campaign, the F1/F3 rows
of this map are its prerequisite review artifact and the Stage-B models
are re-checked with the heartbeat-authority assumptions removed (design
§6) — additional value, but the calibration table then needs a delta
pass.

## Stage B — the tick-level models and their verdicts

Stage B builds the two models this map promised and wires them into the
CI gate. The churn pin above was re-verified immediately before the
model build: the only commits touching the modeled files since the
Stage-A audit are the audit's own marker-only commit and the retry
Phase-1b cross-reference comment bump in `pool/jobs.rs` — no
behavior-relevant drift, no rule-version drift, so Stage A stands.

- **Model J — `spawnCoherence.qnt`** (the L1 coherence protocol): the
  `pool/{jobs,job}.rs` tick body over two intents and one pool — the
  intent poll, the 3-valued placeable gate × pool kind, the Job
  LIST/census, the stale/excess/orphan reap arms behind their
  fail-closed gates, the headroom arithmetic, the 409-deduped spawn
  pass, and the `dispatched_cells`-arming ack. Six configurations:
  base / fault-rpc / fault-lease / fault-stale on the production
  Builder+CRD shape, plus the crd-absent and Fetcher-pool postures
  (the C1/C2 adjudications run as their own exhaustive checks).
- **Model N — `nodeclaimLifecycle.qnt`** (the L10 mirror lifecycle):
  the `nodeclaim_pool/` tick body over two NodeClaim slots and two
  cells — the lease-edge polarity table, the ⊥-streak early-return and
  consolidate-only modes, vanish detection vs the controller's own
  reaps, the recency-gated Registered clears, the reload latch, the
  placeable-gate producer guarantee, and the global / per-class /
  per-tick cover budgets over the hw-class config mirror. Four
  regimes: base / fault-rpc / fault-lease / fault-karpenter.

Checks live in `nix/quint.nix` (`quint-spawn-coherence-*`,
`quint-nodeclaim-*`): one exhaustive TLC check per regime, one
expect-violation check per witness, two expect-violation checks plus a
named-run check for the pre-registered falsifications. State counts,
depths and wall-clocks are in the introducing commit messages and the
checks' transcripts, not here.

### Verdict table (exhaustive TLC, per regime)

Model J — every invariant below holds in **all six** configurations
(base, fault-rpc, fault-lease, fault-stale, crd-absent, fetcher):

| Invariant | Family | Verdict |
|---|---|---|
| `ceilingRespected` | F1/I4 | HOLDS (all 6) |
| `reapSafety` | F1/I3 | HOLDS (all 6) |
| `orphanRemoved` | F1/I2 safety form | HOLDS (all 6) |
| `ackSoundness` | F3/I5 | HOLDS (all 6) |
| `ackCoversPending` | F3/I5 re-ack half | HOLDS (all 6) |
| `degradedPolarity` | F2 | HOLDS (all 6) |
| `gateFailClosed` | F2/I6 (per-configuration clauses) | HOLDS (all 6) |
| `freedSlotsSpendable` | F1 tick-ordering clause | HOLDS (all 6) |

Model N:

| Invariant | Family | base | fault-rpc | fault-lease | fault-karpenter |
|---|---|---|---|---|---|
| `boundsOK` | — | HOLDS | HOLDS | HOLDS | HOLDS |
| `idleReapSafety` | F4/I9 | HOLDS | **VIOLATED (pre-registered)** | HOLDS | HOLDS |
| `iceMarkSoundness` | F3+F4/I7+I8 | HOLDS | HOLDS | HOLDS | HOLDS |
| `bootSampleNotLost` | F4 BootRecordedOnce | HOLDS | **VIOLATED (pre-registered)** | HOLDS | HOLDS |
| `noMassClearAfterFailover` | F3 | HOLDS | HOLDS | HOLDS | HOLDS |
| `reloadLatchRespected` | F4 | HOLDS | HOLDS | HOLDS | HOLDS |
| `singleEffectiveProvisioner` | F4/I11 | HOLDS | HOLDS | HOLDS | HOLDS |
| `gateProducerGuarantee` | F2/I6 producer half | HOLDS | HOLDS | HOLDS | HOLDS |
| `provisioningBudget` | F5/I10 | HOLDS | HOLDS | HOLDS | HOLDS |
| `coverRespectsMask` | F3 mask-before-cover | HOLDS | HOLDS | HOLDS | HOLDS |
| `degradedCoverPolarity` | F2 | HOLDS | HOLDS | HOLDS | HOLDS |

The two VIOLATED cells are exactly the pre-registered
expected-as-built-falsifications entry (the ⊥-tick early-return
observation skip) — confirmed, not new defects. They are excluded from
the fault-rpc HOLD check and pinned by
`quint-nodeclaim-falsification-{idle-conflation,boot-sample-lost}`
(expect-violation) plus the deterministic reproducer runs
`idleConflationRun` / `bootSampleLostRun`
(`quint-nodeclaim-runs-fault-rpc`). When the early-return skip is
fixed, those checks flip to HOLD invariants in the fault-rpc regime
check — the same flip protocol the retry campaign used. As
pre-registered, the bucketed idle-age over-approximates the first
half: the model falsifies at one skipped tick, where the real code
additionally needs the stale entry's skew to cross the per-cell
consolidation floor (≥300 s builders / ≥600 s fetchers vs the ≤40 s
early-return window) — the real-world severity is bounded by that
floor; the model's violation is the structural shape, not the
magnitude.

No falsification outside the pre-registered list appeared in any
regime — the empty-remainder claim of the Stage-A list survived its
first executable test.

### Witness results

Every witness named in the design is violated (the contended scenario
is reachable) in its wired regime: J — excess reap, orphan reap, 409
dedupe, unarmed-gate-blocked spawn, selector-drift reap, ungated
crd-absent spawn, suppressed crd-absent excess; N — idle reap, create,
class-budget bind, vanish mark, clear-of-masked-cell after a mark,
consolidate-only reap, create failure, ceilings fail-closed, create
resuming once ceilings load, unknown-cell drop, handoff with non-empty
inflight, degraded reload tick, fresh clear after acquire, stale
record-only. The design's single "crd-absent spawn while excess-reap
stays suppressed" witness is split into the two halves named above: at
the 2-intent scale a spawnable intent (no Job yet) cannot
simultaneously contribute to a pending surplus, so the conjunction is
structurally unreachable while each half is reachable and the
fail-closed half is separately enforced by `gateFailClosed`.

### Encoding notes (what is by-construction vs checked)

- `SingleJobPerIntent` (I1) is enforced by the model's encoding (Jobs
  keyed by deterministic name; a create on an occupied slot is the
  apiserver's 409): it is not claimed as a checked invariant, and
  `ctrl.pool.spawn-once` therefore carries **no** model verify marker —
  it stays in `tracey query untested` until a code-level test pins it
  (the dedupe-409 witness keeps the collision path reachable in the
  model; the no-ack-on-collision/failure half is checked by
  `ackSoundness`).
- The per-tick ICE-mark dedup is by construction (set-valued marks);
  the checked content is which cells get marked, not how many times.
- Model N abstracts classify()'s in-live ICE/boot-timeout arms to one
  "stuck" bit, omits the dead_nodes arm (consumed input, out of model
  per the Stage-A out-of-model list), abstracts the placeable set's
  content to armed/unarmed (the content guarantee J relies on is
  carried by the J↔N assume-guarantee checklists in both model
  headers), and omits the Pool-coverage filter (demand is modeled
  post-coverage). The per-read snapshot abstraction and its
  pre-registered fallback are documented in the spawnCoherence header;
  Model N's staleness is carried by the inter-tick fault alphabet
  (the LIST-vs-GC race) rather than a separate fault-stale regime.
- Bounded constants: 2 intents, 2 NodeClaim slots, 2 cells, 1 pool
  (the design allowed ≤3 claims; 2 keeps the fault regimes exhaustive
  at CI cost, and every contended scenario the invariants quantify
  over is proven reachable by the witnesses above). The ⊥-streak
  threshold is 2 in the model (5 in the code) — zero / one
  early-return tick / consolidate-only; the invariants quantify over
  the structure, not the constant.
- One inventory/design nit surfaced while encoding: the
  `inflight_created` mutator list (`mod.rs`, mutator 2) and the
  design's fault alphabet describe a clear "on config reload (the
  config-hash gate)", but the only `inflight_created.clear()` call
  site in the code is the lease-acquire Ok arm; the model follows the
  code (a config-cell drop shrinks the configured set without touching
  the inflight map). If the config-hash clear exists elsewhere or is
  added later, the model's `configDropsCell` action is the place to
  mirror it.

### Verify-marker status (Stage-B update)

Seven of the eight Stage-A rules now carry `r[verify]` markers at
their `nix/quint.nix` wiring points: `ctrl.pool.tick-ordering` (base +
fault-stale), `ctrl.pool.degraded-polarity` (fault-rpc),
`ctrl.pool.ack-spawned-soundness` (base + fault-lease),
`ctrl.nodeclaim.lease-edge-polarity` (fault-lease),
`ctrl.nodeclaim.inflight-conservation` (fault-karpenter),
`ctrl.nodeclaim.ice-mark-clear` (fault-karpenter + fault-lease),
`ctrl.nodeclaim.consolidate-only-degraded` (fault-rpc).
`ctrl.pool.spawn-once` deliberately has none (see the encoding notes).
The amended rules gained model-side markers where the model verifies
the amended clauses: `ctrl.nodeclaim.placeable-gate+5` on the
crd-absent and Fetcher configurations (consumer split) and the
fault-lease N regime (producer guarantee);
`ctrl.nodeclaim.budget.per-class+2` on the N base (clamp) and
fault-rpc (failed-creates) regimes.

## Stage-C corpus pin: the calibration denominator

Pinned 2026-05-26 at the worktree base `746164c4f` (the formal-sprint
tip), per the design's Stage-C first-deliverable requirement and the
re-pin protocol above.

### Churn re-pin (run immediately before this pin)

`git log 607a93f3f..HEAD` over the three modeled paths returns three
commits: the Stage-A audit's own map commit, `9283bc450` (a one-line
rule-cross-reference comment bump in `pool/jobs.rs`, retry Phase-1b),
and `782b6155b` (the fetcher-budget stream's successor commit, dated
2026-05-24 — it changes the Simulator-shares-accounting chokepoints in
`pod.rs`/`jobs.rs`, which are the G-C accounting family, outside the
modeled protocol state). Both non-audit commits are ancestors of the
Stage-B model commits, so the models were built against exactly this
tree; the pinned rule versions are unchanged
(`placeable-gate+5`, `consolidate-na+6`, `budget.per-class+2`). No
re-validation trigger fires; Stage A and Stage B stand. The fetcher-
budget successor is recorded here as the design's churn check requires
(its accounting content lands in the G-C rows below as additional
NOT-ENCODED members' context, not as new corpus members — it is a
`feat`, not a `fix`).

### The denominator

The corpus is every `fix(` commit on the modeled tick bodies at the pin:
`pool/jobs.rs` (39 of 63 commits), the `pool/job.rs` lineage followed
through its renames (17 fix commits), and `nodeclaim_pool/` (53). The
design's §3.4 estimate (~101) summed the per-file counts; the pinned
DISTINCT-commit corpus is **95**, because 7 of the job.rs fixes are
jobs.rs multi-file commits (the design assumed 8) and 7 further commits
appear in both the jobs.rs and nodeclaim_pool lists (multi-file commits
the per-file sum double-counts: `f97644a53`, `3f416e02e`, `bcfdc2262`,
`9fd4b6e59`, `b570cdd8d`, `039861b56`, `d5602b3aa`). Each such commit
gets exactly one row, in the family its repair belongs to. The three
incidental cross-crate commits the design named (`3c3062760`,
`c8ca42a91`, `dbc7f7cb2`) are binned in the remainder family.

Excluded by the corpus definition (recorded so the boundary is checked,
not assumed): `pool/pod.rs` commits (39/21 — k8s object construction
outside the modeled tick bodies; G-D-disposition coverage),
`node_informer.rs`-only commits (the M10–M13 / λ-accounting families —
the design lists the M12/M13 pair `ff7f99ab8`/`b80d6f135` for
checker-honesty in the calibration table but outside the denominator),
and ComponentScaler / GC-cron / disruption commits (out-of-scope loops).

### Per-family hash lists (the 95)

| Family | n | Commits |
|---|---|---|
| G-A spawn↔reap↔queued coherence | 10 | `7f04c9d88`, `6a9ba0ef0`, `fb0953870`, `fba9086dc`, `6c4f4983d`, `9123e72d4`, `fd5d7c988`, `5e01a9ff1`, `8b0128f5a`, `004956eeb` |
| G-B ack/ICE protocol | 7 | `cdc78f839`, `5815a7544`, `485e736a2`, `af1383c0e`, `e8bd76451`, `d6bc376d3`, `408a48bcb` |
| G-C resource-accounting parity | 8 | `a415a9a8b`, `286566a57`, `d5602b3aa`, `073170dfb`, `5250a4b9a`, `b25836ef1`, `5c2a83761`, `bcfdc2262` |
| G-D placement derivation | 8 | `80cfcd65c`, `039861b56`, `3f416e02e`, `2f9a3769c`, `9fd4b6e59`, `b570cdd8d`, `015667efa`, `f97644a53` |
| G-E deadline coupling | 2 | `172776b1b`, `f73b98b1f` |
| G-F identity/security plumbing | 3 | `a6697c6b0`, `ea10e1d74`, `acf6d476b` |
| G-G reap delete-propagation & report-path mechanics (job.rs lineage) | 6 | `1779975f6`, `2f04e5432`, `8cbf6d7b3`, `12b86c285`, `2acd1b327`, `6d678ac87` |
| M1 prev_idle / idle model | 7 | `34f37d7e9`, `79f86b888`, `13806e99a`, `a19394346`, `7f91f1892`, `cc2e99887`, `a12c6f9f9` |
| M2 inflight_created / ICE detection | 5 | `0507f9874`, `08d49c52c`, `5935d9122`, `4ece337a4`, `92c2a89f2` |
| M3/M4 sketches lifecycle (lease/PG/seed) | 10 | `92a3dc47d`, `703cbf42a`, `2d62e0b49`, `bd8e57de5`, `6052f84df`, `95fc40fb6`, `9c9bfb7c8`, `b92981881`, `df077d82b`, `3c9aa3919` |
| M5/M6 gauge staleness | 4 | `cab0d2d46`, `d4184cf2b`, `d0c858955`, `e0d504321` |
| FFD/cover ⇄ scheduler-config parity | 16 | `9ff9387f0`, `811489319`, `bd781b004`, `5f754baeb`, `787243ef3`, `45f83cdcd`, `f333ebed5`, `58cd38885`, `c5320b40e`, `e013b2044`, `6c8f13710`, `0fa79fcdf`, `79aa88da2`, `d674f0983`, `4fdf3337b`, `979608619` |
| Remainder (docs/test/alert/infra sweeps + incidental cross-crate) | 9 | `2ad753db9`, `416895e3e`, `3c3062760`, `c8ca42a91`, `dbc7f7cb2`, `a49f78722`, `99a17cd2f`, `f1caa0b60`, `ff5f4e95e` |

File-boundary resolutions the design left to this pin: the FFD/cover
row absorbs the three nodeclaim_pool-touching commits the inventory had
not grouped (`d674f0983` NodeClaim construction, `4fdf3337b` ffd.rs
arch-matching, `979608619` cover.rs ceilings chokepoint); `3c9aa3919`
(sketch persistence serialization) joins M3/M4; `99a17cd2f` (the
scheduler-side authoritative-binding fix whose controller-side touch is
the dead-node reap path — consumed input, out of model) and
`ff5f4e95e`/`f1caa0b60`/`a49f78722` (config/alert plumbing) sit in the
remainder. The inventory's FFD/cover "~13" members that live only in
`node_informer.rs` are outside the corpus by the definition above; all
13 listed there touch `nodeclaim_pool/` and are in.

### Per-family encodability plan (pre-registered → pinned)

The design's §3.4 pre-registration carried over per family, with the
per-commit corrections the pin surfaced (each correction is argued in
the calibration table's rows): G-A encodable via representatives
`fba9086dc`, `6c4f4983d`, `8b0128f5a` (predictions: reapSafety ×2,
gateFailClosed); G-B encodable via `cdc78f839`, `5815a7544`
(ackSoundness, ackCoversPending), the proto/back-compat and
scheduler-side members NOT-ENCODED; G-C / G-D / G-E / G-F NOT-ENCODED
as pre-registered; G-G NOT-ENCODED except the `1779975f6`
census-predicate half (prediction: ackSoundness on today's re-ack
chain); M1 encodable via `79f86b888` (idleReapSafety) with the
`13806e99a` busy-guard half downgraded to a redundancy probe (the
within-tick observe-before-reap ordering covers it at model
resolution); M2 encodable via the two halves of `08d49c52c`
(iceMarkSoundness; the module-local inflight-conservation invariant),
`5935d9122` re-dispositioned NOT-ENCODED (LIST-vs-delete race below
tick atomicity), `0507f9874` treated as the mechanism's introduction;
M3/M4 split exactly as pre-registered — `703cbf42a`
(reloadLatchRespected) and `92a3dc47d`'s recency half
(noMassClearAfterFailover) encodable, content/cell-key members
NOT-ENCODED; M5/M6 NOT-ENCODED (the model carries no cleanup-set state
— a deviation from the pre-registration, recorded); FFD/cover: the
per-class clamp is encoded as a family-level reconstruction
(provisioningBudget), `5f754baeb` itself re-dispositioned to its
sizing content (NOT-ENCODED), `4ece337a4` re-dispositioned NOT-ENCODED
(within-tick per-create granularity below the tick-global create-fault
bit); remainder N/A.

## Stage-C verification runs (serial) and dispositions

Protocol: every override ran serially with the same TLC invocation the
CI checks use (`quint verify --backend=tlc --main=<module>
--step=calibStep --invariant=<predicted>`); violation runs stop at the
first counterexample, baselines and the probe run to exhaustion. The
per-run depths and state counts are recorded in the calibration table
below; wall-clocks live in the introducing commit message and the
transcripts.

Outcome summary:

- **Twelve of twelve pre-fix overrides falsify exactly the invariant
  their module header predicts**, on the first run, with no module
  corrections needed after the corpus-pin commit.
- **The one predicted-HOLDS probe holds**: `m1CalibReapBusyGuardProbe`
  (the `13806e99a` reap_idle busy-guard half) explores the same
  reachable state count as the as-built base regime and finds no
  idle-reap violation. Three-way disposition: this is not a missing
  model dimension to fix and not an incomplete invariant list — the
  busy-guard's trigger state (a live prev_idle entry on a currently
  busy claim at reap time) is unreachable at the model's tick-internal
  ordering resolution because the same tick's observation prunes the
  entry first. It is recorded as defense-in-depth below the per-read
  snapshot abstraction, NOT as a §4(b) redundancy candidate: the real
  loop is not atomic between the observation and the reap, which is
  exactly the window the guard defends. Coverage: the consolidate.rs
  reap_idle unit tests; the windowed-lambda half of the same commit is
  NOT-ENCODED (threshold arithmetic).
- **Both distinguishing baselines hold**: the as-built step at
  CEILING=2 holds `ackCoversPending` (so the `5815a7544` falsification
  is attributable to the missing re-ack, not to the widened ceiling),
  and the as-built step at base constants holds the module-local
  `inflightKeptWhileInFlight` (so the `08d49c52c` KEEP-arm
  falsification is attributable to the drop-on-first-sight prune).
  Overrides at standard regime constants use the wired Stage-B regime
  checks as their baseline (each predicted invariant HOLDS there).
- **The two deterministic reproducer runs pass** (`quint test`:
  `m1AcquireClearOkOnlyRun`, `m2NoConsolidatePruneRun`), pinning the
  documented incident shapes (the failed-reload over-reap and the
  consolidate-only spurious-ICE chain).
- **No stop-and-report event**: no invariant — existing or added —
  falsified on the unmodified as-built models; the calibration added
  no invariant to the main models at all (the one new property,
  `inflightKeptWhileInFlight`, is module-local to the calibration and
  holds on the as-built baseline). The main models and their wired
  Stage-B regime checks are untouched by Stage C, so the Stage-B
  verdict table and state counts above stand as recorded.

## Stage-C calibration: the historical-fix corpus replayed against the models

The 95-commit corpus pinned above, replayed against `spawnCoherence.qnt`
(Model J) and `nodeclaimLifecycle.qnt` (Model N): for each commit the
pre-fix behavior is either expressed as an override of the as-built
model and shown to falsify an invariant (the model would re-find that
bug), or its non-encodability is dispositioned with the missing
dimension and the covering vehicle named. Method per the prior
campaigns: each override is a module in
`docs/spec/models/calibration/controller-<family>.qnt` that instantiates
the as-built model, replaces ONE tick action with a local PRE-FIX
variant, and exposes it as a `calibStep`; the violation latches keep the
as-built oracle. Verdicts are TLC results (violation runs stop at the
first counterexample); verdict format: invariant @ step (depth, states
generated / distinct). Wall-clocks live in the introducing commits'
messages and the run-record section above.

Classification legend: **ENC** — encodable, override written and run.
**ENC-A** — encodable, dispositioned by analogy: the mechanism is
encoded in the as-built model and the named sibling override (or wired
check) exercises the same tick-arm machinery; not separately run.
**NOT-ENC** — the model abstracts the mechanism away (missing dimension
named). **ORIGIN** — the commit introduced the mechanism the model now
encodes; no pre-fix defect of the modeled protocol to revert. **N/A** —
remainder (docs/test/alert/infra sweeps, incidental cross-crate).

### G-A — spawn↔reap↔queued coherence (10)

| Commit | Pre-fix behavior reverted | Class | Override / coverage | Verdict |
|---|---|---|---|---|
| `fba9086dc` | excess-pending DELETE on the informer census alone (no live pod-phase re-check) | ENC | `gaCalibNoLiveRecheck` | **FALSIFIES** reapSafety @ calibStep (depth 10, 4,799/1,000) |
| `6c4f4983d` | orphan_reap_gate treated Ok([]) as authoritative (C3); the C2 orphan-pending arm and C6 admin_call chokepoint are as-built mechanisms | ENC | `gaCalibOrphanGateEmptyOk` | **FALSIFIES** reapSafety @ calibStep (depth 12, 19,785/3,399) |
| `8b0128f5a` | CRD-absent arm armed the gate, so reap_excess_pending ran against an ungated queue | ENC | `gaCalibCrdAbsentArmed` (crd-absent cfg) | **FALSIFIES** gateFailClosed @ calibStep (depth 6, 242/132) |
| `7f04c9d88` | want.is_empty() early-return skipped reap_stale entirely at ceiling-saturation; spawn loop 409-churned | ENC-A | the wantEmpty guard, skip-set and 409-dedupe are the encoded mechanisms; a due-but-skipped stale reap is exactly the orphanRemoved (I2 safety form) latch; same reap-arm machinery as the two G-A overrides above | by analogy |
| `6a9ba0ef0` | spawn stopped at queued−active; selector-drift Pending never reaped | ENC-A | the drift-reap arm and the all-intents spawn iteration are encoded; kept reachable by canReachDriftReap / canReach409Dedupe; the starvation half is liveness-shaped | by analogy |
| `fb0953870` | terminal Jobs name-colliding with a re-queued intent never reaped → respawn blocked | ENC-A | the terminal-collision arm is the encoded unblock mechanism (ETerminalReap path); its loss is a liveness regression below the safety set | by analogy (mechanism encoded; no safety latch) |
| `9123e72d4` | orphan gate without the leader-age arm (plus reap/spawn coherence wiring) | ENC-A | dropping any single 3-arm conjunct reaps under a gate the as-built oracle rejects — same shape and same latch as `gaCalibOrphanGateEmptyOk` | by analogy (sibling falsified reapSafety) |
| `fd5d7c988` | freed slots not credited to headroom the same tick | ENC-A | the freedSlotsSpendable invariant is this clause verbatim; the freed-slot credit is in the encoded headroom arithmetic | by analogy (mechanism encoded; latch exists) |
| `5e01a9ff1` | orphan-reap re-deleted Terminating Jobs every tick | NOT-ENC | the JobPhase partition makes the re-delete a no-op at model resolution (API churn); coverage: is_running_job unit tests, the foreground-delete discipline | n/a |
| `004956eeb` | unpinned selector (reaper thrash on softmax re-roll), HashMap-order truncation, ack arming for headroom-gated intents | ENC-A | the fingerprint pin is the encoded drift mechanism; the deterministic idRank order is by construction; the false-arm half is the same ackSoundness shape `gbCalibAckAttempted` falsifies | by analogy (sibling falsified ackSoundness) |

### G-B — ack/ICE protocol (7)

| Commit | Pre-fix behavior reverted | Class | Override / coverage | Verdict |
|---|---|---|---|---|
| `cdc78f839` | the ATTEMPTED spawn slice was acked (failed creates and 409s included) | ENC | `gbCalibAckAttempted` | **FALSIFIES** ackSoundness @ calibStep (depth 10, 11,881/705) |
| `5815a7544` | only spawned-this-tick intents acked; already-Pending never re-acked after a scheduler restart | ENC | `gbCalibAckOnlyNew` (CEILING=2) | **FALSIFIES** ackCoversPending @ calibStep (depth 11, 9,252/1,776); baseline as-built step HOLDS (depth 32, 81,467,617/4,414,304) |
| `485e736a2` | heartbeat ICE-clear regardless of admissible-cell count | NOT-ENC | scheduler-side clear path (out of J/N per the design); `sched.sla.hw-class.ice-mask` + scheduler tests | n/a |
| `af1383c0e` | intent.ready unwrap back-compat | NOT-ENC | proto-compat surface; unit tests | n/a |
| `e8bd76451` | ready filter + hw_class_names label reconstruction | NOT-ENC | proto-compat surface; unit tests | n/a |
| `d6bc376d3` | trust_threshold single-source | NOT-ENC | §13a bench-gate config plumbing (out of model); hw-bench unit tests + `ctrl.pool.hw-bench-needed+2` | n/a |
| `408a48bcb` | per-dim min_tenants gate | NOT-ENC | same as above | n/a |

### G-C — resource-accounting parity (8)

`a415a9a8b`, `286566a57`, `d5602b3aa`, `073170dfb`, `5250a4b9a`,
`b25836ef1`, `5c2a83761`, `bcfdc2262`: **NOT-ENC**, exactly as
pre-registered — quantity arithmetic across pod spec / FFD / eviction
classification, not protocol state. Coverage: the
Simulator-shares-accounting chokepoint tests in `pool/jobs.rs` /
`pod.rs`, the eviction-classification tests, and the design §3.6 Kani
candidate. The fetcher-budget stream (`2fff4e938`, `782b6155b`) extends
this surface and inherits the same disposition.

### G-D — placement derivation (8)

`80cfcd65c`, `039861b56`, `3f416e02e`, `2f9a3769c`, `9fd4b6e59`,
`b570cdd8d`, `015667efa`, `f97644a53`: **NOT-ENC** (k8s object
construction parity — affinity/toleration/selector/schedulerName).
Coverage: pool construction unit tests, `vm-protocol-*` /
`vm-forecast-provisioning`, helm/CRD drift checks.

### G-E — deadline coupling (2)

| Commit | Class | Coverage |
|---|---|---|
| `172776b1b` | NOT-ENC | the controller half is plumbing (daemon timeout from intent deadline); the at-cap behavior of the report it feeds is the retry campaign's E7 surface (its calibration row `retryCalibG1DeadlineUncapped` already pins the no-cap world) |
| `f73b98b1f` | NOT-ENC | deadline floor constant; unit tests |

### G-F — identity/security plumbing (3)

`a6697c6b0`, `ea10e1d74`, `acf6d476b`: **NOT-ENC** (token/claims
plumbing). Coverage: token-mode unit/VM tests (`vm-token-mode`), auth
tests.

### G-G — reap delete-propagation & report-path mechanics (6)

| Commit | Pre-fix behavior reverted | Class | Override / coverage | Verdict |
|---|---|---|---|---|
| `1779975f6` | is_pending_job counted Terminating Jobs as Pending (census, excess set, ack set); background delete propagation | ENC (census half) / NOT-ENC (propagation half) | `ggCalibTerminatingPending`; the propagation half sits below the model's deletion atomicity — ci-failure-patterns' job-tracking-finalizer entry, `vm-lifecycle-*` | **FALSIFIES** ackSoundness @ calibStep (depth 12, 30,838/4,920) — on today's protocol the lost filter resurfaces as a false re-ack |
| `2f04e5432` | orphan-running reap used background delete (second finalizer-orphan callsite) | NOT-ENC | below deletion atomicity; same coverage as above; the foreground-delete discipline is an explicit §4 non-candidate | n/a |
| `8cbf6d7b3` | kubelet eviction-message match | NOT-ENC | report-classification string matching; unit tests | n/a |
| `12b86c285` | daemon_timeout/deadline alignment + DeadlineExceeded report | NOT-ENC | G-E coupling; the scheduler half is the retry campaign's surface | n/a |
| `2acd1b327` | floor promotion gated on OOMKilled/DiskPressure | NOT-ENC | scheduler-side promotion gate (retry G6 family) | n/a |
| `6d678ac87` | consolidated Job constructor; manifest-path protections | NOT-ENC | G-D construction parity; unit + VM tests | n/a |

### M1 — prev_idle / idle model (7)

| Commit | Pre-fix behavior reverted | Class | Override / coverage | Verdict |
|---|---|---|---|---|
| `79f86b888` | prev_idle cleared only on the reload Ok arm — a failed reload kept the previous tenure's entries (amplify polarity) | ENC | `m1CalibAcquireClearOkOnly` + named run `m1AcquireClearOkOnlyRun` | **FALSIFIES** idleReapSafety @ calibStep (depth 20, 2,355,905/26,339) |
| `13806e99a` (busy-guard half) | reap_idle did not re-check requested>0 at reap time | ENC-probe | `m1CalibReapBusyGuardProbe` — exhaustive | **HOLDS** idleReapSafety @ calibStep (depth 20, 242,933/1,660 — the as-built base state space; the guard never binds at model resolution). Disposition in the run-record section: defense-in-depth below the tick-internal observe-before-reap ordering; not a deletion candidate |
| `13806e99a` (windowed-lambda half) | consolidate_after finite-difference hazard | NOT-ENC | threshold arithmetic inside the abstracted NA model; consolidate.rs unit tests + `ctrl.nodeclaim.consolidate-na+6` | n/a |
| `34f37d7e9` | idle tracking read the dead Karpenter Empty condition | ORIGIN | introduced the controller-side prev_idle mechanism the model encodes as pIdle; the pre-fix world has no idle reaps at all (cost/liveness, no safety latch) | n/a |
| `a19394346` | FFD placed onto terminating NodeClaims | NOT-ENC | the placement-quality consequence is below the safety set; the budget half (terminating still billed) is encoded and checked by provisioningBudget; ffd.rs unit tests + `ctrl.nodeclaim.ffd-exclude-terminating` | n/a |
| `7f91f1892` | builder consolidation floor 60s | NOT-ENC | threshold constant; consolidate.rs tests | n/a |
| `cc2e99887`, `a12c6f9f9` | per-cell context refactor; hold-open clamp | NOT-ENC | refactor / threshold structure inside the abstracted NA model; consolidate.rs tests | n/a |

### M2 — inflight_created / ICE detection (5)

| Commit | Pre-fix behavior reverted | Class | Override / coverage | Verdict |
|---|---|---|---|---|
| `08d49c52c` (bug_012 half) | consolidate_only never pruned inflight_created — the controller's own reaps read as Karpenter GC on the next full tick | ENC | `m2CalibNoConsolidatePrune` + named run `m2NoConsolidatePruneRun` | **FALSIFIES** iceMarkSoundness @ calibStep (depth 16, 1,085,474/4,039) |
| `08d49c52c` (bug_020 half) | detect_vanished pruned entries on first sight in live — fast-GC'd claims escaped detection | ENC | `m2CalibInflightDropOnSight` (module-local invariant `inflightKeptWhileInFlight` — the harm is a missed mark, completeness-shaped, so the calibration pins the structural conservation half) | **FALSIFIES** inflightKeptWhileInFlight @ calibStep (depth 3, 290/6); baseline as-built step HOLDS (depth 18, 242,933/1,660) |
| `0507f9874` | (pre-fix world: no vanish/LaunchFailed ICE detection at all; cover from rio.build/* labels) | ORIGIN (detection half) / NOT-ENC (requirements half) | the detection mechanism is what the model encodes (kept reachable by canReachVanishMark); the requirements half is construction parity | n/a |
| `5935d9122` | reap_unhealthy 404 arm did not mask; dead-cap counted in-flight | NOT-ENC | the LIST-vs-delete 404 race sits below the tick's atomicity; dead_nodes is consumed input (out of model). Coverage: health.rs unit tests. Re-dispositioned from the design's family-level "encodable" pre-registration | n/a |
| `4ece337a4` | failed creates consumed per-tick budget (under-cover within the round-robin) | NOT-ENC | within-tick per-create granularity is below the tick-global create-fault bit, and the budget is recomputed per tick (no cross-tick consequence); coverage: cover.rs accounting tests, `ctrl.nodeclaim.budget.per-class+2`'s failed-creates clause, canReachCreateFailure keeps the path reachable. Re-dispositioned from the design's pre-registration | n/a |
| `92c2a89f2` | drop-reason metrics split | NOT-ENC | observability only | n/a |

### M3/M4 — sketches lifecycle (10)

| Commit | Pre-fix behavior reverted | Class | Override / coverage | Verdict |
|---|---|---|---|---|
| `703cbf42a` | reload latch cleared on the load attempt; persist ungated | ENC | `m34CalibLatchClearOnAttempt` | **FALSIFIES** reloadLatchRespected @ calibStep (depth 14, 40,457/815) |
| `92a3dc47d` (recency-gate half) | observe_registered emitted clears (and samples) for stale registrations after an acquire | ENC | `m34CalibNoRecencyGate` | **FALSIFIES** noMassClearAfterFailover @ calibStep (depth 14, 34,086/767) |
| `92a3dc47d` (other halves) | lease hooks doing the unarm/reload work; ack dedup; non-blocking connect_pg | ENC-A / by construction | the hook machinery is the as-built E-acq/E-loss encoding exercised by the fault-lease regime; the per-tick mark dedup is set-valued by construction; connect_pg is plumbing | n/a |
| `2d62e0b49`, `bd8e57de5`, `6052f84df`, `95fc40fb6`, `9c9bfb7c8`, `b92981881`, `df077d82b` | seed/rotate ordering, shadow gates, quantile fallbacks, cell-key aliases | NOT-ENC | sketch contents and key forms are abstracted (the design's pre-registered split); sketch.rs unit tests | n/a |
| `3c9aa3919` | sketch persistence serialization (bincode→postcard) | NOT-ENC | serialization format below the abstract PG cell; sketch.rs round-trip tests | n/a |

### M5/M6 — gauge staleness (4)

| Commit | Class | Coverage |
|---|---|---|
| `d0c858955` | ENC-A (the kube-only-observations sharing half) / NOT-ENC (the trailing-zero gauge half) | the shared observation block is the as-built consolidate-only encoding; the residual unobserved window (the ⊥ early-return) is exactly the pre-registered as-built falsification already wired (`quint-nodeclaim-falsification-boot-sample-lost`); gauges are observability |
| `cab0d2d46`, `d4184cf2b`, `e0d504321` | NOT-ENC | observability only (the model carries no cleanup-set / gauge state — recorded as a deviation from the design's "polarity classification encodable" pre-registration); gauge_universe / emit_live_gauges unit tests |

### FFD/cover ⇄ scheduler-config parity (16)

| Commit / row | Pre-fix behavior | Class | Override / coverage | Verdict |
|---|---|---|---|---|
| family-level reconstruction (anchors: the cover.rs class_budget mechanism, `ctrl.nodeclaim.budget.per-class+2`; the design's named representative `5f754baeb` turned out to be the per-claim sizing half) | cover sized against the global fleet budget only — no per-class clamp | ENC | `ffdCalibNoClassClamp` | **FALSIFIES** provisioningBudget @ calibStep (depth 8, 2,368/42) |
| `5f754baeb` | per-claim SizingCfg from global max_cores/mem (oversized NodeClaims → InsufficientCapacity → ICE loop) | NOT-ENC | per-claim sizing arithmetic (the design's pre-registered NOT-ENCODED half); cover.rs sizing unit tests; §3.6 Kani candidate | n/a |
| `79aa88da2` | no periodic HwClassConfig refresh (unbounded skew); node-role label | NOT-ENC | the refresh cadence is below time resolution — the model's configuredCells / ceilingsKnown environment actions abstract it; the fail-closed gate it feeds IS encoded (degradedCoverPolarity + canReachCeilingsFailClosed) | n/a |
| `9ff9387f0`, `811489319`, `bd781b004`, `787243ef3`, `45f83cdcd` | sizing predicate / chunking / per-cell cap filtering / anchor sizing / reference-cell assignment | NOT-ENC | FFD/cover sizing arithmetic (pre-registered); cover.rs + ffd.rs unit tests, Kani candidate | n/a |
| `f333ebed5`, `58cd38885`, `c5320b40e`, `e013b2044`, `6c8f13710`, `0fa79fcdf` | feature/arch-axis parity chokepoints shared with the scheduler | NOT-ENC | axis-derivation parity (G-D-shaped); chokepoint unit tests on both sides | n/a |
| `d674f0983`, `4fdf3337b`, `979608619` | NodeClaim spec construction (expireAfter), ffd arch-matching, cover ceilings chokepoint | NOT-ENC | construction/axis parity; unit tests + `vm-forecast-provisioning` | n/a |

### Remainder (9)

`2ad753db9`, `416895e3e` (test/doc sweeps), `3c3062760`, `c8ca42a91`,
`dbc7f7cb2` (incidental cross-crate, named by the design), `a49f78722`,
`f1caa0b60` (helm/alert plumbing), `ff5f4e95e` (configmap/RBAC infra),
`99a17cd2f` (scheduler-side authoritative-binding fix; its
controller-side touch is the dead-node reap path — consumed input, out
of model per the Stage-A out-of-model list): **N/A** — no modeled
protocol content.

### Outside the corpus, listed for checker honesty

`ff7f99ab8`, `b80d6f135` (M12/M13 bound-intent disambiguation,
node_informer family): **NOT-ENC** — Models J/N carry no Pod objects
and `dead_nodes` is consumed input; their protection is the design
§4(a)1 gate tests (the ported two-pods-one-intent derivation tests and
the consequence-chain coverage), not an F1–F5 invariant. Recorded here
so the boundary stays explicit.

### Permanent expect-violation witnesses (wired into nix/quint.nix)

Six of the thirteen modules are wired as `quint-ctrl-calib-*` checks —
one per falsifying family with a plausible regression path in the
as-built code and a cheap state space (the same proportion as the prior
campaigns); the rest are evidence modules, re-runnable with the README
recipe.

| Check | Module | Violated invariant | Guards against |
|---|---|---|---|
| `quint-ctrl-calib-ga-live-recheck` | `gaCalibNoLiveRecheck` | `reapSafety` | losing the live pod-phase re-check before the excess DELETE (fba9086dc) |
| `quint-ctrl-calib-gb-ack-spawned-only` | `gbCalibAckAttempted` | `ackSoundness` | acking attempted instead of created spawns (cdc78f839) |
| `quint-ctrl-calib-m1-acquire-clear` | `m1CalibAcquireClearOkOnly` | `idleReapSafety` | regressing the unconditional prev_idle clear to the Ok arm (79f86b888) |
| `quint-ctrl-calib-m2-consolidate-prune` | `m2CalibNoConsolidatePrune` | `iceMarkSoundness` | losing the consolidate-only inflight prune (08d49c52c) |
| `quint-ctrl-calib-m34-reload-latch` | `m34CalibLatchClearOnAttempt` | `reloadLatchRespected` | clearing the reload latch on attempt instead of on Ok (703cbf42a) |
| `quint-ctrl-calib-ffd-class-clamp` | `ffdCalibNoClassClamp` | `provisioningBudget` | dropping the per-class fleet clamp from cover |

The G-G and the second M1/M2/M3-M4 overrides, the crd-absent and
orphan-gate G-A overrides, the CEILING=2 G-B override and the
busy-guard probe stay evidence modules: their regression paths are
either already guarded by a wired sibling on the same tick arm, pinned
by a deterministic named run, or (the probe) are HOLDS evidence rather
than a regression guard.

### Phase-0 exit-gate verdict

**Met.** Per the design §5 Stage-C gate:

- **Every family has a falsifying representative or an explicit
  NOT-ENCODED disposition naming its coverage.** Falsifying
  representatives exist for G-A (3), G-B (2), G-G (1), M1 (1), M2 (2),
  M3/M4 (2) and FFD/cover (1, family-level); G-C, G-D, G-E, G-F, M5/M6
  and the remainder carry per-commit NOT-ENCODED / N-A rows naming the
  covering vehicle, exactly as the design pre-registered for them.
- **Every falsification trace was walked against the code path** during
  module construction (each override's header cites the decision sites
  and the incident the historical fix recorded; the two deepest traces
  are additionally pinned as deterministic named runs).
- **Permanent `quint-ctrl-calib-*` checks are wired** for the
  regression-worthy subset (six, one per falsifying family).
- **The calibration table is committed next to the invariant map** (this
  section), and the corpus denominator was pinned before any override
  ran.
- **No stop-and-report event occurred**; the main models are untouched,
  so the Stage-B verdicts stand without re-runs.

Honest deltas from the design's pre-registration, all argued in the
rows above: the verdict shape is leaner than the predicted "45–50 of
~101 encodable through a representative" — about 20 of the 95 pinned
commits are ENC/ENC-A, because the per-commit pin shows the family
tails are dominated by sizing arithmetic, content/serialization,
axis-parity and observability commits that the design itself
pre-registered as NOT-ENCODED at the family level; the per-commit
re-dispositions (`5935d9122`, `4ece337a4`, `5f754baeb`'s sizing
content, M5/M6's missing cleanup-set state, the `13806e99a` busy-guard
probe) are recorded as checked-prediction corrections rather than
silent shrinks. The MBT decision the design deferred to this gate:
calibration did NOT keep tripping on model/code mismatch (every
override falsified as predicted on the first run), so the §3.6
recommendation stands — no MBT/trait-extraction prerequisite is imposed
on Phase 1; the decision is recorded as "not now", revisitable if
Phase-1 work surfaces conformance doubts.

### Phase-1 input list

What the calibration adds to the Phase-1 plan, beyond the design §4
scope it leaves unchanged:

- **Licensed by the calibration (no further model evidence needed):**
  nothing new — the §4(a) recomputable-cache deletions (PodRequestedCache
  → per-tick LIST with the named derivation rule and gate tests;
  NodeLabelCache labels → per-need GET / per-flush LIST with the M11
  cursor semantics named) were never model-gated and proceed on their
  own named test/VM evidence, after this Stage C, as the design
  schedules them.
- **§4(b) candidates surfaced by the table: none.** No mechanism's
  revert came back HOLDS-because-redundant with a confirming second
  representative. The single HOLDS row (the `13806e99a` busy-guard) is
  explicitly NOT a candidate: its trigger lives below the model's
  tick-internal ordering resolution, which is an abstraction of the
  model, not a property of the code.
- **Caches the calibration does NOT license for deletion:** `prev_idle`
  (M1), `inflight_created` (M2), the sketches reload latch and
  recency gate (M3/M4), the placeable gate and its fail-closed
  consumer postures, the per-class cover clamp — each now has a wired
  expect-violation check demonstrating the model re-finds the bug its
  removal would reintroduce; they are exactly the design's §4
  non-candidates, now with machine-checked evidence attached.
- **Carry-forward observations for Phase-1/2 work:**
  - The ⊥-tick early-return observation skip (the one pre-registered
    as-built falsification) remains open; when its fix lands, the two
    `quint-nodeclaim-falsification-*` checks flip to HOLD invariants in
    the fault-rpc regime per the Stage-B flip protocol — Phase 1 should
    schedule that fix or explicitly defer it.
  - The `inflight_created` mutator-list comment in
    `nodeclaim_pool/mod.rs` still names a config-reload clear that has
    no call site (the Stage-B encoding note); whoever touches that
    block next should reconcile the comment or add the clear, and the
    model's `configDropsCell` action is the place to mirror it.
  - `ctrl.pool.spawn-once` still carries no verify marker (the Stage-B
    encoding note); a Phase-2 code-level dedupe test remains the
    cheapest way to close it.
  - The G-C/G-D/G-F NOT-ENCODED families and the FFD/cover sizing tail
    are where the §3.6 Kani candidates live if Phase 2 wants more than
    unit tests there.
