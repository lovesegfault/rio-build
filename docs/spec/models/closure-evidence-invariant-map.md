# Closure-evidence lifecycle invariant ↔ spec-rule map

Working artifact for the `closure-evidence-formal` campaign (the
`topdown_pruned` / `closure_hole` lifecycle). Campaign design:
`closure-evidence-formal-design.md` (A2-approved revision, adversarial review
run wf_b88941b2-973 incorporated). Verification subject and fix target:
`formal-sprint` @ `cccb4d778`; calibration source: `origin/main` @
`dfe9a5569`. The executable counterpart of this map is
`docs/spec/models/closureEvidence.qnt`, which does not exist yet — Phase 0a
is deliberately model-free.

**Status: Phase 0a (spec audit) complete; Phase 0b (model construction)
pending.**

## Phase 0a spec-audit record

New rules: `sched.evidence.closure-hole`, `sched.evidence.durability`,
`sched.evidence.settlement` (the last intentionally uncovered — see the
posture records below). Amended and version-bumped, with annotations
re-pointed in the same commits: `sched.merge.substitute-topdown+11`,
`sched.db.derivations-gc+3`, `sched.admin.clear-poison+2`. Rationale-prose
records (non-normative): the as-built fencing posture for evidence writes,
the accepted residuals (expired-at-load poison, lost-hole-stamp /
builds-row-purge conjunction, GC-after-vouch re-detection shapes), the
spawn-intent refusal churn, and the in-memory-only recovery Substituting
reset. Errata: `M_064` doc-const (frozen-header staleness; GC-erasure
preconditions).

## Invariant ↔ rule map (filled in at Phase 0b/0c)

Verdict legend (house format): COVERS / PARTIAL / GAP / CONTRADICTION.

### Group A — safety (A1–A22)

| # | Property | Rule(s) | Verdict | Notes |
|---|---|---|---|---|
| _pending Phase 0b_ | | | | |

### Group B — missing families (B1–B10)

| # | Property | Rule(s) | Verdict | Notes |
|---|---|---|---|---|
| _pending Phase 0b_ | | | | |

### Group C — permissiveness (C1–C5)

| # | Property | Rule(s) | Verdict | Notes |
|---|---|---|---|---|
| _pending Phase 0b_ | | | | |

### Group L — settlement, armed-state form (L1–L3)

| # | Property | Rule(s) | Verdict | Notes |
|---|---|---|---|---|
| _pending Phase 0b_ | | | | |

## Contradiction / posture records

| Item | Record | Disposition |
|---|---|---|
| Fencing posture for evidence writes (D14/D15) | Entry-time leader gates only; no SQL fence on any evidence write; the only fenced statements are the three attempt-ledger transactions; the MergeDag handler is ungated past the SubmitBuild enqueue guard. Recorded as rationale prose after `sched.evidence.durability`. | Owner decision deferred to Phase 0c evidence (the A17/A18 stale-tenure probes); no fencing requirement is pre-committed. |
| D16 present-but-tried limbo cell | `sched.evidence.settlement` added (owner adopted the obligation); the as-built dispatch probe violates it, so the rule is intentionally uncovered (`tracey query uncovered`) until the Phase-1 fix lands red-first. | Settling arm chosen by the model (L2); fix in Phase 1. |

## Verify-marker status (Phase 0a)

- `sched.evidence.closure-hole`, `sched.evidence.durability`: impl markers at
  the inventoried sites; verify markers on the existing unit tests that
  already pin the behaviors (merge/recovery closure-hole battery, the
  PG-persistence and stamp-rollback tests).
- `sched.evidence.settlement`: no impl, no verify (intentional — see above).
- §7.12 zero-verify adjacents — left unannotated, recorded here instead:
  `sched.merge.stale-substitutable` (no existing test exercises the
  stale-Completed-but-substitutable stays-completed path; the nearby tests
  cover the newly-merged substitutable matrix and the reset direction),
  `sched.merge.ca-fod-substitute` (no test pins the FOD-in-path-lane
  partition; the CA tests cover the realisations lane), and
  `sched.recovery.poisoned-failed-count` (recovery tests load poisoned rows
  and bound resubmits, but none asserts the recovered build counts them in
  `failed` via `check_build_completion`). Writing those tests is follow-up
  work for Phase 1/2, not the 0a spec audit.

## Stage records

Later phases append here (0b: vertical-slice measurement + model
construction; 0c: exhaustive checks, expected-fail witnesses incl. the L2 /
A17 / A18 traces; 0d: calibration verdict table; 1: red-first fixes; 2:
acceptance table; close-out: deployment-checklist deltas and
counter-signatures).
