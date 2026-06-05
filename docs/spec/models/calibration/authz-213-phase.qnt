// -*- mode: Bluespec; -*-
//
// Calibration twin (expect-violation), bug_213: the WatchBuild
// terminal-row fallback fetched the durable verdict with NO tenant
// bind — the actor arm (resident phase) checked the caller, the
// post-cleanup arm did not, so a foreign tenant streamed the settled
// verdict the moment the build left the actor (recorded red: watch as
// tenant B after CancelBuild + CleanupTerminalBuild returned
// Ok(stream) carrying tenant A's verdict).
//
// Frozen pre-fix law: Terminal-phase verdict ignores the caller
// entirely (any authenticated caller admits).
//
// PREDICTED falsification: lifecyclePhaseIndependence — the assigned
// verdict diverges from the phase-free law exactly in the
// (Terminal, foreign-caller) cell.
//
// Run (the wired pin):
//   quint verify --backend=tlc --main=authzCalib213Phase \
//     --step=calibStep --invariant=lifecyclePhaseIndependence \
//     docs/spec/models/calibration/authz-213-phase.qnt
module authzCalib213Phase {
  import authz.* from "../authz"

  /// PRE-FIX: the terminal arm admits regardless of ownership.
  pure def watchLawPhaseDependent(dev: bool, owner: bool, ph: Phase): bool =
    if (ph == Terminal) true else watchLaw(dev, owner)

  action calibDecideWatch = {
    nondet d = BOOLS.oneOf()
    nondet o = BOOLS.oneOf()
    nondet ph = Set(Resident, Terminal).oneOf()
    all {
      watchDev' = d,
      watchOwner' = o,
      watchPhase' = ph,
      watchAdmit' = watchLawPhaseDependent(d, o, ph),
      cls' = cls,
      cfg' = cfg,
      presented' = presented,
      layerAdmit' = layerAdmit,
      tailDev' = tailDev,
      tailMember' = tailMember,
      tailReqMatches' = tailReqMatches,
      tailAdmit' = tailAdmit,
    }
  }

  action calibStep = any {
    decideLayer,
    decideTail,
    calibDecideWatch,
  }
}
