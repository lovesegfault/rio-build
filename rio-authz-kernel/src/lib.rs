//! Pure authorization decision kernel for the store's per-method
//! credential-class layer.
//!
//! # Why a kernel
//!
//! Round-2 triage (bug_237) found the transport authz layer deriving a
//! class's enforcement state from a knob the class never declared: the
//! admin class was keyed on the *JWT* knob, so the half-configured
//! state (JWT on, service key off) silently widened any tenant into a
//! cluster admin — and the class doc certified that state as safe.
//! The structural cause was that every class arm could read every
//! knob.
//!
//! This crate makes that unwritable: each class declares exactly one
//! verifier family ([`consumes`]), [`decide`] hands each arm a
//! projection carrying ONLY its declared knob ([`TenantJwtKnob`],
//! [`ServiceKnob`], [`AssignmentKnob`]), and the arms are free
//! functions whose signatures cannot name a foreign knob. The ~10-line
//! projection-constructing dispatch in [`decide`] is the trusted
//! residual, pinned by the foreign-knob-independence proof
//! (`check_foreign_knob_independence`): for any two configurations
//! that agree on the declared family, the verdict is identical.
//!
//! # Credential vectors
//!
//! Real requests carry credential *vectors* (the dashboard's nginx
//! injects a service token on TailLog, scheduler probes carry a
//! service token next to a tenant header, gateway PutPath attaches a
//! JWT and a service token). Classification — performed by the caller
//! (`rio_store::authz`) — is *relative to the class's declared
//! family*: when classifying for a `TenantJwt` method only the tenant
//! claims are inspected, for a `Service` method only the service
//! token, for an `AssignmentToken` method only the assignment header.
//! A foreign credential is invisible: it can neither widen nor poison
//! a verdict. (A global priority-ordered classifier was considered
//! and REJECTED for exactly that reason.)
//!
//! # Dual-mode doctrine
//!
//! Every keyed class is enforce-when-configured: knob off ⇒ admit
//! (single-node dev stores and keyless VM scenarios keep working).
//! The dangerous half-configured states are killed at BOOT, not at
//! the layer: [`key_coherence`] refuses any configuration where JWT
//! is on but the service or assignment key is missing
//! (`jwt ⇒ (service ∧ hmac)`), naming the missing knob. The five
//! coherent states — dev `(0,0,0)`, the helm default `(0,1,1)`, full
//! `(1,1,1)`, and the two keys-without-jwt states — keep booting.
//
// r[impl store.authz.declared-verifier]

#![forbid(unsafe_code)]

/// The three transport verifier families the store can be configured
/// with. Exactly one per keyed [`CredentialClass`]; [`Public`] and
/// [`HandlerEnforced`] classes consume none.
///
/// [`Public`]: CredentialClass::Public
/// [`HandlerEnforced`]: CredentialClass::HandlerEnforced
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifierFamily {
    /// The JWT pubkey (verifies `x-rio-tenant-token`).
    TenantJwt,
    /// The service HMAC key (verifies `x-rio-service-token`).
    Service,
    /// The assignment HMAC key (binds builder ingest tokens).
    Assignment,
}

/// The handler-side check a [`CredentialClass::HandlerEnforced`]
/// method's data path requires a witness from. The variants name the
/// real check functions in `rio_store` — a "checked in the handler"
/// table claim with no named check (and, store-side, no typed witness
/// on the data path) no longer typechecks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerCheck {
    /// `verify_assignment_token` — the per-message builder ingest
    /// token (HMAC binding rides in the first streamed frame, not a
    /// transport header).
    IngestToken,
    /// `sig_visibility_gate{,_batch}` — tenant-key signature
    /// visibility for path metadata reads.
    SigVisibility,
    /// `verified_service_caller` / `ensure_service_caller` — the
    /// cluster-internal service token, verified in the handler.
    ServiceCaller,
    /// `reject_end_user_tenant` — the DENY-tenants polarity (builder
    /// internal surfaces): not a require-credential check, an
    /// end-user-rejection check. The sig-visibility gate-skip on these
    /// methods cannot be a bypass because end-user tenants are
    /// rejected outright.
    EndUserRejected,
}

