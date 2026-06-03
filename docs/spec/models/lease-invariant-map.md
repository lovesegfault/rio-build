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
| merged_bug_138 | leader marks were edge-triggered over an ENUMERATED writer set; any falsifier outside it (foreign sweep racing a re-acquire, kubectl, future actors) left the load-bearing leader label wrong until the next leadership transition; the sweep could strip a just-re-acquired holder; the rebound never re-dirtied; the in-flight task outlived the loop | `sched.lease.marks-verify` (NEW rule): bounded-cadence verification every `MARKS_VERIFY_EVERY` = 12 rounds — the writer enumeration stops being load-bearing. Riders: rebound re-dirty (`sched.lease.rebound+2`), holder-aware sweep with same-pass Lease read (`sched.lease.deletion-cost+3`), loop-exit abort of the in-flight task | NEW `leaderMarks.qnt` (headline `marksDivergenceBounded`: divergence is discovered or younger than the cadence; `wrongSince` maintained by ONE derived helper in every action — no enumerated stamp list to drift) + `noStrip` witness + calibration `lease-138-edge-only` (the verify pass removed MUST violate) + `verifyConvergesRun` + production reds (rebound / sweep-spares-holder / nth-renew-verify) |

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
