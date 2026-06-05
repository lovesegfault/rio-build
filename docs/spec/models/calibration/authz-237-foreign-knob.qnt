// -*- mode: Bluespec; -*-
//
// Calibration twin (expect-violation), bug_237: the PRE-KERNEL Service
// arm keyed on the JWT knob — the `ServiceOrTenant` class read a
// verifier family it never declared, so two configurations agreeing on
// the service knob produced DIFFERENT verdicts (the half-config with
// JWT on and the service key off admitted everything the service class
// guarded, and the class's own docs certified the state as safe).
//
// Frozen pre-fix law: layer verdict for ServiceK keys on cfg.jwt
// (admit-all when jwt off; when jwt on, claims admit — the dead tenant
// leg rides the same foreign-knob read).
//
// PREDICTED falsification: enforcementFromDeclaredVerifiersOnly — the
// assigned verdict diverges from the declared-knob law (and the
// quantified leg cannot save it: the assignment itself disagrees).
//
// Run (the wired pin):
//   quint verify --backend=tlc --main=authzCalib237ForeignKnob \
//     --step=calibStep --invariant=enforcementFromDeclaredVerifiersOnly \
//     docs/spec/models/calibration/authz-237-foreign-knob.qnt
module authzCalib237ForeignKnob {
  import authz.* from "../authz"

  /// PRE-FIX layer law: ServiceK reads the FOREIGN jwt knob.
  pure def layerLawForeignKnob(k: ClassKind, c: Cfg, p: PresentedKind): bool =
    if (k == ServiceK)
      // ServiceOrTenant: unenforced unless the JWT knob is on; when
      // on, tenant claims OR a verified service token admit.
      not(c.jwt) or (p == PTenant or p == PSvcVerified)
    else layerLaw(k, c, p)

  action calibDecideLayer = {
    nondet k = CLASSES.oneOf()
    nondet c = CONFIGS.filter(x => bootable(x)).oneOf()
    nondet p = PRESENTEDS.oneOf()
    all {
      cls' = k,
      cfg' = c,
      presented' = p,
      layerAdmit' = layerLawForeignKnob(k, c, p),
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

  /// The live step with ONLY the layer decision swapped pre-fix.
  action calibStep = any {
    calibDecideLayer,
    decideTail,
    decideWatch,
  }
}