/// What a caller must present for a method to be dispatched.
///
/// The store's method table (`rio_store::authz::METHOD_CREDENTIALS`)
/// assigns exactly one of these to every bound method; the tower
/// layer classifies the request relative to the class and delegates
/// the verdict to [`decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialClass {
    /// An `x-rio-assignment-token` header must be present (builder
    /// ingest streams). Presence is pinned at the layer; the HMAC
    /// *binding* stays in the stream gate which sees the first frame.
    /// Enforced when the assignment-HMAC verifier is configured.
    AssignmentToken,
    /// Verified tenant claims must be attached by the JWT
    /// interceptor. Enforced when the JWT pubkey is configured. Per
    /// the standing bug_290 owner decision there is NO service-token
    /// bypass for this class.
    TenantJwt,
    /// A VERIFIED `x-rio-service-token` (cluster-internal callers:
    /// controller, scheduler, operator tooling). Enforced when the
    /// service verifier is configured — and ONLY that knob: the
    /// pre-kernel class (`ServiceOrTenant`) was keyed on the JWT knob
    /// and admitted bare tenant claims, a leg every admin handler
    /// rejected anyway (dead in the green path, live as the
    /// half-config hole). The tenant leg is deleted; tenant claims on
    /// a `Service` method are a foreign credential and do not admit.
    Service,
    /// Genuinely public: no caller identity exists or the resource is
    /// content-addressed public metadata. The rationale is part of
    /// the table row — an undocumented `Public` does not construct.
    Public {
        /// Why this method is safe with no transport credential.
        rationale: &'static str,
    },
    /// The transport layer admits; the handler's data path requires a
    /// typed witness from the named check. This replaces the old
    /// catch-all `Open` class for every method whose credential the
    /// transport cannot see.
    HandlerEnforced {
        /// The handler check whose witness the data path requires.
        check: HandlerCheck,
    },
}

/// The verifier family a class's layer verdict consumes, if any.
#[must_use]
pub const fn consumes(class: CredentialClass) -> Option<VerifierFamily> {
    match class {
        CredentialClass::AssignmentToken => Some(VerifierFamily::Assignment),
        CredentialClass::TenantJwt => Some(VerifierFamily::TenantJwt),
        CredentialClass::Service => Some(VerifierFamily::Service),
        CredentialClass::Public { .. } | CredentialClass::HandlerEnforced { .. } => None,
    }
}

/// The three verifier knobs as configured at boot. `true` = the key
/// material is present and the corresponding verifier is live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifierConfig {
    /// JWT pubkey configured (tenant-token verification).
    pub jwt: bool,
    /// Service HMAC key configured (service-token verification).
    pub service: bool,
    /// Assignment HMAC key configured (builder ingest binding).
    pub hmac: bool,
}

/// The classified credential presentation, *relative to the method's
/// class* (see the module doc on credential vectors): the classifier
/// inspects only the declared family's material, so e.g.
/// `TenantClaims` is never produced for a `Service`-class method.
/// [`decide`] is nonetheless total over the full product — a foreign
/// presentation simply does not admit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presented {
    /// No credential of the declared family.
    None,
    /// Verified tenant claims attached by the JWT interceptor.
    TenantClaims,
    /// A service token that VERIFIED against the configured key.
    ServiceVerified,
    /// A service token header that failed verification (present but
    /// garbage/forged — distinct from absent so the rejection can say
    /// so).
    ServiceGarbage,
    /// An assignment-token header is present (binding checked
    /// downstream by the stream gate).
    AssignmentHeader,
}

/// Why the layer refused dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// `TenantJwt` method without verified tenant claims.
    MissingTenantToken,
    /// `AssignmentToken` method without the token header.
    MissingAssignmentToken,
    /// `Service` method without a service token.
    MissingServiceToken,
    /// `Service` method with a token that failed verification.
    ServiceVerificationFailed,
}

/// The transport-layer verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerVerdict {
    /// Dispatch the request (handler-side checks still apply).
    Admit,
    /// Refuse dispatch.
    Reject(RejectReason),
}

/// Projection carrying ONLY the JWT knob — the [`TenantJwt`] arm's
/// entire view of the configuration.
///
/// [`TenantJwt`]: CredentialClass::TenantJwt
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TenantJwtKnob(pub bool);

/// Projection carrying ONLY the service knob — the [`Service`] arm's
/// entire view of the configuration.
///
/// [`Service`]: CredentialClass::Service
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceKnob(pub bool);

/// Projection carrying ONLY the assignment knob — the
/// [`AssignmentToken`] arm's entire view of the configuration.
///
/// [`AssignmentToken`]: CredentialClass::AssignmentToken
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssignmentKnob(pub bool);

