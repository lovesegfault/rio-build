# Lease invariant map — bughunt-wave F1 (rio-lease discipline)

Campaign record for the bughunt fix wave's F1 workstream
(`bughunt-fix-specs.md` bucket F): the rio-lease findings, their
structural closures, and the formal coverage that pins them. The
leader-election protocol's pre-existing record (the four regimes, the
TLA+-port lineage, the asymmetric-TTL boundary measurements) lives in
`leaderElection.qnt`'s module comments and the introducing commits;
this map records the F1 delta and its two documented rationales.

## Finding → closure table

| finding | mechanism | structural closure | formal pin |
|---|---|---|---|
| bug_096 | suspend straddling an IN-FLIGHT renew: the Ok arm stamped `last_successful_renew` at RESPONSE arrival, erasing the suspended interval — zombie leader until the next failed round-trip | `RenewAnchor` (single mint site, before the attempt await; anchor ≤ send ≤ commit) + `BlindClock` (the only write API consumes an anchor) — a post-response stamp has no API. Response-anchoring const assert deleted (premise gone); FENCE_MARGIN/clock.rs/model-anchoring docs re-derived | `leaderElectionSuspend` regime (headline `boundedDualLeadership`; see rationale (a)) + `noSuspendStraddle` witness + calibration `lease-096-response-anchor` (response anchoring MUST violate boundedDualLeadership — exhaustive backend; the violation needs the full straddle interleaving) + production red `fence_fires_after_suspend_straddles_inflight_renew` |
| bug_387 | the graceful-release gate read `was_leading` — the bool the local self-fence clears — so fence-then-SIGTERM skipped `step_down` while the apiserver still named us | `LeaseStanding`: `believes` (edge detection; fence clears it) split from `held_unsuperseded` (release gate; cleared only by a completed not-leading round). Private fields: the fence cannot touch the hold, the gate cannot read belief | `leaderElectionShutdown` regime (NEW invariant `gracefulHandover`) + `noFenceThenExit` witness + calibration `lease-387-belief-gate` (belief gating MUST violate gracefulHandover) + 2 CBMC harnesses over the standing algebra (`lease_standing_*`, length-8 event sequences) + production reds (`shutdown_after_self_fence_still_releases_lease`) |
| merged_bug_138 | leader marks were edge-triggered over an ENUMERATED writer set; any falsifier outside it (foreign sweep racing a re-acquire, kubectl, future actors) left the load-bearing leader label wrong until the next leadership transition; the sweep could strip a just-re-acquired holder; the rebound never re-dirtied; the in-flight task outlived the loop | `sched.lease.marks-verify` (NEW rule): bounded-cadence verification every `MARKS_VERIFY_EVERY` = 12 rounds — the writer enumeration stops being load-bearing. Riders: rebound re-dirty (`sched.lease.rebound+3`), holder-aware sweep with same-pass Lease read (`sched.lease.deletion-cost+3`), loop-exit abort of the in-flight task | NEW `leaderMarks.qnt` (headline `marksDivergenceBounded`: divergence is discovered or younger than the cadence; `wrongSince` maintained by ONE derived helper in every action — no enumerated stamp list to drift) + `noStrip` witness + calibration `lease-138-edge-only` (the verify pass removed MUST violate) + `verifyConvergesRun` + production reds (rebound / sweep-spares-holder / nth-renew-verify) |

## Regime / witness / calibration inventory (measured at the introducing commits)

