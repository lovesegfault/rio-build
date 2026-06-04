# Gateway-resync / open-attempts / tail-reader invariant map (bughunt fix wave, workstream C1; bughunt-2 wave, slot 1)

Status: LANDED with the terminal-capture workstream (2026-06-03);
EXTENDED by the server-side-liveness workstream (2026-06-04):
gwBuildResync's heal split signal → failable re-attach → snapshot
apply (merged_bug_056) and the NEW `tailReaderLoop.qnt`.
Models: `gwBuildResync.qnt`, `openAttempts.qnt`, `tailReaderLoop.qnt`.
Wired checks: 3 holds (TLC exhaustive, Tier-1) + 9 expect-violation
calibrations.

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
| `boundedResyncStreak` | merged_bug_056 | the gateway's re-attach counter equals the cycles-since-organic oracle and stays within `MAX_STREAK` — a reset on the snapshot apply diverges on the first cycle |
| `snapshotOwedNoConsume` | merged_bug_056 | while signalled or snapshot-owed, display and tails are frozen at the signal-time checkpoint — the dead stream cannot mutate them |

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

`tailReaderLoop.qnt` — one reader (the gateway relay's `run_tail` or
the dashboard's follow loop — same decision skeleton) cycling stream
attempts. Each re-open yields a productive verdict (serve /
gapThenServe), a bare receipt (the store's always-final arm — the
`skip` verdict), or fails at the open; the consumer set can die at any
point (orphan). The pacer level is pinned to a cycles-since-progress
oracle; the open count is pinned to its orphan-time checkpoint.

| invariant | finding | meaning |
|---|---|---|
| `orphanNeverReopens` | merged_bug_130 | once orphaned, the open count is frozen — an orphaned relay never opens another stream (the kernel law's model twin; the Rust side carries `check_tail_next_orphan_always_exits`) |
| `pacingEscalatesAbsentProgress` | merged_bug_054 | the pacer level always equals min(cycles-since-progress, cap) — bare receipt never resets the ladder |

### Heal-split model-boundary notes (merged_bug_056, recorded at landing)

- The model charges ONE budget cycle per heal cycle (`reattachOk` /
  `reattachFailed`); the implementation also charges the cycle that
  CONSUMES the loss signal (`note_reattach` on the stream error). Same
  monotone accounting, off by a constant — the property under test
  (reset only on organic receipts) is unaffected.
- `MAX_STREAK = 3` is the model-scale stand-in for `MAX_RECONNECT =
  10` (state-space, not semantics: both are "the budget is finite and
  only organic receipts refresh it").
- Emissions while signalled/snapshotOwed take the Skipped shape with
  no new `pendingResync`: the dead stream cannot carry them and the
  owed snapshot reconcile covers them. The bridge's per-streak
  debounce makes this faithful (one signal per lagged streak).
- The DELIBERATE NON-DELTA from the workstream order stands:
  `logService.qnt` gains no builder-vanish/non-reading-client actions —
  hung awaits are invisible to untimed models; the streaming-open-ban
  lint, the keepalive chokepoint, and the timed conformance tests are
  the binding artifacts there (also keeps the file collision-free with
  log-serve's extension).

## Calibrations (expect-violation — each pins an as-shipped design)

| check | frozen design | falsifies |
|---|---|---|
| `quint-gwresync-calib-two-map` | two independent display maps; the gone-reconcile sweeps only the build family | `noStuckDisplay` |
| `quint-gwresync-calib-no-signal` | broadcast-lag skips are scheduler-log-only; the gateway never learns | `tailCoverage` |
| `quint-gwresync-calib-kind-blind` | no wire kind; materializations get the uniform build treatment (display + tail) | `kindedSurfaceAgrees` |
| `quint-gwresync-calib-no-pg-fallback` | no durable terminal payload; post-cleanup attach fabricates | `terminalVerdictNeverFabricated` |
| `quint-openattempts-calib-charge-blind` | failed cancel persist dropped; sweep charges without consulting the node | `cancelledNeverChargedAsCrash` |
| `quint-gwresync-calib-reset-on-snapshot` | reset-on-any-event: the snapshot apply resets the re-attach counter (live-import, `calibStep`) | `boundedResyncStreak` |
| `quint-gwresync-calib-consume-while-owed` | dead stream stays bound after a failed re-attach; its buffered events are consumed mid-heal (live-import, `calibStep`) | `snapshotOwedNoConsume` |
| `quint-tailreader-calib-orphan-hotloop` | orphan mapped to naturalEnd: the reopen arm stays enabled after the drain sender died (live-import, `calibStep`) | `orphanNeverReopens` |
| `quint-tailreader-calib-reset-on-receipt` | receipt treated as progress: a skip cycle resets the pacer (live-import, `calibStep`) | `pacingEscalatesAbsentProgress` |

## Budgets (recorded at landing)

All three models are Tier-1: TLC exhausts each in seconds
(gwBuildResync post-split: 4,173 states generated / 1,048 distinct,
depth 12, ~1.1 s — the heal-split vars are heavily lockstepped, so the
distinct-state count stays small; openAttempts ≤36 states;
tailReaderLoop ~hundreds of states, ~0.85 s). Re-measured at the
bughunt-2 slot-1 landing: all six gwBuildResync invariants hold in the
same exhaustive run; all four pre-split calibrations (frozen copies,
self-contained) still falsify byte-identically; the four new
live-import calibrations falsify under `calibStep` and HOLD under the
as-built `step` (P3 baseline pairing, zero extra TLC). No
`serverHeapMb` override needed (default 4 GiB converts all three).

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
