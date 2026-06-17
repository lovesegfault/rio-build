// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin, sh-023
// (rio-store placeholder-claim RAII pre-arm): re-introduces the
// UNGUARDED claim window — the +2-era behavior the
// PlaceholderClaim::Owned(PlaceholderGuard) type makes
// unrepresentable. Pre-fix the guard was constructed at the CALLER
// (`spawn_placeholder_guard` at put_path/mod.rs:240) AFTER
// `claim_placeholder` returned `Owned(Uuid)`; a handler future
// dropped at any of the four claim-granting awaits (or between
// `claim_placeholder` returning and the caller-side spawn) leaked
// `claimed ∧ ¬ownerAlive ∧ ¬guardArmed` — every retry inside 5min
// hit `Concurrent`. The §Verifier-one-step-removed pin: the model
// already assumed the post-fix invariant (the heartbeat folded into
// `tick` "fires while held — only process death or release stops
// it"); this calibration proves the model would have caught the
// pre-fix code had it modeled the arm-after-claim gap.
//
// PRE-FIX behavior modeled (one swapped init + one new action):
//   calibInit       — identical to init except `guardArmed' = false`:
//                     the claim row is INSERTed but the guard is not
//                     yet spawned (the +2-era state at
//                     `claim_placeholder` return).
//   calibArmGuard   — the post-INSERT caller-side spawn: arms the
//                     guard while the owner is alive and holds the
//                     claim. Reachable in the calibration; a no-op in
//                     the main regime (guard already armed at init).
//
// PREDICTED falsification: noUnguardedOrphan, via the one-step trace
// calibInit → ownerCancelled (guard not yet armed → drop-reap is the
// dead branch of `if (guardArmed)`; the row stays claimed with the
// owner gone). The post-fix init sets guardArmed = true so the main
// regime holds noUnguardedOrphan vacuously.
//
// Run (the wired pin — pre-fix unguarded gap falsifies):
//   quint verify --backend=tlc --main=placeholderCalibSh023PreArmGap \
//     --init=calibInit --step=calibStep --invariant=noUnguardedOrphan \
//     docs/spec/models/calibration/placeholder-calib-sh023-prearm-gap.qnt
module placeholderCalibSh023PreArmGap {
  import placeholderClaim(
    MAX_TIME = 14, NAR_SIZE = 2,
    STALL_WINDOW = 6, REAP_AFTER = 10, HEARTBEAT_EVERY = 1,
    ENABLE_PHASE_CONJUNCT = true, ENABLE_LIVENESS_CONJUNCT = true
  ).* from "../placeholderClaim"

  /// PRE-FIX init: the claim row exists (`claimed`) but the guard has
  /// not been spawned (`guardArmed = false`) — the +2-era state at
  /// `claim_placeholder` returning `Owned(Uuid)`, before the
  /// caller-side `spawn_placeholder_guard`.
  // Mirrors: rio-store/src/ingest.rs claim_placeholder
  // (pre-sh-023 — guard armed at the caller, not pre-armed)
  action calibInit = all {
    time' = 0,
    ownerAlive' = true,
    claimed' = true,
    guardArmed' = false,
    phase' = Downloading,
    bytes' = 0,
    hbBytes' = 0,
    lastProgressAt' = 0,
    updatedAt' = 0,
    lastHbAt' = 0,
    lastAdvanceAt' = 0,
    deposedExemptPhase' = false,
    deposedSteadyAdvancer' = false,
    strikedLivenessStale' = false,
    sawTakeover' = false,
    sawDeadReap' = false,
  }

  /// PRE-FIX caller-side `spawn_placeholder_guard`: the guard is armed
  /// AFTER the claim, while the owner is alive and holds it. The
  /// cancellation gap is the trace where `ownerCancelled` fires before
  /// this action does.
  // Mirrors: rio-store/src/ingest.rs:spawn_placeholder_guard
  // (pre-sh-023 — invoked at the four production callers AFTER
  // claim_placeholder returned Owned(Uuid); deleted at put_path/mod.rs,
  // put_path_batch.rs, substitute.rs, common.rs in the sh-023 fix)
  action calibArmGuard = all {
    ownerAlive, claimed, not(guardArmed),
    guardArmed' = true,
    time' = time, ownerAlive' = ownerAlive, claimed' = claimed,
    phase' = phase, bytes' = bytes,
    hbBytes' = hbBytes, lastProgressAt' = lastProgressAt, updatedAt' = updatedAt,
    lastHbAt' = lastHbAt, lastAdvanceAt' = lastAdvanceAt,
    deposedExemptPhase' = deposedExemptPhase,
    deposedSteadyAdvancer' = deposedSteadyAdvancer,
    strikedLivenessStale' = strikedLivenessStale,
    sawTakeover' = sawTakeover, sawDeadReap' = sawDeadReap,
  }

  action calibStep = any {
    tick, advance, park, unpark, startPersist, complete,
    ownerDies, ownerCancelled, stallTakeover, reap,
    calibArmGuard,
  }
}
