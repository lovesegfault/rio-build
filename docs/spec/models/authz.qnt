// -*- mode: Bluespec; -*-
//
// Transport + handler authorization verdicts (bughunt-2 wave, slot 4:
// bug_237 / merged_bug_122 / bug_213 / merged_bug_064; spec rules
// store.authz.declared-verifier, store.authz.key-coherence,
// store.log.method-credential+2, store.log.tail-ownership,
// sched.tenant.authz+3, sched.watch.terminal-from-durable-row+2).
//
// WHAT IS MODELED
//   Each step nondeterministically picks one authorization question —
//   a credential class, a verifier configuration, a presented
//   credential kind (the transport layer); a TailLog ownership tuple
//   (dev-mode, build-membership, and the request-string poison input);
//   a WatchBuild tuple (dev-mode, caller-is-owner, lifecycle phase) —
//   and ASSIGNS the implementation verdicts via the live actions. The
//   invariants pin every assigned verdict to the spec law. Verdicts
//   are {admit, deny} ONLY — status-code shape is out of model by
//   design: the resident-phase existence oracle (foreign+resident =
//   PermissionDenied vs foreign+terminal = NotFound) is the
//   spec-pinned asymmetry the §5-S Q4 sign-off names as a known
//   residual.
//
// THE PROPERTIES (state invariants):
//   enforcementFromDeclaredVerifiersOnly (237) — a class's layer
//     verdict is a function of its DECLARED knob alone: all config
//     pairs agreeing on the declared family agree on the verdict, and
//     the assigned verdict equals the declared-knob law.
//   noUndeclaredAdmitWhenKeyed (237 dead leg / 122) — when a keyed
//     class's knob is on, no undeclared credential kind admits (tenant
//     claims never admit a Service method; an assignment header never
//     admits a TenantJwt method; refused-boot configurations never
//     reach verdicts at all).
//   lifecyclePhaseIndependence (213) — the WatchBuild admit/deny
//     verdict for a (caller, owner) pair is the same in the resident
//     and the terminal/cleaned phase.
//   requestStringIndependence (064) — the TailLog ownership verdict is
//     a function of (dev-mode, build-membership) only; the
//     request-string predicate input appears in no law.
//
// CALIBRATIONS (expect-violation, one frozen pre-fix law each):
//   authz-237-foreign-knob — the Service arm keys on the JWT knob (the
//     pre-kernel ServiceOrTenant leg);
//     enforcementFromDeclaredVerifiersOnly falls.
//   authz-122-dead-leg — tenant claims admit a KEYED Service method
//     (the dead tenant leg);
//     noUndeclaredAdmitWhenKeyed falls. Its witness pair
//     (advertisedLegSilent, expected-violation in the BASE model)
//     proves the advertised legs actually admit somewhere — the green
//     invariants are not vacuous.
//   authz-213-phase — the terminal arm ignores the caller (the
//     unbound get_build_terminal_row); lifecyclePhaseIndependence
//     falls.
//   authz-064-string — the ownership fallback consults the request
//     string (the LIKE-prefix IDOR); requestStringIndependence falls.
//
// Tier-1 budget: one-step verdict sweep over a few thousand
// combinations; TLC exhausts instantly.

module authz {
  // ── vocabulary ────────────────────────────────────────────────────
  // Credential classes (rio-authz-kernel CredentialClass, collapsed to
  // kind — Public's rationale and HandlerEnforced's check name don't
  // affect the layer verdict).
  type ClassKind = AssignmentK | TenantK | ServiceK | PublicK | HandlerK
  // Presented credential kinds (kernel Presented).
  type PresentedKind = PNone | PTenant | PSvcVerified | PSvcGarbage | PAssignment
  // WatchBuild lifecycle phase (213).
  type Phase = Resident | Terminal

  type Cfg = { jwt: bool, svc: bool, hmac: bool }

  val BOOLS: Set[bool] = Set(true, false)
  val CONFIGS: Set[Cfg] =
    tuples(BOOLS, BOOLS, BOOLS).map(t => { jwt: t._1, svc: t._2, hmac: t._3 })
  val CLASSES: Set[ClassKind] = Set(AssignmentK, TenantK, ServiceK, PublicK, HandlerK)
  val PRESENTEDS: Set[PresentedKind] = Set(PNone, PTenant, PSvcVerified, PSvcGarbage, PAssignment)

  // ── the spec law (derived from the rule text, mirrored by the
  //    owner-signed EXPECTED_LAYER table in rio-store/src/authz.rs) ──

  /// jwt ⇒ (service ∧ hmac): the bootable states
  /// (store.authz.key-coherence). Refused states never serve, so no
  /// layer verdict is reachable in them.
  pure def bootable(c: Cfg): bool = not(c.jwt) or (c.svc and c.hmac)

  /// The single verifier knob a class declares
  /// (store.authz.declared-verifier).
  pure def declaredKnob(k: ClassKind, c: Cfg): bool =
    if (k == AssignmentK) c.hmac
    else if (k == TenantK) c.jwt
    else if (k == ServiceK) c.svc
    else false

  /// The credential kind that satisfies a keyed class.
  pure def satisfies(k: ClassKind, p: PresentedKind): bool =
    if (k == AssignmentK) p == PAssignment
    else if (k == TenantK) p == PTenant
    else if (k == ServiceK) p == PSvcVerified
    else true

