# Substitution-replacement campaign records (closed-campaign archive)

Archived verbatim from
`docs/spec/models/substitution-replacement-invariant-map.md` @ `a00957266`
(the retirement wave's base; the map is deleted unchanged by the wave's final
commit); append-only; this is a closed-campaign archive, not a live registry —
nothing here maps live artifacts. Relocated by owner directive 2026-06-12
("can we get rid of the invariant-map.md's now?"), content unmodified.

References to `*-invariant-map.md` files inside the archived text below are
historical: all 13 per-campaign invariant maps were retired in the same wave.
Their surviving records live in the sibling `*-records.md` archives, the model
and calibration `.qnt` headers, the `nix/quint.nix` check comments, the spec
`.typ` rules, and `docs/ops/` — and the full originals in git history.

Live carriers for this campaign (not this file): `materializationJob.qnt`
(model + header measurement block) with the `quint-materialization-*` checks
and `mat-*.qnt` calibration twins (`nix/quint.nix`), the §9.3
calibration-transfer verdict section in `calibration/README.md`, the
`sched.materialize.*` rules in `docs/spec/components/scheduler.typ`, and
`docs/ops/materialization-deployment-checklist.typ` (the operator index).

---

## 1. Campaign close-out (A5/A6), counter-signature record, supersessions, final consolidation, follow-up ledger

> Origin: substitution-replacement-invariant-map.md §§ "Campaign close-out
> (A5/A6)" through "Post-ledger closure" — the five-ruling counter-signature
> record (applied), the B5/E5/evidence-bits/CE-D7/CE-D8 supersession
> authorizations inside the final acceptance accounting, the FINAL
> CONSOLIDATION deployment checklist (the archive copy of the MD-D5 and
> M1/M2 rows; the operator-facing copy lives in
> docs/ops/materialization-deployment-checklist.typ), the follow-up ledger
> rows 1-10, and the post-ledger closure with the Q5 establishment-park
> reversal (owner-signed supersession; the superseded residual-(a) text is
> preserved inside it), verbatim.

## Campaign close-out (A5/A6) — CAMPAIGN CLOSED 2026-06-02

The substitution-replacement campaign (round-2 Track A stages A3–A6;
design `substitution-replacement-design.md`, owner A4 all-phases approval
2026-05-31) is **complete and CLOSED**. All four phases are landed and
integrated on `formal-sprint` behind green full gates; the owner's
2026-06-01 final decision gate authorized D′ and counter-signed the five
standing orchestrator rulings; this section discharges the Phase D′
A5/A6 handoff (the signature applications, the acceptance accounting,
the deployment-checklist consolidation, the follow-up ledger). The stage
records above are the permanent evidence and are not edited by this
section.

| Phase | Landed | One line | Stage record |
|---|---|---|---|
| A — additive-dormant | 2026-06-01 | The full job architecture (migrations 078/079, ledger kinds, fenced db modules, store executor, scheduler machinery, 4 spec rules) behind `materialization.enabled = false`; dormancy criteria 1–7 HOLD; 30 commits, +75 tests, +6 CBMC harnesses, +2 wired quint checks | "Phase A stage record" |
| B — the flip | 2026-06-01 | Helm defaults ON (AS-6 AND-guard); the six equivalence criteria HOLD against byte-original `-walk` oracles; five product bugs (B1–B5) + the finding-11 design correction fixed red-first; VM matrix 30→37; +43 tests; 31 commits | "Phase B stage record" |
| C′ — the model | 2026-06-01 | `materializationJob.qnt` re-targeted to the as-built system (six deltas; two draft-wrong-verdict classes caught); 21 invariants × 5 regimes; the §9.3 calibration transfer at 100% (18/18 falsifications + all baselines + 6 by-construction + 2 liveness flips, zero failures); 43 checks wired; CE-D6 re-run 7/7 zero-violation; GO | "Phase C′ stage record" |
| D′ — the deletion | 2026-06-02 | Walk machinery, `Substituting`, the lossy wanted-set, BOTH evidence columns, the coexistence flag and the superseded verification surface DELETED — net −16,075 lines (134 files); migration 080; kernel 17→14→9 harnesses; 27 closure checks → survivors core; 26-rule spec sweep + 6 forced additions; 43/43 verdict-identical; 26 commits | "Phase D′ stage record" |