| check | kind | measured |
|---|---|---|
| quint-leader-election (base) | exhaustive, unchanged | 18,842,319 generated / 3,225,441 distinct — BIT-IDENTICAL to pre-F1 (state-space identity at zero fault budgets) |
| quint-leader-election-deletion | exhaustive, unchanged | 140,175,863 / 23,352,605 — identical |
| quint-leader-election-asymmetric | exhaustive, unchanged | 10,673,727 / 1,833,297 — identical |
| quint-leader-election-pg-faults | exhaustive, unchanged | 125,058,351 / 20,120,417 — identical |
| quint-leader-election-suspend | NEW exhaustive regime | 41,602,525 / 7,171,509 distinct, depth 44, 30s @ 192 workers |
| quint-leader-election-shutdown | NEW exhaustive regime | 17,410,617 / 2,978,409 distinct, depth 43, 15s |
| quint-leader-election-witness-{suspend-straddle, fence-then-exit} | expect-violation | both find their violations (reachability evidence) |
| quint-leader-election-runs-{suspend, shutdown} | named-run replay | suspendStraddleFencesRun; fenceThenSigtermReleasesRun + conflictThenSigtermSkipsRun |
| quint-leader-marks | NEW exhaustive | 20,691 / 7,929 distinct, ~1s |
| quint-leader-marks-witness-strip | expect-violation | strip reachable |
| quint-leader-marks-runs | named-run replay | verifyConvergesRun |
| quint-lease-calib-096-response-anchor | expect-violation pin | violates boundedDualLeadership at 428,930 distinct explored, 4s (exhaustive backend — the straddle interleaving is past a sim-budget hunt) |
| quint-lease-calib-387-belief-gate | expect-violation pin | violates gracefulHandover at 4,591 distinct, ~1s |
| quint-lease-calib-138-edge-only | expect-violation pin | violates marksDivergenceBounded at 1,736 distinct, <1s |

## Documented rationales

**(a) `neverDual` is deliberately EXCLUDED from the suspend regime.**
The triage sketch asked for neverDual under suspend; the code-grounded
truth is weaker and the model states it honestly: a parked believer (a
host suspended mid-await holds `is_leader` frozen — it takes no
actions, discovers nothing) plus a thief past its steal threshold is a
genuine dual-belief state, reachable under EITHER anchoring. What the
anchoring decides is BOUNDEDNESS: with attempt-start anchoring the
post-read believer is at/past its own fence deadline — discovery
disjunct (1) armed, `boundedDualLeadership` HOLDS (and the calibration
shows response anchoring breaks exactly that). The
resume-to-first-tick gap (clock.rs residual) is the same statement at
the production level, and `loopIntervalResumeBounded` is its
model-side form (the exact `loopInterval` tripwire stays in the four
legacy regimes). The generation fence
(`sched.lease.generation-fence+3`) remains the correctness backstop
throughout — the blast radius of every lease-side property here is
ops/availability, not execution correctness.

**(b) The sweep TOCTOU residual is accepted and bounded by
marks-verify.** The holder-aware sweep reads the holder and patches
peers in one reconcile pass but not one transaction; a holder change
between the read and a peer patch can still strip a just-re-acquired
leader. Closing it transactionally is not available (two apiserver
objects); the verify cadence converts the damage from "until the next
leadership transition" to "≤ MARKS_VERIFY_EVERY rounds + one
reconcile" — the victim's own loop re-discovers the strip. Recorded in
`sweep_peer_leader_marks`'s residuals doc and encoded in
`leaderMarks.qnt` (the spawn-to-complete window is explorable; the
invariant is divergence-DISCOVERY boundedness, which is exactly the
production contract).

## Production test cross-reference (red-first, all recorded in the F1 commit bodies)

- 096: `fence_fires_after_suspend_straddles_inflight_renew` (Park/release_parked mock primitive), `blind_clock_window_algebra`
- 387: `shutdown_after_self_fence_still_releases_lease` (red), `shutdown_after_observed_supersession_skips_release` (preserved-skip guard), CBMC `lease_standing_fence_never_clears_held` + `lease_standing_release_gate_iff_acquired_unsuperseded`
- 138: `rebound_marks_leader_marks_dirty` (red), `peer_sweep_spares_current_lease_holder` (red), `nth_renew_verify_redirties_on_external_strip` (red), `aborting_inflight_marks_task_releases_slot_and_keeps_dirty`, `marks_match_quadrants`, `peer_sweep_targets_excludes_own_name_and_holder`

---

# Bughunt2-wave slot 8 appendix (lease-builder riders)

Campaign record for the bughunt2 fix wave's slot-8 workstream: the
rio-lease/rio-scheduler lease findings, their structural closures, and
the formal coverage. Code closures landed in the slot's code half
(commits `0a9914355`..`ebbf9a997`); this appendix records the formal
delta and its rationales.

## Finding → closure table

