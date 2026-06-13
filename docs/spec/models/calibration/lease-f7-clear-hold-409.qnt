// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin, bughunt-13 F7 (rio-lease
// leader election, the graceful-release hold): re-introduces the
// PRE-merged_bug_051/077 hold posture — a resolved 409 CLEARS the
// graceful-release hold. The model carried that law for every world
// until F7, justified by a stale citation of on_observed_not_leading
// (the exact hold-clearing posture the wave-10 production fix
// deliberately removed; the production doc at on_believing_conflict
// records that the stale name "invited a refactor back to it" — the
// model WAS that refactor), and the divergence stayed masked because
// no wired regime co-enabled shutdowns with rv-movers.
//
// PRE-FIX behavior modeled: CONFLICT_CLEARS_HOLD = true — the one
// toggle F7 added; everything else is the live
// leaderElectionShutdownConflict regime (foreign bumps + holder
// evidence + one SIGTERM).
//
// PREDICTED falsification: noConflictClearedHold — the LAW-SITE
// latch. A foreign bump 409s the believing holder's renew (the
// deferral), a second bump exhausts it — and the pre-051 law CLEARS
// the hold on that resolved 409, although every exhausted round's
// read still named the victim and the lease still does (its write
// never vacated). The production consequence is the skipped SIGTERM
// release and the successor's full steal-threshold wait — but the
// shutdown plane's own gracefulHandover latch CANNOT see it: that
// latch judges "the gate's own spec says release" by `held`, which
// is exactly the value the broken law corrupts (held=false makes the
// skip look lawful). The detector is therefore the law-site latch
// (holdClearedByConflict — the blindConflictLose house pattern),
// recorded the instant the clearing branch lands on a held hold.
// The live regime's zombieConflictSigtermReleasesRun pins the
// consequence end (the release actually runs under the fixed law).
//
// This pin is the non-vacuity evidence for
// quint-leader-election-shutdown-conflict: if it ever stops
// falsifying, the regime has drifted away from the 409 shape the
// hold law exists to survive.
//
// Run (the wired pin — the clear-on-409 world falsifies):
//   quint verify --backend=tlc --main=leaseCalibF7ClearHold409 \
//     --invariant=noConflictClearedHold \
//     docs/spec/models/calibration/lease-f7-clear-hold-409.qnt
module leaseCalibF7ClearHold409 {
  import leaderElection(
    NODES = Set("n1", "n2"),
    FENCE_AFTER = 2,
    STEAL_AFTER = 5,
    RENEW = 1,
    MAX_SKEW = 1,
    MAX_TIME = 8,
    MAX_GEN = 4,
    MAX_RV = 8,
    MAX_DELETES = 0,
    MAX_CLAIM_FAILURES = 0,
    MAX_RESTORES = 0,
    MAX_SUSPENDS = 0,
    MAX_SHUTDOWNS = 1,
    MAX_DROPS = 0,
    MAX_FOREIGN_BUMPS = 2,
    OBS_KEY_RV = false,
    BLIND_HOLDER_BOUND = 0,
    HOLDER_EVIDENCE_LOSE = true,
    CONFLICT_IMMEDIATE_LOSE = false,
    MAX_STEP_DOWNS = 0,
    STEP_DOWN_COUNT_KEYED = false,
    CONFLICT_CLEARS_HOLD = true
  ).* from "../leaderElection"
}
