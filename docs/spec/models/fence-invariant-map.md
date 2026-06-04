# fencedWrites — invariant map (A1 fenced-write-discipline)

Status: LANDED with the bughunt-wave A1 workstream (2026-06-03). Model:
`docs/spec/models/fencedWrites.qnt` (Tier-1 exhaustive: 2 replicas,
MAX_GEN = 3, one derivation's active assignments row, one persisted
resource floor). Companion regime: `materializationJobResolveFaults`
in `docs/spec/models/materializationJob.qnt` (the view-settlement
pair). This map is the model↔code record the wave-close checklist and
later counter-signatures cite.

## What the model adds over its siblings

`leaderElection.qnt` proves generation distinctness;
`materializationJob.qnt` proves job-table writes are fenced as ATOMIC
actions. Neither models the READ COMMITTED window between a
transaction's floor read and its write — exactly where the four
findings lived. `fencedWrites.qnt` states the fence over INTERLEAVED
transactions: `beginTx` snapshots the floor visible at read time (the
advisory half); the guarded upsert's in-statement predicate evaluates
against the row's latest committed version, EvalPlanQual-style (the
authoritative half — `sched.lease.fence-statement-guard`).

## Model ↔ code site table

| Model action | Production site | Notes |
|---|---|---|
| `beginTx` / `fenceRefuse` | `SchedulerDb::begin_fenced` (rio-scheduler/src/db/mod.rs) | The ONLY constructor of decision-state write transactions; `claims_floor`/`at_or_above_floor` are private to db/mod.rs (bug_269's structural close) |
| `guardedUpsertCommit` | `mint_pull_attempt_fenced` (db/open_attempts.rs) — `ON CONFLICT … DO UPDATE … WHERE assignments.generation <= EXCLUDED.generation` | bug_261. Equality passes: the same-epoch re-acquire keep. Pinned in production by the real-PG interleaving test `mint_statement_guard_blocks_generation_regression` (never weaken to a mock — the EvalPlanQual semantic is the load-bearing fact) |
| `fencedClose` | `FencedTx::close_assignment` (db/mod.rs) — exec_id-scoped, never derivation_id | merged_bug_231. The unique pull-mode closer; the dead derivation_id-keyed writers were deleted (cfg(test) fixtures remain for db tests) |
| `floorWriteGreatest` | `update_resource_floor` (db/derivations.rs) — fenced + per-dimension server-side `GREATEST` ratchet | bug_273. The ratchet catches the same-tenure stale-base regression the fence cannot see |
| `answerRetryable` | `actor_error_to_status` / `pull_rejection_to_status` (grpc/actor_guards.rs) + the gateway's bounded pre-build_id SubmitBuild retry | bug_393 (`sched.grpc.fence-retryable`): Retryable ⟺ code ∈ {UNAVAILABLE, RESOURCE_EXHAUSTED}, pinned by `retry_class_code_consistency` |
| `successorClaim` | `leader_generation_claims` insert (the lease task's claim stamp) | Claims commit immediately; in-flight snapshots do not see them (READ COMMITTED) |
| `guardedUpsertCommitAs(r, replicaGen)` vs `atomicGen` (plane 1) | `ServingGeneration` (db/mod.rs) — sole constructor `stamp_from_claim`, boot + claim stamps only; `fence_coverage.rs` census forbids fresh `leader.generation()` reads in write paths | merged_bug_338. The model's `atomicGen` is the live lease atomic; `atomicBumpReacquire` is the mid-mailbox re-acquire window. The fixed system CANNOT pass `atomicGen` — the type has no ambient reader; the calibration passes it explicitly |
| `latchFailedPersist` / `advanceDerivation` / `flushReplayApply(true, true)` (plane 2) | `StatusBatch.exec_ids` latch (actor/mod.rs), the flush's present-state partition (actor/housekeeping.rs `tick_flush_status_outbox`), `replay_status_batch_guarded` (db/derivations.rs) — close `WHERE exec_id = ANY($latched)` | merged_bug_011. `advanceDerivation` is the resubmit's active-row upsert rewriting exec_id in place; the model's drop-when-stale is the flush-time re-derivation (KEEP present-equal/absent, DROP present-different) |
| `recoverySucceed` / `completeTenure` / `stepDownFailedTenure` (plane 3) | `RecoveredDag` witness (actor/recovery.rs) — minted at `recover_from_pg`'s Ok tail, consumed by `complete_tenure` (sole `set_recovery_complete` + `dag_authoritative = true` writer); `LeaderState::request_step_down` + the lease-loop consumption (rio-lease) | bug_155. `stepDownFailedTenure` keeps `committedClaims` (the durable claim stays — harmless over-claim) and zeroes the replica's serving state; candidacy resumes via `successorClaim` |

View-settlement pair (`materializationJobResolveFaults` regime):

| Model element | Production site |
|---|---|
| `resolveFenced`/`resolveErr` (fault stutters: job, view, attempt all kept) | `WriteDisposition::{Fenced,Failed}` arms — `JobView::remove_settled` refuses the removal; every companion gates on `settled()` (`sched.materialize.view-settlement`) |
| `resolveApplied` = `cancelOnZeroInterest` (single step: job + view + attempt) | `cancel_job_and_close_attempt_fenced` (db/materialization.rs) — one fenced tx, kind-guarded attempt close, total over the DAG-absent arm (merged_bug_276) |
| `viewMatchesDurableUnresolved` | the JobView wrapper: `remove_settled` is the only per-entry removal; `wipe`/`rebuild` belong to LeaderLost/recovery |
| `chargeFreeCancellation` (`chargedAfterCancel` ghost) | a closed assignment is invisible to the establishment sweep — the leaked-attempt → `materialization_infra` conversion is unreachable for cancelled jobs |

## Calibration table (expect-violation pins)

| Check | Pre-fix behavior re-found | Violated invariant |
|---|---|---|
| `quint-fence-calib-261-unguarded-upsert` | predicate-free DO UPDATE applies on the stale advisory pass (the pause trace) | `activeRowGenMonotonic` |
| `quint-fence-calib-231-unfenced-close` | derivation-keyed unfenced close — a deposed sweep closes the successor's row | `openAttemptViewStableUnderDeposedClose` |
| `quint-fence-calib-273-plain-floor-set` | plain `SET floor = $2` — regression under interleaving / deposed write | `resourceFloorMonotonic` |
| `quint-fence-calib-393-terminal-refusal` | the refusal answered FAILED_PRECONDITION; the client gives up | `fenceRefusalAlwaysRetryable` |
| `quint-fence-calib-floor-blind` | the begin_fenced admission compare dropped — a tx the fence refused mutates decision state anyway | `belowFloorTxNeverMutates` |
| `quint-materialization-calib-133-discarded-outcome` | the fenced/errored resolve still discards the view entry | `viewMatchesDurableUnresolved` |
| `quint-materialization-calib-276-dag-absent-cancel` | the split cancel leaks the open attempt; the establishment sweep charges it | `chargeFreeCancellation` |
| `quint-fence-calib-338-atomic-reread` | the mint stamps the fresh lease-atomic read after a mid-tenure bump (the shared apply oracle latches the non-claim stamp) | `writesCarryClaimedTenure` |
| `quint-fence-calib-011-absolute-replay` | the outbox flush replays a stale batch verbatim after the resubmit advanced past the latch | `outboxReplayNeverRegresses` |
| `quint-fence-calib-011-foreign-close` | same module/trace: the derivation-scoped close lands on the successor's fresh exec | `outboxClosesOnlyLatchedExecs` |
| `quint-fence-calib-155-serve-after-failed-recovery` | the retired "degrade, don't block" doctrine: an un-recovered tenure completes and serves (previously model-unrepresentable; the plane makes the zombie expressible) | `failedRecoveryNeverServes` |

Every wired holds/exhaustive check has its falsifiability pair above
(the constructor's vacuity rule); the baselines (as-built `step`) hold
on every calibration module.

Latch integrity (bughunt-2 bug_358): the five latches are computed
LIVE by the oracle-seated apply sub-actions (`upsertApply`,
`closeApply`, `floorWriteApply`, `answerRefusal`, with the
`snapshotExceedsGen` oracle) declared in fencedWrites.qnt and named in
its `quint-policy-latches:` header directive. Calibrations perturb
DECISIONS only — admission guards, the EvalPlanQual re-check, the
GREATEST ratchet, the answer code — and never assign a latch; the
quint-policy lint (P4/P5) enforces both halves mechanically.

## Priced residuals (deliberate, bounded)

1. **The fresh-INSERT-below-floor window** (`guardedUpsertCommit`
   with `activeGen == 0`): a deposed believer whose floor knowledge
   predates the successor's claim can mint a fresh active row below
   the claims floor when NO conflict row exists to evaluate the
   in-statement predicate against. It cannot regress any newer row by
   construction — `activeRowGenMonotonic` holds with the residual
   REACHABLE in the model (deliberately not excluded). Production
   bound: the successor's own re-mint conflicts on the partial-unique
   index, where the in-statement guard governs; the worst case is one
   transient dual delivery, settled by the report-side fence.
2. **The adopt two-tx non-atomicity** (merged_bug_231's residual): the
   establishment adopt persists status and closes the assignment in
   two fenced transactions; a crash between them leaves an open
   assignment for an adopted derivation. Benign: the sweep re-runs
   idempotently (the second pass closes; no double charge — the
   charge row's terminal-row-wins append). Bounded by one sweep tick.

## None-sensible rationales (directive 2)

- **bug_269 (open-coded floor SQL)** — no model: the property is
  TEXTUAL ("the GREATEST claims-floor literal exists in exactly two
  files"), not behavioral. Carrier: the `fence-sql-canonical` policy
  check (nix/misc-checks.nix), red-verified against the pre-A1 base
  (housekeeping.rs, pull.rs, open_attempts.rs carried the literal).
- **bug_273's coverage half (which writers are fenced)** — no model:
  the property is an ENUMERATION over the crate's SQL surface.
  Carrier: `db/tests/fence_coverage.rs` (source-enumerating: every
  write-verb statement on a decision table is FencedTx-taking or
  allowlisted with rationale) plus the `fence-no-raw-decision-sql`
  policy check for the actor/grpc/admin side, red-verified against
  the base (housekeeping.rs, pull.rs, materialize.rs carried raw
  decision SQL).

Kani: explicitly none-sensible for this workstream — the subjects are
DB-transaction interleavings and a gRPC code mapping, not pure
algebra (nix/kani.nix's scope note; the kernel crates stay the kani
domain).

## Budgets (recorded at introduction)

- `quint-fenced-writes` (TLC exhaustive, fencedWritesT1): full state
  space at MAX_GEN=3/2 replicas/1 drv — seconds-class on the wiring
  measurement host; no step bound (TLC BFS). Re-measured at the
  bughunt-2 plane introduction (all three planes LIVE): 16,643,269
  states generated, 795,900 distinct, ~10.5s TLC wall-clock — still
  seconds-class; the three ENABLE_* axes exist so any future plane
  growth can be split per-regime instead of multiplying one board.
- The four legacy fence calibrations: first-violation TLC, each found
  in under 2s at the same scope; re-verified violating with the
  bughunt-2 planes bound DORMANT (state space unchanged).
- The three bughunt-2 plane calibrations (338 / 011×2 / 155):
  first-violation TLC, each found in ~2s with only its own plane
  enabled.
- `quint-materialization-holds-resolve-faults`: 2M samples × 15 steps
  (the regime budget every materializationJob holds check uses); the
  two A1 invariants joined `matJobInvariants` for ALL regimes at
  unchanged budgets.
