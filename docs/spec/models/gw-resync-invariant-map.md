# Gateway-resync / open-attempts invariant map (bughunt fix wave, workstream C1)

Status: LANDED with the terminal-capture workstream (2026-06-03).
Models: `gwBuildResync.qnt`, `openAttempts.qnt`. Wired checks: 2 holds
(TLC exhaustive, Tier-1) + 5 expect-violation calibrations.

## What the models cover

`gwBuildResync.qnt` — two derivations, one build, a lossy broadcast
ring, one watching gateway. The pivot is the KNOWLEDGE of loss: a
skipped emission sets `pendingResync` (the bridge's in-stream
`ResyncRequired`, one per Lagged streak), and the gateway's resync
(zero-backoff WatchBuild snapshot reconcile) restores the entire
display from the kinded running set. Terminal answers come from the
live event, the resident snapshot, or the durable builds row
(migration 087) — never fabrication.

| invariant | finding | meaning |
|---|---|---|
| `kindedSurfaceAgrees` | bug_144 | absent a pending resync, every display entry's family matches the scheduler's running kind |
| `noStuckDisplay` | bug_150 | absent a pending resync, every display entry's drv is running (one map, every sweep total over THE key set) |
| `tailCoverage` | bug_153 | absent a pending resync, tails are exactly the running BUILD set |
| `terminalVerdictNeverFabricated` | merged_bug_323 | the gateway never reports a fabricated outcome |

`openAttempts.qnt` — one derivation's open pull-mode attempt under
cancellation: the outbox-backed terminal persist (latch on Ok only,
leader-scoped, cleared on leadership loss) and the establishment
kernel's charge-free arm (`rio-evidence-kernel/src/establish.rs`).
Failover is honest: a non-durable cancel evaporates and the successor
may legitimately charge live-wanted work.

| invariant | finding | meaning |
|---|---|---|
| `cancelledNeverChargedAsCrash` | bug_347 | cancelled/absent-node work is never the charged party; a durable cancel is never followed by a charge |
| `openAttemptHasDriver` | bug_347 | an open attempt for a cancelled node always has a driver: the outbox, or the sweep's charge-free row |

## Calibrations (expect-violation — each pins an as-shipped design)

| check | frozen design | falsifies |
|---|---|---|
| `quint-gwresync-calib-two-map` | two independent display maps; the gone-reconcile sweeps only the build family | `noStuckDisplay` |
| `quint-gwresync-calib-no-signal` | broadcast-lag skips are scheduler-log-only; the gateway never learns | `tailCoverage` |
| `quint-gwresync-calib-kind-blind` | no wire kind; materializations get the uniform build treatment (display + tail) | `kindedSurfaceAgrees` |
| `quint-gwresync-calib-no-pg-fallback` | no durable terminal payload; post-cleanup attach fabricates | `terminalVerdictNeverFabricated` |
| `quint-openattempts-calib-charge-blind` | failed cancel persist dropped; sweep charges without consulting the node | `cancelledNeverChargedAsCrash` |

## Budgets (recorded at landing)

Both models are Tier-1: TLC exhausts each in seconds (gwBuildResync
~thousands of states at 2 drvs; openAttempts ≤36 states). Local
simulation evidence at landing: holds at 3,000 samples × 12–14 steps
(80–125 ms); every calibration falsifies within 8,000 samples × 14
steps (73–116 ms, shallow traces). No `serverHeapMb` override needed
(default 4 GiB converts both).

## None-sensible rationales (directive 2, recorded per the triage)

- **merged_bug_097 / merged_bug_302 / merged_bug_036 (settled
  payloads)**: single-actor in-memory capture-at-transition — no
  protocol concurrency to model. The pins are the Rust settled-equality
  battery (`test_settled_snapshot_equals_live_emit` per terminal arm,
  the cancel-reason round-trip, the timeout-watchdog
  `override_failure_build_level` unit) and the type system itself
  (`Lifecycle::Terminal(SettledBuild)` cannot exist without its
  payload; partial trio writes do not compile).
- **bug_304 (progress fields)**: a shared 4-tuple producer
  (`build_progress_fields`) consumed by both emitters — drift is
  unrepresentable by construction; the unit test pins the field order.
- **bug_150's live arms**: the live event arms were already correct;
  the model deliberately freezes only the RECONCILE in the two-map
  calibration, matching the as-shipped defect surface.