### Counter-signature record (the five rulings, applied)

The owner counter-signed all five standing orchestrator rulings at the
2026-06-01 final decision gate; the signature lines are applied at each
ruling's flag (handoff item 1's finding-11 half plus the four rulings the
decision named alongside it):

| # | Ruling | Flag site | Signature |
|---|---|---|---|
| 1 | Finding-11 mark discriminator (B2; `sched.materialize.routing+2`, post-D′ the pruned-origin form `+3`) | Phase B stage record (ruling-status note); C′ entry criterion 2 / GO condition 1 | Applied at the Phase B finding-11 note; C′ GO condition 1 marked resolved |
| 2 | PD-15b (tonic-forced dormant handler arms, Phase A Wave 2) | Phase A plan-decision deviations table | In-row |
| 3 | PD-21 (regime split instead of extending the existing check, Phase A Wave 5) | Phase A plan-decision deviations table | In-row |
| 4 | L2 armed form (predecessor campaign; orchestrator call within owner decision 4) | `closure-evidence-invariant-map.md`, owner-decision provenance, decision-4 row | In-row there |
| 5 | Closure-evidence Phase-1 provenance (the 4 orchestrator calls: L2 armed form; Wave-1 battery enumeration; Wave-3 fence enumeration; Wave-4 C3-pin scope) | `closure-evidence-invariant-map.md`, the provenance paragraph | Appended there |

The closure-evidence Phase-2 counter-signature line items 5–8 (the
decision-3 residual; the C5/CE-7 residual row; the trailing-EM closures;
the admit_pull extraction scope) were NOT part of the 2026-06-01 decision
and remain standing owner items — carried in the follow-up ledger below.

**Counter-signature record addendum (2026-06-02, follow-up-ledger close-out).**
Line items 5–8 and the Item I controller-map delta entry were
counter-signed by the owner on 2026-06-02 (gate label: follow-up-ledger
close-out); the executor-campaign 1c OA2 landed-form counter-signature was
additionally collected in the same batch (S-OA2, owner "collect"
2026-06-02). Signature blocks applied at the closure-evidence decision-3
provenance row and line-items section, the controller-map Item I delta
entry, and the controller-map 1c entry; ledger row 8 below is CLOSED
accordingly. The paragraph above stays as the historical pre-batch record.

### Final acceptance accounting (the design §8 table, final form)

Per handoff item 5, A5/A6 own this table; each phase's full criteria
evidence is its stage record above.

| Phase | Acceptance criteria | Verdict | Evidence |
|---|---|---|---|
| A | Dormancy criteria 1–7 (existing-test / empty-tables / kernel / wire / wired-check / schema / config invariance) | **ALL HOLD** | Phase A "Dormancy evidence" table; the T-6.3 diff audit; full gate green at the Phase A tip |
| B | Equivalence criteria 1–6 (outcome equivalence ×5 sequences both runs; flag-off invariance; zero-walk for fresh flag-on work; settlement totality; wire/schema freeze; formal-check invariance) | **ALL HOLD** | Phase B "Equivalence evidence" table; the 37-attr both-state VM matrix; the Wave 7 gate (380 attrs, exit 0) |
| C′ | Go/no-go criteria 1–6 (calibration transfer; §9.1 holds + witnesses; six-delta as-built match; CE-D6 re-run; dormancy-oracle bit-identity; finding-11 resolution) | **GO — both conditions discharged** (condition 1 by the 2026-06-01 counter-signature; condition 2 by the integration gate and the D′ gates on top of the integrated tree) | Phase C′ record; the counter-signature record above |
| D′ | Every commit boundary green; 43/43 verdict-identical re-run; survivors core wired; absence gate zero-unexplained; spec sweep complete; full gate at exit | **MET** | Phase D′ record; gate #4/#4b (392 successes + the docs-lint fixup → exit 0) |

