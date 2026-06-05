// -*- mode: Bluespec; -*-
//
// Calibration twin (expect-violation), merged_bug_064: the TailLog
// ownership fallback matched the CALLER'S request string against
// derivations the caller already owned — owning any unrelated
// derivation + pinning a foreign exec_id streamed the foreign log
// (recorded red: verbatim own-drv + foreign-pin admitted by
// tenant_owns_exec on the pre-fix tree).
//
// Frozen pre-fix law: the verdict consults the request-string
// predicate (tailReqMatches) as an OR-arm of ownership.
//
// PREDICTED falsification: requestStringIndependence — the assigned
// verdict diverges from the string-free law exactly in the
// (non-member, reqMatches) cell.
//
// Run (the wired pin):
//   quint verify --backend=tlc --main=authzCalib064String \
//     --step=calibStep --invariant=requestStringIndependence \
//     docs/spec/models/calibration/authz-064-string.qnt
module authzCalib064String {
  import authz.* from "../authz"

  /// PRE-FIX: the request string is an ownership arm.
  pure def tailLawStringPoisoned(dev: bool, member: bool, reqMatches: bool): bool =
    dev or member or reqMatches

  action calibDecideTail = {
    nondet d = BOOLS.oneOf()
    nondet m = BOOLS.oneOf()
    nondet r = BOOLS.oneOf()
    all {
      tailDev' = d,
      tailMember' = m,
      tailReqMatches' = r,
      tailAdmit' = tailLawStringPoisoned(d, m, r),
      cls' = cls,
      cfg' = cfg,
      presented' = presented,
      layerAdmit' = layerAdmit,
      watchDev' = watchDev,
      watchOwner' = watchOwner,
      watchPhase' = watchPhase,
      watchAdmit' = watchAdmit,
    }
  }

  action calibStep = any {
    decideLayer,
    calibDecideTail,
    decideWatch,
  }
}