/// `AssignmentToken` arm: enforce-when-configured presence pin.
const fn arm_assignment(knob: AssignmentKnob, presented: Presented) -> LayerVerdict {
    if !knob.0 {
        return LayerVerdict::Admit;
    }
    match presented {
        Presented::AssignmentHeader => LayerVerdict::Admit,
        _ => LayerVerdict::Reject(RejectReason::MissingAssignmentToken),
    }
}

/// `TenantJwt` arm: enforce-when-configured verified-claims
/// requirement. No service bypass (bug_290 owner decision).
const fn arm_tenant_jwt(knob: TenantJwtKnob, presented: Presented) -> LayerVerdict {
    if !knob.0 {
        return LayerVerdict::Admit;
    }
    match presented {
        Presented::TenantClaims => LayerVerdict::Admit,
        _ => LayerVerdict::Reject(RejectReason::MissingTenantToken),
    }
}

/// `Service` arm: enforce-when-configured VERIFIED service token.
/// Tenant claims are a foreign credential here — the pre-kernel
/// tenant leg is deleted (bug_237).
const fn arm_service(knob: ServiceKnob, presented: Presented) -> LayerVerdict {
    if !knob.0 {
        return LayerVerdict::Admit;
    }
    match presented {
        Presented::ServiceVerified => LayerVerdict::Admit,
        Presented::ServiceGarbage => LayerVerdict::Reject(RejectReason::ServiceVerificationFailed),
        _ => LayerVerdict::Reject(RejectReason::MissingServiceToken),
    }
}

/// The pure, total layer verdict.
///
/// This dispatch is the trusted residual: it constructs each arm's
/// single-knob projection from the full configuration. Everything
/// else is covered by the type system (arms cannot name foreign
/// knobs); the dispatch itself is covered by
/// `check_foreign_knob_independence`.
// r[impl store.authz.declared-verifier]
#[must_use]
pub const fn decide(
    class: CredentialClass,
    cfg: VerifierConfig,
    presented: Presented,
) -> LayerVerdict {
    match class {
        CredentialClass::AssignmentToken => arm_assignment(AssignmentKnob(cfg.hmac), presented),
        CredentialClass::TenantJwt => arm_tenant_jwt(TenantJwtKnob(cfg.jwt), presented),
        CredentialClass::Service => arm_service(ServiceKnob(cfg.service), presented),
        // Transport admits; Public is public by recorded rationale,
        // HandlerEnforced methods require a typed witness on the
        // handler data path (the table row names the check).
        CredentialClass::Public { .. } | CredentialClass::HandlerEnforced { .. } => {
            LayerVerdict::Admit
        }
    }
}

/// Boot key-coherence verdict: which knob is missing, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCoherence {
    /// The configuration is coherent — serve.
    Coherent,
    /// JWT is on but the service HMAC key is missing.
    MissingServiceKey,
    /// JWT is on but the assignment HMAC key is missing.
    MissingAssignmentKey,
    /// JWT is on and BOTH HMAC keys are missing.
    MissingBothKeys,
}

