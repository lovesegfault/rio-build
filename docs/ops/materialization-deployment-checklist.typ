#import "/lib/rio.typ": *
#show: rio.with(domains: none)


The consolidated deployment index for the substitution-replacement
(materialization) rollout — the single operator index across the round-2
campaigns' checklist rows, relocated from the retired
substitution-replacement invariant map's FINAL CONSOLIDATION (the campaign
archive is `docs/spec/models/substitution-replacement-records.md`). One
line per row: the final state an operator inherits.

= Closure-evidence rows (CE-D)

- *CE-D1 fenced-write metric + leader alert* — SURVIVES, extended:
  job-table and wanted-relation writes share the claims-floor fence and the
  #(refs.metric)("rio_scheduler_evidence_write_fenced_total") counter; the
  #(refs.alert)("RioSchedulerEvidenceWriteFenced") alert ships in the chart. On a replica
  that just lost the lease, nonzero during failover IS the fence working;
  on the CURRENT leader it must be zero — alert on any sustained nonzero
  leader rate.
- *CE-D2 failover PG-flap alerts* — SURVIVES unchanged:
  #(refs.metric)("rio_scheduler_generation_claim_failed_total") and
  #(refs.metric)("rio_scheduler_generation_floor_read_failed_total") sustained nonzero
  means PG is flapping at failover time; the same metrics now also guard
  the job-write floor reads.
- *CE-D3 merge `FAILED_PRECONDITION` at failover* — SURVIVES unchanged:
  in-tx job creation rides the same fenced merge refusal (a fenced merge
  creates no jobs); clients re-submit against the current leader.
- *CE-D4 wrongful-fail-fast gone / resubmit guidance* — SURVIVES re-keyed:
  the single fail-fast site is consumption-settlement arm 3
  (four-conjunct), keyed on pruned job origin + the durable three-part
  classification; a resubmit mints a new job with a fresh one-shot.
- *CE-D5 poison-clear wakes spared parents* — SURVIVES re-keyed: survivors
  carrying unresolved jobs are job-armed; the reprobe lane creates
  reprobe-origin jobs; the promotion arm is untouched.
- *CE-D6 manual-target runbook* — RE-KEYED to the successor model: the
  standing manual targets are the five `materializationJob*Ex` conjunctions
  (coordinates and command shape: the `materializationJob.qnt` header
  MEASUREMENT block); the survivors core stays wired with its two pins.
- *CE-D7 AW1 lost-hole-stamp bound* — CLOSED vacuous-by-construction (no
  hole breadcrumb exists).
- *CE-D8 GC-after-vouch bounds* — SPLIT: shape (a) delivered by wired
  pin-at-ingest; shape (b) narrowed into the kept B9 guard.
- *CE-D9 expired-at-load poison residual* — SURVIVES unchanged, re-homed to
  the surviving recovery-time fenced write.

= Gateway rows (GW-D)

GW-D1 through GW-D5 survive unchanged — see
#cross-link("/ops/gateway-deployment-checklist.typ")[the gateway deployment checklist].

= Materialization rows (MD-D)

- *MD-D1 park alerting + runbook* — SURVIVES, population re-keyed (the Q5
  establishment-park reversal): the
  #(refs.metric)("rio_scheduler_materialization_stalled") gauge, the
  #(refs.alert)("RioSchedulerMaterializationStalled") alert (15 m) and the runbook are
  intact (same gauge, no new label); the population now INCLUDES
  establishment-only crash-loops (party-blind parking). A parked
  materialization job means UPSTREAM trouble, never build failure: builds
  wait visibly, and every parked job has Broken closure evidence (jobs with
  buildable closures are auto-resolved from-source within one tick).
  Runbook, two arms: (1) the upstream arm — check the tenant's upstream
  cache config/health (#(refs.metric)("rio_store_substitute_total") error rate, store
  logs); (2) the store-replica arm — a parked job whose ledger rows are all
  establishment-written (zero worker reports) with k8s CrashLoopBackOff on
  a store replica and NO #(refs.metric)("rio_store_substitute_total") error signal is the
  crash-looping-replica case; fix the replica, not the upstream. Builds
  resume on upstream recovery (park-expiry re-claim) or can be cancelled.
  Do NOT restart the scheduler to "fix" a park — the park is durable state
  and survives restarts by design.
- *MD-D2 / MD-D3 flag rollback / mixed-flag guidance* — RETIRED at D′,
  replaced by the §7.5 deployment-and-rollback posture: binary rollback
  through D′.1; #(refs.migration)("080_drop_walk_evidence") roll-forward only;
  binaries-first deployment
  order; bounded self-identifying transition residuals.
- *MD-D4 instance-binding skew posture* — SURVIVES: fail-closed across
  version skew; the binding check is unconditional post-D′.

= Post-wipe cold-start observation set (MD-D5, with M1/M2)

- *MD-D5* — the harden-store memo §3.4 P1--P10 predicates in
  materialization terms + the warm-phase capture (Item I's
  threshold-calibration seed); observation, never a gate; a P7 wall-clock
  miss adjudicates against P8's lever record before reading as
  harden-store trigger 3.
- *MD-D5/M1 pre-scale option* — zero-code belt-and-braces for the planned
  wipe (replica count multiplies executor lanes); record the
  pre-scale-vs-measure choice if Item I has landed; option, never a gate.
- *MD-D5/M2 store-at-floor observation* — record store ready-replicas vs
  the floor through the wave UNCONDITIONALLY — this keeps harden-store
  trigger 4 observable in exactly the branch where it arms.