  /// The layer law: enforce-when-configured on the declared knob only.
  pure def layerLaw(k: ClassKind, c: Cfg, p: PresentedKind): bool =
    not(declaredKnob(k, c)) or satisfies(k, p)

  /// TailLog ownership law (store.log.tail-ownership): dev-mode
  /// passes; otherwise build-membership decides. The request string is
  /// NOT an argument — that absence IS the law.
  pure def tailLaw(dev: bool, member: bool): bool = dev or member

  /// WatchBuild law (sched.tenant.authz+3): dev-mode passes; otherwise
  /// the caller must own the build. Phase is NOT an argument.
  pure def watchLaw(dev: bool, owner: bool): bool = dev or owner

  // ── state: one authorization question + its assigned verdicts ─────
  var cls: ClassKind
  var cfg: Cfg
  var presented: PresentedKind
  var layerAdmit: bool

  var tailDev: bool
  var tailMember: bool
  /// The 064 poison input: "the caller-supplied derivation string
  /// would satisfy a prefix predicate against something the caller
  /// owns". The law never reads it.
  var tailReqMatches: bool
  var tailAdmit: bool

  var watchDev: bool
  var watchOwner: bool
  var watchPhase: Phase
  var watchAdmit: bool

  action init = all {
    cls' = PublicK,
    cfg' = { jwt: false, svc: false, hmac: false },
    presented' = PNone,
    layerAdmit' = true,
    tailDev' = true,
    tailMember' = false,
    tailReqMatches' = false,
    tailAdmit' = true,
    watchDev' = true,
    watchOwner' = false,
    watchPhase' = Resident,
    watchAdmit' = true,
  }

  /// One transport-layer decision: pick a bootable configuration, a
  /// class, a presented kind; assign the implementation verdict (the
  /// kernel's decide(), which the calibrations override).
  action decideLayer = {
    nondet k = CLASSES.oneOf()
    nondet c = CONFIGS.filter(x => bootable(x)).oneOf()
    nondet p = PRESENTEDS.oneOf()
    all {
      cls' = k,
      cfg' = c,
      presented' = p,
      layerAdmit' = layerLaw(k, c, p),
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

  /// One TailLog ownership decision (authorize_tail).
  action decideTail = {
    nondet d = BOOLS.oneOf()
    nondet m = BOOLS.oneOf()
    nondet r = BOOLS.oneOf()
    all {
      tailDev' = d,
      tailMember' = m,
      tailReqMatches' = r,
      tailAdmit' = tailLaw(d, m),
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

  /// One WatchBuild decision (actor arm when Resident, the durable
  /// terminal-row arm when Terminal — ONE law for both).
  action decideWatch = {
    nondet d = BOOLS.oneOf()
    nondet o = BOOLS.oneOf()
    nondet ph = Set(Resident, Terminal).oneOf()
    all {
      watchDev' = d,
      watchOwner' = o,
      watchPhase' = ph,
      watchAdmit' = watchLaw(d, o),
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

  action doStep = any {
    decideLayer,
    decideTail,
    decideWatch,
  }

  action step = doStep

  // ── invariants ─────────────────────────────────────────────────────

  /// (237) The assigned verdict equals the declared-knob law, AND the
  /// law itself is foreign-knob independent over all bootable config
  /// pairs (the quantified leg makes the invariant fall in the
  /// calibration even for combinations the sweep has not yet visited).
  val enforcementFromDeclaredVerifiersOnly = and {
    layerAdmit == layerLaw(cls, cfg, presented),
    CLASSES.forall(k =>
      PRESENTEDS.forall(p =>
        CONFIGS.filter(c => bootable(c)).forall(c1 =>
          CONFIGS.filter(c => bootable(c)).forall(c2 =>
            (declaredKnob(k, c1) == declaredKnob(k, c2))
              implies (layerLaw(k, c1, p) == layerLaw(k, c2, p))))))
  }

  /// (237 dead leg / 122) A keyed class admits ONLY its declared
  /// credential kind.
  val noUndeclaredAdmitWhenKeyed =
    (declaredKnob(cls, cfg) and layerAdmit) implies satisfies(cls, presented)

  /// Witness pair for the two invariants above (wired as an
  /// EXPECT-VIOLATION check on this BASE model): "no keyed declared
  /// credential is ever admitted" must be FALSIFIED — i.e. the
  /// advertised legs (claims on a keyed TenantJwt method, a verified
  /// service token on a keyed Service method, an assignment header on
  /// a keyed AssignmentToken method) really do admit somewhere. If a
  /// regression made the keyed classes deny everything (enforcement
  /// collapse into fail-closed-always), the green invariants would
  /// hold vacuously — this witness turns that into a red check.
  val advertisedLegSilent =
    not(declaredKnob(cls, cfg) and satisfies(cls, presented) and layerAdmit)

  /// (213) The WatchBuild verdict never depends on the lifecycle
  /// phase: the assigned verdict equals the phase-free law in BOTH
  /// phases (a phase-dependent assignment falls here the moment the
  /// terminal arm diverges).
  val lifecyclePhaseIndependence = watchAdmit == watchLaw(watchDev, watchOwner)

  /// (064) The TailLog verdict never depends on the request string:
  /// the assigned verdict equals the string-free law for every value
  /// of the poison input.
  val requestStringIndependence = tailAdmit == tailLaw(tailDev, tailMember)
}