**The §9.1 property set, end state.** 21 named invariants, all CHECKED at
the three-tier posture: bounded-simulation holds ×5 regimes (2 M × 15),
19 TLC falsifiability pins, 14 reachability witnesses, 10 named-run pins;
the Ex-scope exhaustive conjunctions remain documented manual targets
with zero-violation bounded prefixes (the C′ measurement table;
`longChecks` ships empty by physics). Post-D′ the model is re-targeted to
the final system (`ad23341ae`: the builderPull `mustSubstitute` conjunct
dropped under the window-empty equivalence; PD-D1 `dedupPrunedUpgrade`
encoded — closing the C′ delta-1 scope note) and all 43 checks re-ran
verdict-identical (T-D7.2). The predecessor's six surviving invariants
(A14/A15/A22/B9/B10/L3) ride the wired survivors core with the two kept
pins.

**The calibration-transfer accounting, final form = the C′ table** (the
go/no-go criterion-1 record above): 18/18 predicted falsifications
VIOLATED under both backends, every baseline HOLDS (one model-side
recalibration, recorded), 6 by-construction rows with structural
arguments, 2 liveness flips exactly as predicted, zero transfer failures;
the D′ T-D7.2 re-run is the verdict-preservation proof for the final
tree. Nothing supersedes the C′ table — it is the campaign's calibration
record.

**Supersession and retirement records (the handoff item 1 balance; each
authorized by the owner's D′ GO, 2026-06-01):**

- **B5 supersession** — the predecessor's stored-union write-once shape
  is superseded by last-write-wins-per-build over the durable
  `build_wanted_outputs` relation (`sched.merge.wanted-outputs+3`). The
  sign-off evidence is the C′ F5/PP-5(ii) DOCUMENTED-INTENDED row
  (same-build narrowing reachable, intended) plus the
  `crossBuildWantedIsolation` wired pin for the cross-build direction.
- **E5 supersession** — the retry campaign's substitution carve-out (the
  walk-era E5 boundary) is succeeded by the materialization attempt
  class: kind-partitioned fold, own budget, never-poisons
  (`quint-retry-policy-pull-materialization` + the kind-partition CBMC
  harnesses); the cross-campaign record is `retry-invariant-map.md`'s
  Wave-5 addendum.
- **Evidence-bits retirement** — `topdown_pruned` / `closure_hole` (and
  the lossy `wanted_output_names` stored union) dropped by migration 080
  after the three durable re-sources (PD-D1/PD-D4/PD-D5) landed
  red-first; dispositions in the D′ plan §3.7 + the D′ stage record; the
  legacy decode arm was verified then removed (migrate-before-recover).
- **CE-D7 vacuous-close** — the AW1 lost-hole-stamp ∩ builds-row-purge
  bound closes vacuous-by-construction (no hole breadcrumb exists; the
  durable classifier decides at decision time). Recorded in the
  closure-evidence map's Phase D′ cross-reference note.
- **CE-D8 split (design §5.4, handoff item 2)** — shape (a)
  GC-between-vouch-and-use is DELIVERED by pin-at-ingest
  (`pinCoversIngestUntilAllInterestTerminal`, wired; the `mat-b2-no-pin`
  calibration re-finds the GC trace); shape (b) stale-Produced narrows
  into the kept B9 guard (`quint-closure-calib-f1-stale-produced`).
  Pointer rows live in the closure-evidence map (added at D′).

### Deployment checklist — FINAL CONSOLIDATION

The single operator index: every deployment-checklist row across the
round-2 campaigns with its final state, one line each. Source rows live
in their owning records (the closure-evidence map, the gw-session map,
the Phase B checklist above, the harden-store reconciliation memo §3.4);
this table does not restate their bodies.