/// The startup coherence predicate: `jwt ⇒ (service ∧ hmac)`.
///
/// Refused states — `(jwt, service, hmac)` ∈ {(1,0,0), (1,0,1),
/// (1,1,0)} — are exactly the exploitable half-configurations
/// (bug_237: with JWT on and a key missing, some keyed class is
/// silently unenforced while the deployment believes itself
/// authenticated). Dev `(0,0,0)`, the helm default `(0,1,1)`, full
/// `(1,1,1)`, and the keys-without-jwt states keep booting
/// (dual-mode-permanent doctrine).
// r[impl store.authz.key-coherence]
#[must_use]
pub const fn key_coherence(cfg: VerifierConfig) -> KeyCoherence {
    if !cfg.jwt {
        return KeyCoherence::Coherent;
    }
    match (cfg.service, cfg.hmac) {
        (true, true) => KeyCoherence::Coherent,
        (false, true) => KeyCoherence::MissingServiceKey,
        (true, false) => KeyCoherence::MissingAssignmentKey,
        (false, false) => KeyCoherence::MissingBothKeys,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLASSES: [CredentialClass; 5] = [
        CredentialClass::AssignmentToken,
        CredentialClass::TenantJwt,
        CredentialClass::Service,
        CredentialClass::Public {
            rationale: "unit-test row",
        },
        CredentialClass::HandlerEnforced {
            check: HandlerCheck::IngestToken,
        },
    ];
    const PRESENTED: [Presented; 5] = [
        Presented::None,
        Presented::TenantClaims,
        Presented::ServiceVerified,
        Presented::ServiceGarbage,
        Presented::AssignmentHeader,
    ];

    fn configs() -> impl Iterator<Item = VerifierConfig> {
        (0u8..8).map(|b| VerifierConfig {
            jwt: b & 1 != 0,
            service: b & 2 != 0,
            hmac: b & 4 != 0,
        })
    }

    /// The dead tenant leg stays dead: tenant claims on a `Service`
    /// method never admit while the service verifier is keyed.
    /// (bug_237 — pre-kernel, `ServiceOrTenant` admitted this and the
    /// half-config made it cluster admin.)
    #[test]
    fn tenant_claims_never_admit_a_keyed_service_method() {
        for cfg in configs().filter(|c| c.service) {
            assert_eq!(
                decide(CredentialClass::Service, cfg, Presented::TenantClaims),
                LayerVerdict::Reject(RejectReason::MissingServiceToken),
            );
        }
    }

    /// Enforce-when-configured: knob off admits everything (dev mode),
    /// for every keyed class.
    #[test]
    fn unkeyed_classes_admit_dev_mode() {
        for p in PRESENTED {
            let off = VerifierConfig {
                jwt: false,
                service: false,
                hmac: false,
            };
            for class in CLASSES {
                assert_eq!(decide(class, off, p), LayerVerdict::Admit);
            }
        }
    }

    /// Keyed classes never admit without their declared credential.
    #[test]
    fn keyed_classes_reject_foreign_or_absent_credentials() {
        let full = VerifierConfig {
            jwt: true,
            service: true,
            hmac: true,
        };
        for p in PRESENTED {
            let tj = decide(CredentialClass::TenantJwt, full, p);
            assert_eq!(
                tj == LayerVerdict::Admit,
                p == Presented::TenantClaims,
                "TenantJwt × {p:?}"
            );
            let sv = decide(CredentialClass::Service, full, p);
            assert_eq!(
                sv == LayerVerdict::Admit,
                p == Presented::ServiceVerified,
                "Service × {p:?}"
            );
            let at = decide(CredentialClass::AssignmentToken, full, p);
            assert_eq!(
                at == LayerVerdict::Admit,
                p == Presented::AssignmentHeader,
                "AssignmentToken × {p:?}"
            );
        }
    }

    /// Garbage service tokens are distinguishable from absent ones.
    #[test]
    fn garbage_service_token_says_verification_failed() {
        let cfg = VerifierConfig {
            jwt: false,
            service: true,
            hmac: false,
        };
        assert_eq!(
            decide(CredentialClass::Service, cfg, Presented::ServiceGarbage),
            LayerVerdict::Reject(RejectReason::ServiceVerificationFailed),
        );
    }

    /// The coherence partition, exhaustively: refused = {(1,0,0),
    /// (1,0,1), (1,1,0)}; booting = the other five.
    // r[verify store.authz.key-coherence]
    #[test]
    fn key_coherence_partition_is_exact() {
        for cfg in configs() {
            let verdict = key_coherence(cfg);
            let expect = match (cfg.jwt, cfg.service, cfg.hmac) {
                (true, false, false) => KeyCoherence::MissingBothKeys,
                (true, false, true) => KeyCoherence::MissingServiceKey,
                (true, true, false) => KeyCoherence::MissingAssignmentKey,
                _ => KeyCoherence::Coherent,
            };
            assert_eq!(verdict, expect, "{cfg:?}");
        }
    }

    /// Every class declares at most one family, and the keyed classes
    /// declare exactly the family their arm consumes.
    #[test]
    fn consumes_matches_arms() {
        assert_eq!(
            consumes(CredentialClass::AssignmentToken),
            Some(VerifierFamily::Assignment)
        );
        assert_eq!(
            consumes(CredentialClass::TenantJwt),
            Some(VerifierFamily::TenantJwt)
        );
        assert_eq!(
            consumes(CredentialClass::Service),
            Some(VerifierFamily::Service)
        );
        assert_eq!(consumes(CredentialClass::Public { rationale: "" }), None);
        assert_eq!(
            consumes(CredentialClass::HandlerEnforced {
                check: HandlerCheck::SigVisibility
            }),
            None
        );
    }
}

/// CBMC proof harnesses (run by `kani-rio-authz-kernel`,
/// `expectedHarnesses = 4` in nix/kani.nix).
#[cfg(kani)]
mod proofs {
    use super::*;

    fn any_class() -> CredentialClass {
        match kani::any::<u8>() % 5 {
            0 => CredentialClass::AssignmentToken,
            1 => CredentialClass::TenantJwt,
            2 => CredentialClass::Service,
            3 => CredentialClass::Public { rationale: "" },
            _ => CredentialClass::HandlerEnforced {
                check: match kani::any::<u8>() % 4 {
                    0 => HandlerCheck::IngestToken,
                    1 => HandlerCheck::SigVisibility,
                    2 => HandlerCheck::ServiceCaller,
                    _ => HandlerCheck::EndUserRejected,
                },
            },
        }
    }

    fn any_cfg() -> VerifierConfig {
        VerifierConfig {
            jwt: kani::any(),
            service: kani::any(),
            hmac: kani::any(),
        }
    }

    fn any_presented() -> Presented {
        match kani::any::<u8>() % 5 {
            0 => Presented::None,
            1 => Presented::TenantClaims,
            2 => Presented::ServiceVerified,
            3 => Presented::ServiceGarbage,
            _ => Presented::AssignmentHeader,
        }
    }

    /// Knob projection per family.
    fn family_knob(cfg: VerifierConfig, fam: VerifierFamily) -> bool {
        match fam {
            VerifierFamily::TenantJwt => cfg.jwt,
            VerifierFamily::Service => cfg.service,
            VerifierFamily::Assignment => cfg.hmac,
        }
    }

    /// THE dispatch pin: two configurations that agree on the class's
    /// declared family (or any two at all, for undeclared classes)
    /// produce identical verdicts — no arm reads a foreign knob, and
    /// the dispatch wires each arm to exactly its declared knob.
    #[kani::proof]
    fn check_foreign_knob_independence() {
        let class = any_class();
        let a = any_cfg();
        let b = any_cfg();
        let p = any_presented();
        if let Some(fam) = consumes(class) {
            kani::assume(family_knob(a, fam) == family_knob(b, fam));
        }
        assert_eq!(decide(class, a, p), decide(class, b, p));
    }

    /// The boot partition is exactly `jwt ⇒ (service ∧ hmac)`: a
    /// refused state is one with jwt on and a key missing, and every
    /// refusal names a genuinely-missing knob.
    #[kani::proof]
    fn check_key_coherence_partition() {
        let cfg = any_cfg();
        let v = key_coherence(cfg);
        let coherent = !cfg.jwt || (cfg.service && cfg.hmac);
        assert_eq!(v == KeyCoherence::Coherent, coherent);
        match v {
            KeyCoherence::Coherent => {}
            KeyCoherence::MissingServiceKey => assert!(cfg.jwt && !cfg.service && cfg.hmac),
            KeyCoherence::MissingAssignmentKey => assert!(cfg.jwt && cfg.service && !cfg.hmac),
            KeyCoherence::MissingBothKeys => assert!(cfg.jwt && !cfg.service && !cfg.hmac),
        }
    }

    /// A keyed class whose knob is ON never admits anything but its
    /// declared accepting presentation — there is no undeclared admit
    /// path (the dead-leg deletion, proven).
    #[kani::proof]
    fn check_no_undeclared_admit() {
        let class = any_class();
        let cfg = any_cfg();
        let p = any_presented();
        let Some(fam) = consumes(class) else { return };
        kani::assume(family_knob(cfg, fam));
        if decide(class, cfg, p) == LayerVerdict::Admit {
            let accepting = match fam {
                VerifierFamily::TenantJwt => Presented::TenantClaims,
                VerifierFamily::Service => Presented::ServiceVerified,
                VerifierFamily::Assignment => Presented::AssignmentHeader,
            };
            assert_eq!(p, accepting);
        }
    }

    /// `decide` is total: defined (panic-free) over the whole input
    /// product, and an unkeyed knob always admits (dual-mode).
    #[kani::proof]
    fn check_decide_total() {
        let class = any_class();
        let cfg = any_cfg();
        let p = any_presented();
        let v = decide(class, cfg, p);
        if let Some(fam) = consumes(class) {
            if !family_knob(cfg, fam) {
                assert_eq!(v, LayerVerdict::Admit);
            }
        } else {
            assert_eq!(v, LayerVerdict::Admit);
        }
    }
}
