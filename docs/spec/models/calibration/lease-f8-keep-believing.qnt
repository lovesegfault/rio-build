// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin, bughunt-13 F8 (rio-lease
// leader election, the absence partition): re-introduces the
// PRE-bug_143 posture — a believing node's completed read that
// observes the lease ABSENT (404) keeps believing. Production exits
// belief AT that read (election.rs FetchOutcome::Create routed
// through route_act_failed_read to AbsenceLose/AbsenceLoseHoldKept):
// a vanished lease grants a creator immediate acquisition with no
// steal wait, so the fence/steal margin does not shield the absence
// partition and read evidence is the only thing that bounds the dual
// window. Before F8 the model could not even represent the state (no
// reachable state lacked a lease object); this twin pins the law the
// new vanish regime wires.
//
// PRE-FIX behavior modeled: ABSENT_READ_KEEPS_BELIEF = true — the one
// toggle F8 added; everything else is the live leaderElectionVanish
// regime (one vanish, asymmetric separation, holder-evidence law).
//
// PREDICTED falsification: believerExitsAtAbsentRead — the LAW-SITE
// latch (keptBelievingPastAbsentRead) records, at the read arm
// itself, a believing absent read whose belief survived the read.
// The consequence invariants cannot be the detector here (the F7
// lesson): the kept belief is exactly the state they would have to
// read, and the dual window it opens is bounded by other clocks in
// most traces — the latch fires the moment the kept-believing branch
// runs, deterministically, at depth acquire → vanish → read.
//
// This pin is the quint-policy P1 falsify twin for
// believerExitsAtAbsentRead and the non-vacuity evidence for the
// quint-leader-election-vanish regime check: if it ever stops
// falsifying, the believing absent read is no longer reachable there
// and the regime's green verdict has gone vacuous.
//
// Run (the wired pin — the keep-believing world falsifies):
//   quint verify --backend=tlc --main=leaseCalibF8KeepBelieving \
//     --invariant=believerExitsAtAbsentRead \
//     docs/spec/models/calibration/lease-f8-keep-believing.qnt
module leaseCalibF8KeepBelieving {
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
    MAX_SHUTDOWNS = 0,
    MAX_DROPS = 0,
    MAX_FOREIGN_BUMPS = 0,
    OBS_KEY_RV = false,
    BLIND_HOLDER_BOUND = 0,
    HOLDER_EVIDENCE_LOSE = true,
    CONFLICT_IMMEDIATE_LOSE = false,
    MAX_STEP_DOWNS = 0,
    STEP_DOWN_COUNT_KEYED = false,
    CONFLICT_CLEARS_HOLD = false,
    MAX_VANISHES = 1,
    ABSENT_READ_KEEPS_BELIEF = true
  ).* from "../leaderElection"
}