| Row | Final state |
|---|---|
| CE-D1 fenced-write metric + leader alert | SURVIVES, extended — job-table and wanted-relation writes share the claims-floor fence and counter; the `RioSchedulerEvidenceWriteFenced` alert ships in the chart (Phase B). |
| CE-D2 failover PG-flap alerts | SURVIVES unchanged — the same metrics now also guard the job-write floor reads. |
| CE-D3 merge `FAILED_PRECONDITION` at failover | SURVIVES unchanged — in-tx job creation rides the same fenced merge refusal (a fenced merge creates no jobs). |
| CE-D4 wrongful-fail-fast gone / resubmit guidance | SURVIVES re-keyed — the single fail-fast site is consumption-settlement arm 3 (four-conjunct), post-D′ keyed on pruned job ORIGIN + the durable three-part classification (`sched.materialize.routing+3`); a resubmit mints a new job with a fresh one-shot. |
| CE-D5 poison-clear wakes spared parents | SURVIVES re-keyed — survivors carrying unresolved jobs are job-armed; the reprobe lane creates `reprobe`-origin jobs (AS-5 reset + `poison_cleared`); the promotion arm is untouched. |
| CE-D6 manual-target runbook | RE-KEYED to the successor model — the seven closureEvidence conjunctions re-ran zero-violation at C′ (the dual-coverage instant) and the 27 checks retired at D′; the standing manual targets are the five `materializationJob*Ex` conjunctions (the C′ measurement table); the survivors core stays wired with its two pins. |
| CE-D7 AW1 lost-hole-stamp bound | CLOSED vacuous-by-construction at D′ (no hole breadcrumb exists). |
| CE-D8 GC-after-vouch bounds | SPLIT per design §5.4 — shape (a) delivered by wired pin-at-ingest; shape (b) narrowed into the kept B9 guard (the supersession records above). |
| CE-D9 D10 expired-at-load poison residual | SURVIVES unchanged — re-homed to the surviving recovery-time fenced write. |
| GW-D1 TCP_USER_TIMEOUT setsockopt-failure alert | SURVIVES unchanged (zero gateway production changes this campaign; the W7 premise is intact). |
| GW-D2 `channels_active` autoscaling caveat | SURVIVES unchanged. |
| GW-D3 NAR-buffering memory headroom | SURVIVES unchanged. |
| GW-D4 conn-permit occupancy alert | SURVIVES unchanged. |
| GW-D5 SIGKILL / terminationGracePeriodSeconds | SURVIVES unchanged. |
| MD-D1 park alerting + runbook | SURVIVES, population re-keyed (2026-06-03, the Q5 reversal) — `rio_scheduler_materialization_stalled`, the `RioSchedulerMaterializationStalled` alert and the runbook are intact (same gauge, no new label); the population now INCLUDES establishment-only crash-loops (party-blind parking) and the runbook gained the store-replica arm; Item T builds on them; harden-store trigger 1's observation point. |
| MD-D2 / MD-D3 flag rollback / mixed-flag guidance | RETIRED at D′ — replaced by the §7.5 deployment-and-rollback posture (the D′ stage record): binary rollback through D′.1; migration 080 roll-forward only; binaries-first deployment order; bounded self-identifying transition residuals. |
| MD-D4 instance-binding skew posture | SURVIVES — fail-closed across version skew; the binding check is unconditional post-D′. |
| MD-D5 post-wipe cold-start observation set (NEW — lands with this consolidation per harden-store memo §6.3) | The memo §3.4 P1–P10 predicates in materialization terms + the warm-phase capture (Item I's threshold-calibration seed); observation, never a gate; a P7 wall-clock miss adjudicates against P8's lever record before reading as harden-store trigger 3. |
| MD-D5/M1 pre-scale option (NEW, memo-added) | Zero-code belt-and-braces for the planned wipe (replica count multiplies executor lanes); record the pre-scale-vs-measure choice if Item I has landed; option, never a gate. |
| MD-D5/M2 store-at-floor observation (NEW, memo-added) | Record store ready-replicas vs the floor through the wave UNCONDITIONALLY — keeps harden-store trigger 4 observable in exactly the branch where it arms. |

### Follow-up ledger (everything leaving the campaign open)

| # | Item | Status / pointer |
|---|---|---|
| 1 | **Floating-CA stale-reset carrier gap** — the stale-verify reset clears `output_paths`; job assignments carry expected paths `== [""]`; pre-existing Phase B shape, and the walk's stash that papered over it is deleted | CLOSED (2026-06-02, this commit): red-first carrier fix landed `1cedf8957` (owner option (a) 2026-06-02: durable `carried_realized_paths` column, migration 082, `stale_reset`-origin-only writes) — realized paths carried across the stale-Completed reset into the stale_reset lane; executor wanted set non-empty for floating-CA; coverage non-vacuity scoped to the empty-expected-slot shape; spec sentence + markers in the same commit |
| 2 | **FailoverDuo B9 corner** — the predecessor encoding's post-failover stored-union widening violates B9 at the two-builds+failover scope (full alphabet, reproduced; seed `0xffbfc9ac0c85df5b`; transcript in the T-D7.1 commit body and the module scope note). History — production deleted the lossy fallback at D2.3; the corner documents WHY the 062 semantics had to die | CLOSED (2026-06-02, this commit): wired as the permanent seeded expect-violation check `quint-closure-corner-failover-duo-b9` (`mkQuintSimWitnessCheck` gained an optional `--seed`; the check replays the discovery configuration — survivors-restricted alphabet at the FailoverDuo constants, module `closureEvidenceCornerFailoverDuo`; the full-alphabet form's hit rate is below a bounded budget, its status stays the trace-inclusion corollary in the module SCOPE NOTE; red means the predecessor encoding drifted — route to row 3's archive record, never silently delete). The check is the seed-carrier row 3 inherits; retroactive validation of the replacement stands |
| 3 | **`closureEvidence.qnt` archive** — deliberately NOT archived at this close-out: the survivors core (A14/A15/A22/B9/B10/L3 + 2 kept pins) soaks one integration first; the archive then carries the survivors core forward and the B9 corner with its seed | CLOSED (2026-06-02, this commit): archived IN PLACE under the owner's 2026-06-02 waiver of the post-soak precondition — the file stays at `docs/spec/models/closureEvidence.qnt` as the full predecessor record (header banner records the archival; zero model text deleted, zero checks removed); survivors core + 2 kept pins carried forward unchanged; the B9 corner carried with seed `0xffbfc9ac0c85df5b`, machine-checked via `quint-closure-corner-failover-duo-b9` (row 2); the 15 unwired calibration retirement notes remain valid as-is; pre-prune wiring at D7 commit `94996482b` (identify by subject after rebases). **RDC-5 addition rider (A4, bughunt wave, 2026-06-03):** one standalone module `closureEvidenceReapTruncate` APPENDED to the archived file (additions-only; archived module text verbatim) — the reap-truncation invariant `vouchedImpliesAllDurableChildrenProduced` + pre-fix flip `calibReapNoTruncate`, wired `quint-closure-reap-truncate-{holds,calib}` (calib seed `0xc214b66a0b0eb6b0`); posture note at the closure-evidence map's A6-archive block |
| 4 | **Frozen-contract addendum** — `executor-invariant-map.md`'s frozen pull-protocol contract (:2361–2536) records the materialization PullAssignment kind/`executor_instance` addendum in FINAL form | CLOSED (2026-06-02, this commit): the "Materialization addendum (final, post-D′)" subsection appended to the T-0e.6 contract section — the wire-deltas table, the four semantic rules (BC-1 identity, kind authorization, establishment kind-partition, one-winner arbitration) with record pointers into this map + `sched.materialize.*` / `store.materialize.executor`, and the extends-never-modifies statement (pre-existing rows unchanged) |
| 5 | **Item I — decision-5 store scaling** | LANDED/INTEGRATED (Items I/S/T integrated 2026-06-02; commissioned owner 2026-06-01): the full scope — backlog gauge `c64ba82e6`, KEDA ScaledObject (three triggers, `open_attempts` re-key) + ComponentScaler CR removal + the vm-substitute-scale decoupling `ebd7def0f`, the controller-map delta entry (controller-invariant-map.md "Item I entry") (harden-store memo §6.2); CLOSED (2026-06-02, this commit): landed form counter-signed at the item-I landing review (controller map delta entry SIGNED 2026-06-02); lineage re-resolution of the cell's pre-rebase hashes — backlog gauge = `1bd2ecf5c`, CR removal = `c9a9d163e` |
| 6 | **Item S — store ingest progress/stall hardening** | LANDED/INTEGRATED (Items I/S/T integrated 2026-06-02; commissioned owner 2026-06-01): store-side only as scoped — manifests progress columns `cb98b09eb`, placeholder-heartbeat download progress `84c699e2e`, owner-side stall abort + download-scoped reclaim `26c59ff7d` (harden-store memo §6.2); CLOSED (2026-06-02, this commit): Item S follow-through complete — §6.2 forced-stall VM subtest (tc-netem) landed `954989dd8`; helm values-gated RIO_SUBSTITUTE_STALL_SECS landed `045ddcdd5` (memo §3.1 row 26 helm half) |
| 7 | **Item T — conversion visibility & strictness** | CLOSED (2026-06-02, this commit): observability half landed earlier — conversion counter + paged alert `RioSchedulerMaterializationConversions` (`17e8de205`) on the intact PD-20/MD-D1 surface; strictness knob landed DEFAULT-OFF `9e410d174` per the owner's implement-now ruling (owner bundle 2026-06-02: `conversion_requires_worker_charge` + `conversion_min_park_dwell_secs`, durable `park_began_at` carrier, migration 083); flipping the default remains an operational act gated on `RioSchedulerMaterializationConversions` alert evidence (harden-store trigger 1) |
| 8 | **Standing owner line items, predecessor map** — closure-evidence Phase-2 counter-signature line items 5–8 (the decision-3 residual; the C5/CE-7 residual row; the trailing-EM closures; the admit_pull extraction scope) | CLOSED (2026-06-02, this commit): counter-signed owner 2026-06-02 — items 5–8 signature blocks applied at the closure-evidence decision-3 provenance row + line-items section; the Item I controller-map delta entry signature collected at landing review |
| 9 | `setup_with_mock_store_materialization_enabled` alias fold-in (~30 call sites onto `setup_with_mock_store`) | CLOSED (2026-06-02, this commit): alias folded `83a8c12db` — 35 call sites onto `setup_with_mock_store`, alias deleted, test count unchanged |
| 10 | **Materialization-kind establishment deadline (post-ledger)** — can a claimed materialization attempt sit unestablished forever when its executor dies unreported? | CLOSED-WITH-RATIONALE (2026-06-02, this commit): the answer is NO — see "Post-ledger closure — materialization-kind establishment deadline" below; no code change, no new knob |

### Post-ledger closure — materialization-kind establishment deadline (2026-06-02)

**The decisive question** (owner brief, investigate-first): can a
materialization job sit pending-establishment forever with no timer or tick
reaping or re-evaluating it? **The answer is NO** — a claimed materialization
attempt whose executor dies unreported CANNOT wait for establishment
unboundedly. Every link of the bounding chain is in code today (line numbers
as of this commit):

1. **The claim mints a finite deadline anchor.** The shared pull mint
   computes the solved deadline BEFORE the kind branch
   (`mint_and_deliver`, rio-scheduler/src/actor/pull.rs — the
   `solve_intent_for(...).deadline_secs` read precedes the kind mapping)
   and persists it for BOTH kinds via `mint_pull_attempt_fenced`
   (`drv_executions.deadline_secs`). `SolvedIntent.deadline_secs` is u32
   (state/derivation.rs; the unfitted-probe fallback in sla/config.rs has
   the same width — `f64::from` compiles only for ≤32-bit integers, so
   finiteness is structurally enforced), and the node must be in the DAG at
   mint, so every fresh materialization attempt row carries `Some(deadline)`.
2. **The establishment sweep is kind-shared and tick-driven.**
   `list_open_pull_attempts` has NO kind filter (db/open_attempts.rs —
   `COALESCE(e.attempt_kind,'build')`; the module doc: every consumer reads
   it unfiltered). `tick_sweep_open_pull_attempts` (actor/housekeeping.rs)
   runs on every leader tick (`spawn_periodic("tick-loop", cfg.tick_interval)`,
   default 10 s).
3. **The window is finite and can only widen.** Expired ⟺ age >
   max(persisted dispatched deadline, sweep-time re-solve) +
   `establishment_report_slack` (default 120 s). The re-solve may only
   WIDEN the window; a node evicted from the DAG falls back to 0.0 — the
   sweep then over-fires, never hangs.
4. **The kind branch establishes and re-arms.** `establish_open_pull_attempt`
   routes kind=materialization into `establish_materialization_attempt`:
   one fenced transaction closes the attempt and appends the
   `materialization_infra` Scheduler-party ("unreported") charge
   (`close_materialization_attempt`, exec_id terminal-row-wins idempotence),
   re-arms the job pending claimable (`rearm_materialization_job` — "leave
   the job pending"), and requeues the node (`reassign_derivations`). No
   adopt arm, never an executor-crash charge (BC-2/BC-3; executor map,
   Materialization addendum rule 3).
5. **Failure and failover do not break the chain.** A failed close leaves
   the attempt open for the next tick ("the establishment sweep remains the
   backstop"); a failed sweep READ is likewise tick-retried over the same
   durable view; a fenced close is the deposed-leader case, and the new
   leader's sweep reads the same durable PG rows ("The sweep reads durable
   rows — not an in-memory claim a one-shot timer can forget",
   scheduler.typ); recovery rebuilds the job view including claim holders
   and park expiries.
6. **The spec already binds it; the pins exist.**
   `sched.attempt.establishment-window+3` ("MUST visit every open attempt
   ... on every sweep" — kind-agnostic) and `sched.materialize.settlement`
   (claimed ⇒ report-or-establishment). Test pins:
   `establishment_writes_materialization_infra_never_adopts`,
   `flag_on_queued_mint_crash_between_commit_and_transition_recovers`,
   `flag_on_every_job_state_has_armed_action`; model pin:
   `unresolvedJobAlwaysArmed` across the five materialization regimes
   (Phase C′ stage record).

**Settlement totality across all three job states** (so the closure holds
under either reading of "pending-establishment"): pending (unclaimed) has
no deadline BY DESIGN — claimable is the armed action, with the backlog
gauge published per tick and Item I's KEDA scaling as the visibility and
capacity response (ledger row 5); claimed is bounded by
deadline + slack + tick as traced above; parked has the per-tick PD-20
re-evaluation, the finite durable backoff (cap 900 s), and the MD-D1
stalled alert.

**The two deliberate residuals** (recorded as record, NOT defects — owner
ratified 2026-06-02, §6 block C):

> **Residual (a) SUPERSEDED — reversed by the bughunt fix wave under its
> own recorded terms (owner-signed 2026-06-03, bughunt wave §5 Q5).** The
> block below is preserved verbatim as the historical record; the three
> preconditions it set for exactly this change were each discharged:
> red-first justification (test
> `establishment_only_charges_park_at_max_attempts` — RED on the pre-fix
> tree: the crash-loop re-listed forever), model recalibration per C′
> delta row 3 (the contract re-derivation of `materializationJob.qnt`:
> the never-parks encoding REMOVED from the main model and preserved as
> the expect-violation calibration `mat-establish-never-parks`;
> `establishmentParkRun` + `budgetSoundness` are the contract pins; all
> six holds regimes re-measured at unchanged budgets), and owner
> sign-off (Q5, 2026-06-03 — "silence ratifies the REVERSAL" was NOT
> relied on; the reversal was affirmatively signed). Parking is now
> PARTY-BLIND: `charge_materialization_infra` fuses BOTH charge channels
> with the kernel park verdict (`rio_retry_kernel::materialization_counters`
> — the per-job 085 window), so an establishment-only crash-loop parks
> at the budget and joins the MD-D1 stalled population (the alert
> population re-key is recorded at the MD-D1 row and the
> FINAL-CONSOLIDATION row below; the alert keeps the existing gauge, no
> new label, per the Q5 sub-decision). Residual (b) STANDS unchanged.

- *(a) Establishment never parks.* ***[SUPERSEDED 2026-06-03 — see the
  block above.]*** The park/budget decision rides ONLY the
  worker-reported InfraFailure consumption arm, while establishment-written
  rows DO count toward that budget (OQ1 amendment 1: "worker-reported AND
  establishment-written — both channels charge the same budget"). A store
  replica crash-looping without ever reporting therefore yields repeated
  BOUNDED claim→establish cycles — each ≤ deadline + slack + tick, armed at
  every step, never parked; the first worker-reported charge that lands
  parks promptly under the party-blind fold. This matches the C′
  delta-encoding row 3 wrong-verdict record exactly: a model that parked or
  reset at establishment "would have verified a park cycle that cannot
  stall" — changing the posture would re-key the MD-D1 stalled-alert
  population and invalidate the calibrated model. The cycle count is
  unbounded in REPETITIONS though bounded per cycle; its visibility
  surfaces are the `drv_attempts` ledger rows, k8s CrashLoopBackOff on the
  replica, and OA2's controller-side wedge clustering over per-node
  attempt-deadline expiries.
- *(b) The window anchors to the build-SLA solve* (FP-4(a), findings row
  14). A download outliving deadline + slack is established mid-flight: one
  infra charge plus a re-claim, with terminal-row-wins dropping the late
  report against the closed attempt. Item S's progress/stall machinery is
  store-side by scope and deliberately does not feed this window.

NO mechanism is missing beyond the reversal recorded above; NO
materialization-specific deadline knob exists or is needed. ***[The "out
of scope" coda below is part of the superseded record:]*** Any future
establishment-side park ladder or per-kind window is a behavior change
requiring red-first justification, model recalibration (C′ delta row 3),
and owner sign-off — out of scope for this closure. *(Those terms were
met in full by the 2026-06-03 reversal — see the supersession block.)*

---


---

## 2. Round 2 program close-out (the program index)

> Origin: substitution-replacement-invariant-map.md § "Round 2 program
> close-out (2026-06-02)", verbatim.

## Round 2 program close-out (2026-06-02)

Round 2 (2026-05-29 → 2026-06-02, post-main-rebase, on `formal-sprint`)
ran two verification campaigns, one replacement campaign, three candidate
adjudications, the Track C small items and a CI-lane rework. This is the
program index; every line points at a permanent record.

**Verification campaigns:**

- **Closure evidence** (Track A, Phases 0/1/2): four defect classes fixed
  red-first (C3 wrongful terminal failure; D16 settlement; the
  L3-residual recovery-condemnation scoping; the D14/D15
  deposed-believer windows → the uniform claims-floor fence) and one
  refuted-with-record (L3 as premised); calibrated model, 29 wired
  checks, the CE-1..81 acceptance table, the rio-evidence-kernel CBMC
  extraction. Record: `closure-evidence-invariant-map.md` (post-D′ the
  wired family is the survivors core + 2 kept pins).
- **Gateway session/connection lifecycle** (Track B, verify-only): zero
  as-built defects at model resolution; 34 properties, 40 wired checks,
  all 17 encodable calibration candidates falsify; closed at Phase 0
  with counter-signatures. Record: `gw-session-invariant-map.md`.

**The replacement** (Track A stages A3–A6 — this map): the scheduler's
detached substitution walk replaced by store-owned materialization jobs
across four phases (dormant → flip → model+calibration → deletion); five
product bugs + one design correction fixed red-first on the way; net
−16,075 lines at D′; 21 invariants, 43 wired checks, the calibration
transfer at 100%.

**Candidates adjudicated and retired:**

- **DAG authority inversion** — RETIRED on the merits (every found
  defect reproduces under PG authority; the fence is required either
  way); owner-ratified with named re-open triggers. Record:
  `dag-authority-rescope-memo.md` §7.
- **Build-event sourcing (C4)** — the memo's RETIRE verdict was
  owner-OVERRIDDEN to EXECUTE: the WatchBuild resumability layer deleted
  (snapshot-first replacement, net −591 lines, migration 077; rules
  `sched.watch.snapshot-first` / `gw.reconnect.snapshot-resync`); the
  memo's latent-defect disclosure (post-terminal BuildProgress) fixed
  red-first via ce-phase2. Records: the C4 re-scope memo; the Track B
  map's coordination note.
- **harden-store FINAL-substitution-race-fix (rev 5, never landed)** —
  RATIFIED RETIRED 2026-06-01: materialization is that design's own D5
  alternative built out, ~80% of its scheduler half overtaken; the
  survivors are commissioned as Items S and I, with Item T
  observability-first; MD-D5 + M1/M2 land in the consolidated checklist
  above; five owned re-open triggers. Record:
  `harden-store-reconciliation-memo.md`.

**Small items (Track C):** C1 controller recomputable-cache cleanup
(net −136), C2 builder envelope re-homing, C3 `dispatchMode: Stream`
retirement (net −281, migration 076). **CI (Track E):** the dedicated
formal check lane (sharded gen-matrix fan-out); first green
formal-sprint GHA run.

End state: one substitution mechanism (store-owned materialization
jobs); 565 spec rules; 32 VM attrs; 246 quint check attrs; 9 + 8 kernel
CBMC harnesses; every campaign closed with a permanent calibrated
falsifiability stack. The program's open work leaves through the
follow-up ledger above — each row with an owner and a record.

---