| finding | mechanism | structural closure | formal pin |
|---|---|---|---|
| bug_181 | `marks_dirty: AtomicBool` — the reconcile success path's `store(false)` clobbered any dirtying that landed between the task spawn and the clear; the is_leader-ordering premise and the polarity re-check were partial patches | `DirtyGen{marked, cleared}`: the loop snapshots `marked` at spawn, success clears THROUGH the snapshot — any post-snapshot mark stays dirty by arithmetic; all six `store(true)` sites became `mark()`; premise comment + re-check DELETED; kani `dirty_gen_mark_after_snapshot_never_cleared` (≤8 events) | `leaderMarks.qnt` REWORKED to the generation encoding with the completion SPLIT into `reconcilePatched`/`reconcileClear` (dirtying sites interleave with both boundaries) + NEW invariant `notClobbered` (live-written by the clear from its own transition values) + cause-tagged `marksDivergenceBounded` (edge/write-caused divergence gets NO age window) + `reboundRedirty` (the future-writer class) + witness `noRebound` + calibration `lease-181-bool-clear` (clear-all restored MUST violate notClobbered) + `clearKeepsPostSnapMarkRun` |
| merged_bug_212 | the rebound was delivered as a bare `LeaderAcquired`: the LEADER_EDGES cost-latch lose cell never ran, `cost_was_leader` stayed true across the unobserved holder change, the next leading tick skipped the reload and persisted the deposed tenure's prices | `LeaseHooks::on_rebound()` REQUIRED + `LeaderEdge.rebound: ReboundPolicy` required field + `ActorCommand::LeaderRebound` → Compound lose cells THEN acquire; writer-census include_str! test | NEW `costLatch.qnt` (prelude three arms + lose cell + foreign tenure exactly when the real lease is not ours, incl. the rebound gap's nondet `during`) — headline `noStalePersist` + witnesses `noPersist`/`noForeignPersist` + calibration `costlatch-212-acquire-only` (lose cell skipped MUST violate) + `reboundReloadsBeforePersistRun` |
| merged_bug_303 | a renew PUT that committed while the composition was cancelled after transmit stamped nothing; the mid-band latency band fenced the holder while its committed garbage writes re-anchored every standby's staleness clock — the unbounded leaderless livelock | `renew_phased` (1.5s+1.5s const-asserted belt tiling) + `RenewOutcome` facts + `UnconfirmedPut` ledger (oldest-kept) consumed ONLY by own-commit evidence stamping at the LEDGER's anchor; belief re-entry through the ordinary same-count acquire edge; NEW rule `sched.lease.cancelled-write` | `leaderElection.qnt` dropped-write plane: ledger + evidence-cursor vars, `renewSendDropped` + `fetchObservesOwnCommit` actions, the evidence-forcing tick cap, completed-round wholesale consumption; NEW regime `leaderElectionDroppedWrite` (mid-band step) holding the full set INCLUDING `neverDual` (the soundness arbiter for anchor-stamping) and `blindHolderBounded` (see rationale (d)) + witness `noDroppedCommit` + calibration `lease-303-blind-timeout` (evidence rule removed in its three pieces MUST violate) + `livelockBrokenRun` |
| bug_241 | post-uploader-death batch discards vanished uncounted (zero counter increments) — the producer's loss was invisible to the disclosure plane | `UploadSink{Open(Sender), Lost(DiscardLedger)}` — the only path that drops a batch is the ledger method; SendError's bounce seeds it; Drop routes through `disclose()` as `uploader_dead`; disjoint from LossGuard by construction | `logService.qnt` producer plane (const-gated per the file's own calibration convention): `uploaderDies` (EARLY death — the drain-deadline abandon requires a finished build, so the refused class was previously unreachable), `refusedFrom` watermark, `producerCountedBelow` advanced at `buildFinishes` (the Drop) + NEW invariant `producerLossCounted` + witness `noRefusedLines` + calibration regime `logServiceCalibProducerBlind` (silent Drop MUST violate) + `producerDeathDisclosedRun` |

## Inventory additions (measured at the introducing commits)

| check | kind | measured |
|---|---|---|
| quint-leader-marks (reworked) | exhaustive | 538,830 generated / 187,322 distinct, depth 26, ~4s — now also carries `notClobbered` |
| quint-leader-marks-witness-rebound | expect-violation | rebound reachable |
| quint-lease-calib-181-bool-clear | expect-violation pin | violates notClobbered, ~1s |
| quint-lease-calib-138-edge-only (updated) | expect-violation pin | STILL violates marksDivergenceBounded under the new encoding, ~3s |
| quint-cost-latch | NEW exhaustive | 1,298 / 586 distinct, depth 12, <1s |
| quint-cost-latch-witness-{persist, foreign} | expect-violation | both reachable |
| quint-costlatch-calib-212-acquire-only | expect-violation pin | violates noStalePersist, <1s |
| quint-leader-election-dropped-write | NEW exhaustive regime | 2,167,991 / 619,166 distinct, depth 35, ~6s — neverDual + blindHolderBounded both hold |
| quint-leader-election-witness-dropped-commit | expect-violation | drop reachable |
| quint-lease-calib-303-blind-timeout | expect-violation pin | violates blindHolderBounded, ~4s |
| quint-log-service-producer | NEW exhaustive regime | 161,780 / 53,159 distinct, depth 20, ~3s |
| quint-log-service-witness-refused + calib-producer-blind | expect-violation | refused lines reachable; silent Drop violates |
| six pre-existing leaderElection regimes | byte-identity re-measure | generated/distinct counts EXACTLY equal pre/post (base 3,225,441 / deletion 23,352,605 / asymmetric 1,833,297 / pg-faults 20,120,417 / suspend 7,171,509 / shutdown 2,978,409 distinct). TLC's reported depth jitters ±1 across runs of the IDENTICAL file under parallel workers (measured 44/44/43 thrice on asymmetric) — state counts are the identity signal |
| four pre-existing logService base regimes | byte-identity re-measure (R15: this model landed LAST) | EXACTLY equal (base 26,304 / redispatch 4,383,193 / resend 12,132 / sweep 10,278 distinct) |

## Documented rationales (slot 8)

**(b) `TASK_TIMEOUT` is a NEW load-bearing constant in leaderMarks.**
The split-phase completion surfaced a real bound the old atomic
completion hid: a strip landing between the API write and the clear is
undiscoverable until the slot frees, so the divergence bound's parking
term is exactly the production patch/call timeout
(rio-lease/src/lib.rs:2137/:2311). The old "within VERIFY_EVERY" claim
was an artifact of the atomic write repairing in-flight strips by
construction. The first encoding without the term was red against the
live model — kept, with the widened `VERIFY_EVERY + TASK_TIMEOUT`
window, as the honest statement.

**(c) The evidence round deliberately does NOT run bump-confirmation.**
`fetchObservesOwnCommit` re-enters belief through the ordinary
same-count acquire edge (generation fetch_max provably a no-op: the
holder never changed, and a crash wipes the in-memory ledger so a
reset-generation incarnation can never reach the action), but
`sched.recovery.bump-confirm` stays a completed-round property — the
evidence fetch alone never confirms a claim-target bump. Pre-fix this
regime fenced outright, so a gated recovery strictly dominates. The
narrowing is encoded in the model (no genHW writes on the evidence
action) and documented at the production arm.

**(d) `blindHolderBounded`, not "leaderless bounded".** The first
encoding asserted global leaderlessness was bounded and TLC correctly
refused: a standby's steal is never poll-obligatory, so global
leaderlessness is a LIVENESS property a safety checker cannot force.
The honest safety form binds the EVIDENCE-STARVED BLIND HOLDER window
(holder fenced ∧ own-commit evidence observable ∧ unobserved), which
the evidence-forcing tick cap genuinely closes within one poll.
BLIND_HOLDER_BOUND = 8 is boundary-measured (7 violates via the
3-tick forcing slack + 5-tick peer skew schedule; 8 holds).

**(e) The logService twin is a const-toggled regime, not a separate
override file.** The successor brief sketched
`calibration/log-241-producer-blind.qnt`; logService's own calibration
convention is CALIB_* consts with in-file regime modules (22 prior
instances), which also avoids re-framing 30+ actions in an override
file. `logServiceCalibProducerBlind` follows the house pattern.
