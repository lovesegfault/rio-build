// -*- mode: Bluespec; -*-
//
// Calibration twin (expect-violation), merged_bug_122 / bug_237 dead
// leg: tenant claims ADMITTED a keyed Service method (the
// `ServiceOrTenant` tenant leg — all six admin handlers demand service
// callers, so the leg was dead weight that became an any-tenant-admin
// door in the half-config; recorded red pre-kernel: TenantClaims on
// TriggerGC with the service key configured → grpc-status None).
//
// Frozen pre-fix law: keyed ServiceK admits PTenant alongside
// PSvcVerified.
//
// PREDICTED falsification: noUndeclaredAdmitWhenKeyed — a keyed class
// admits a credential kind that does not satisfy it.
//
// Run (the wired pin):
//   quint verify --backend=tlc --main=authzCalib122DeadLeg \
//     --step=calibStep --invariant=noUndeclaredAdmitWhenKeyed \
//     docs/spec/models/calibration/authz-122-dead-leg.qnt
module authzCalib122DeadLeg {
  import authz.* from "../authz"

  /// PRE-FIX: the dead tenant leg admits claims on a KEYED service
  /// class.
  pure def layerLawDeadLeg(k: ClassKind, c: Cfg, p: PresentedKind): bool =
    if (k == ServiceK and declaredKnob(k, c))
      p == PSvcVerified or p == PTenant
    else layerLaw(k, c, p)

  action calibDecideLayer = {
    nondet k = CLASSES.oneOf()
    nondet c = CONFIGS.filter(x => bootable(x)).oneOf()
    nondet p = PRESENTEDS.oneOf()
    all {
      cls' = k,
      cfg' = c,
      presented' = p,
      layerAdmit' = layerLawDeadLeg(k, c, p),
      tailDev' = tailDev,
      tailMember' = tailMember,
      tailReqMatches' = tailReqMatches,
      tailAdmit' = tailAdmit,
      watchDev' = watchDev,
      watchOwner' = watchOwner,
      watchPhase' = watchPhase,
      watchAdmit' = watchAdmit,
    }
  }

  action calibStep = any {
    calibDecideLayer,
    decideTail,
    decideWatch,
  }
}
