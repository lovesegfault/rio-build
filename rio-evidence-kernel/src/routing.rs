//! The four-arm Unobtainable consumption routing — PURE (no IO, no
//! clocks), lifted from `rio-scheduler/src/actor/materialize.rs`
//! (bughunt wave, A4: rio-scheduler is a bin crate and the kani gate is
//! lib-only; the decision surface lives here so the proofs sweep it).
//!
//! The scheduler keeps thin wiring: it projects the durable facts
//! (live wanted paths, durable closure evidence, the job origin, the
//! prior one-shot count, the same-transaction re-probe answer) and
//! executes the returned arm.

use crate::ClosureEvidence;

/// What the Unobtainable routing decided (design §2.4's four arms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnobtainableRouting {
    /// Arm 0, covered: consume as success-for-live-interest.
    CompleteForLiveInterest,
    /// Arms 0 (uncovered) / 3a: job returns to pending.
    ReArm,
    /// Arms 1/2: node becomes from-source dispatchable.
    ResolveFromSource,
    /// Arm 3b: fail-fast every live DAG-interested build.
    FailFast,
}

/// The same-transaction FMP re-probe answer over the live wanted paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReprobeAnswer {
    /// Every live-wanted path present, substitutable, or indeterminate.
    Obtainable,
    /// Some live-wanted path confirmed missing-and-unsubstitutable.
    ConfirmedMissing,
}

/// The kernel's CLOSED deterministic-refusal alphabet (bug_084) — the
/// decoded form of the wire's `UnobtainableRefusal`: which refusal
/// axes the store observed on paths PRESENT upstream that could not
/// be taken. A refusal is deterministic store-integrity feedback (a
/// re-probe or resubmit meets the same refusal), so the routing
/// matches this alphabet EXHAUSTIVELY — zero `_` wildcard arms — and
/// a future axis is a compile error at every decision site instead of
/// a per-axis re-plumbing exercise (the bug_084 shape: wave-4 typed
/// trust end-to-end and content store-side only; the gap between the
/// two closes was a refusal routing as a clean miss).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// No refusal axis observed: every unobtained path is a genuine
    /// miss (the clean lane — re-arm/fail-fast stay reachable).
    None,
    /// Present upstream but no narinfo signature verified against
    /// `trusted_keys` (merged_bug_263).
    Trust,
    /// Present upstream but claiming different bytes than the stored
    /// row (merged_bug_046 — the dedup-arm content disagreement).
    Content,
    /// Both axes observed across the walk's unobtained paths.
    TrustAndContent,
    /// A wire value this kernel does not know: a FUTURE refusal axis.
    /// Routed conservatively AS a refusal (from-source), never
    /// laundered into the clean lane — the decode chokepoint maps
    /// unknown nonzero wire values here precisely so the next
    /// alphabet evolution cannot repeat bug_084.
    Unrecognized,
}

impl Refusal {
    /// Whether ANY refusal axis (known or future) was observed — the
    /// guard under which re-arm and fail-fast are unreachable.
    pub fn is_refused(self) -> bool {
        // Exhaustive BY DESIGN (no `_`): a new variant must decide
        // its refused-ness here or the build breaks.
        match self {
            Refusal::None => false,
            Refusal::Trust
            | Refusal::Content
            | Refusal::TrustAndContent
            | Refusal::Unrecognized => true,
        }
    }
}

/// The non-empty live-wanted witness (merged_bug_194): a verifiable
/// wanted set that EXISTS. Constructing one requires at least one
/// non-empty path, so the covered checks below cannot be vacuous —
/// "covered over nothing" is unrepresentable at the type level. A
/// caller whose resolution yields no verifiable paths holds `None` and
/// MUST take its conservative branch (re-arm / infra-failure), never a
/// completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveWanted(Vec<String>);

impl LiveWanted {
    /// Build the witness: `None` when the set is empty or any entry is
    /// the empty string (the floating-CA placeholder shape).
    pub fn new(paths: Vec<String>) -> Option<Self> {
        if paths.is_empty() || paths.iter().any(|p| p.is_empty()) {
            return None;
        }
        Some(Self(paths))
    }

    /// The witnessed paths (non-empty, no empty strings).
    pub fn paths(&self) -> &[String] {
        &self.0
    }

    /// Membership in the witnessed set.
    pub fn contains(&self, path: &str) -> bool {
        self.0.iter().any(|p| p == path)
    }
}

/// The inputs of one Unobtainable routing decision.
pub struct RoutingInputs<'a> {
    /// Paths the executor confirmed absent upstream.
    pub missing_paths: &'a [String],
    /// REFERENCE paths (narinfo closure extensions, never live-wanted
    /// seeds) the executor confirmed absent upstream (merged_bug_193:
    /// the walk's wanted-miss vs closure-reference-miss distinction —
    /// proto `Unobtainable.missing_reference_paths`, plus the consumer-
    /// side skew partition for old-store reports). A non-empty set is
    /// a CONFIRMED CLOSURE HOLE: the moot arm must never complete over
    /// it, whatever the live-wanted coverage says.
    pub missing_references: &'a [String],
    /// Paths the executor verified present (and pinned).
    pub verified_paths: &'a [String],
    /// The live effective wanted PATHS (the §6 join, resolved to store
    /// paths by the caller inside the consumption transaction, carrier
    /// paths unioned in) — witnessed non-empty.
    pub live_wanted_paths: &'a LiveWanted,
    /// The durable closure-evidence cell (`classify_durable_evidence`
    /// — never the in-memory child set).
    pub durable_evidence: ClosureEvidence,
    /// Prior materialization_unobtainable rows for THIS job (the
    /// re-probe one-shot; design §2.4 arm 3).
    pub prior_unobtainable_count: u32,
    /// The same-transaction FMP re-probe answer over live_wanted_paths.
    /// `None` = not fetched (arms 0–2 decided without it); the caller
    /// fetches it only when arms 0–2 do not apply (purity by
    /// parameterization — design §9.4).
    pub reprobe: Option<ReprobeAnswer>,
    /// Whether the consumed job's `origin == 'pruned'` — the durable
    /// successor of the walk-era pruned mark (design §4/A2/A13,
    /// T-D2.1) and the arm-3 settlement discriminator (finding 11):
    /// only a pruned-origin job may fail-fast (the prune deliberately
    /// dropped the node's closure, so from-source is doomed); a
    /// non-pruned-origin job whose evidence is broken by structure
    /// (childless leaf / hole) releases to from-source dispatch
    /// instead — non-pruned nodes are never affected, whatever their
    /// evidence.
    pub pruned_origin: bool,
    /// The store's typed refusal verdict (bug_084, proto
    /// `Unobtainable.refusal` decoded at the scheduler's ONE
    /// chokepoint): which deterministic refusal axes rode the outcome
    /// — paths PRESENT upstream but refused on signature trust
    /// (merged_bug_263), content agreement (merged_bug_046), both, or
    /// a future axis this kernel does not know (`Unrecognized`). The
    /// settlement consumes the alphabet typed, end-to-end: the
    /// re-probe is a presence-only HEAD (blind to every refusal axis)
    /// so its Obtainable answer never licenses a ReArm, and a
    /// pruned-origin fail-fast is never the verdict (a resubmit meets
    /// the same refusal) — a refused settlement resolves from-source.
    pub refusal: Refusal,
}

// r[impl sched.materialize.routing+7]
/// The four-arm routing core. PURE (no IO, no clocks); the FMP
/// re-probe answer is an input.
///
/// The probe-failure case: the consumption HANDLER maps "the re-probe
/// RPC itself failed/timed out" to ReArm before calling this core (B3:
/// an indeterminate answer never fail-fasts). Under a typed refusal
/// the handler does not probe at all (the round-trip is doomed — a
/// presence-only HEAD cannot answer a trust/content question, and the
/// probe-failure ReArm would re-meet the refusal), so with
/// `Refusal::None` the core's `None` reprobe arm is only reachable as
/// one-shot-spent, and with any refusal the arm-3 refusal match owns
/// the verdict before the reprobe is consulted.
pub fn route_unobtainable(inputs: &RoutingInputs<'_>) -> UnobtainableRouting {
    let missing_live: bool = inputs
        .missing_paths
        .iter()
        .any(|p| inputs.live_wanted_paths.contains(p));
    let covered = inputs
        .live_wanted_paths
        .paths()
        .iter()
        .all(|w| inputs.verified_paths.contains(w));
    route_from_classes(
        missing_live,
        !inputs.missing_references.is_empty(),
        covered,
        inputs.durable_evidence,
        inputs.prior_unobtainable_count > 0,
        inputs.reprobe,
        inputs.pruned_origin,
        inputs.refusal,
    )
}

/// The set-free routing core (the establish.rs discipline: heap
/// collections under CBMC reach `getrandom`, so the kani sweep targets
/// THIS — `route_unobtainable` only collapses its slice inputs to the
/// three predicates, each pinned by the unit tests above it).
///
/// Arm 0 — moot-failure (the C3 arm). The witness type guarantees the
/// wanted set is non-empty, so `covered` cannot be vacuous.
/// merged_bug_193: arm 0 additionally requires NO confirmed reference
/// miss — a hole in the dependency closure is never moot, even when
/// every live-wanted root is itself covered (the covered root's
/// closure is exactly what the reference miss punctured). With a
/// reference miss the decision falls through to arms 1–3, where
/// Vouched/Pending route from-source and the leaf/holed arm settles on
/// the origin discriminator.
#[allow(clippy::too_many_arguments)] // the set-free CBMC target restates the axes
pub(crate) fn route_from_classes(
    missing_live: bool,
    refs_missing: bool,
    covered: bool,
    durable_evidence: ClosureEvidence,
    one_shot_spent: bool,
    reprobe: Option<ReprobeAnswer>,
    pruned_origin: bool,
    refusal: Refusal,
) -> UnobtainableRouting {
    // Arm 0 is the refusal-MOOT cell BY DESIGN: nothing the live
    // interest wants is missing, so whatever the store refused never
    // intersected the live want — completion/re-arm semantics stand
    // (pinned by the reachability covers in `mod proofs`).
    if !missing_live && !refs_missing {
        return if covered {
            UnobtainableRouting::CompleteForLiveInterest
        } else {
            UnobtainableRouting::ReArm
        };
    }
    // Arms 1/2 — durable Vouched / Pending: from-source.
    match durable_evidence {
        ClosureEvidence::Vouched | ClosureEvidence::Pending => {
            return UnobtainableRouting::ResolveFromSource;
        }
        ClosureEvidence::ChildlessLeaf | ClosureEvidence::Holed => {}
    }
    // Arm 3 opens on the refusal alphabet, EXHAUSTIVELY — no wildcard
    // (bug_084): a deterministic refusal with anything missing settles
    // from-source before the re-probe or the origin is consulted. The
    // re-probe is a presence-only HEAD — under any refusal its
    // Obtainable answer confirms only the presence that was never in
    // question, so re-arming would burn the one-shot on a doomed
    // re-attempt against the same refusal (merged_bug_263 for trust;
    // merged_bug_046's content disagreement is the same shape — a key
    // rotation will not fix disagreeing bytes, and neither will a
    // retry). And a refused settlement never fail-fasts, pruned
    // origin or not — the resubmit-directing error sends the user
    // into the SAME refusal (an unbounded resubmit loop); from-source
    // is the repair that bypasses the refused upstream artifact. An
    // Unrecognized value is a FUTURE axis decoded conservatively:
    // refusal semantics, never the clean lane. A 6th variant breaks
    // this match at compile time — the decision site is the closure
    // set.
    match refusal {
        Refusal::Trust | Refusal::Content | Refusal::TrustAndContent | Refusal::Unrecognized => {
            return UnobtainableRouting::ResolveFromSource;
        }
        Refusal::None => {}
    }
    // Arm 3, unrefused — the re-probe gate (re-arm and fail-fast are
    // reachable ONLY here, under `Refusal::None`).
    match reprobe {
        Some(ReprobeAnswer::Obtainable) if !one_shot_spent => UnobtainableRouting::ReArm,
        // Re-probe confirms missing, or the one-shot is spent. (A
        // missing probe is mapped to ReArm by the caller before this
        // core runs — see the doc above.)
        //
        // The settlement discriminates on the consumed job's ORIGIN
        // (finding 11, durably re-sourced by T-D2.1): only a
        // pruned-origin job fail-fasts — the prune deliberately
        // dropped the node's closure ("this was not built because
        // outputs were expected available"), so from-source is doomed
        // and the resubmit-directing error is the correct verdict. A
        // non-pruned-origin job — a genuine leaf whose evidence is
        // broken by structure (childless) or holed — releases to
        // from-source dispatch instead; non-pruned nodes are never
        // affected, whatever their evidence.
        _ if pruned_origin => UnobtainableRouting::FailFast,
        _ => UnobtainableRouting::ResolveFromSource,
    }
}

/// Success-consumption coverage check (the CE-17 closer): the live
/// wanted set is covered by what the execution ingested or verified.
/// The witness type makes the check non-vacuous by construction.
// r[impl sched.materialize.routing+7]
pub fn success_covers_live_wanted(
    ingested_paths: &[String],
    verified_paths: &[String],
    live_wanted_paths: &LiveWanted,
) -> bool {
    live_wanted_paths
        .paths()
        .iter()
        .all(|w| ingested_paths.contains(w) || verified_paths.contains(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lw(paths: &[&str]) -> LiveWanted {
        LiveWanted::new(paths.iter().map(|s| s.to_string()).collect()).expect("non-empty")
    }

    #[test]
    fn live_wanted_rejects_empty_and_empty_strings() {
        assert!(LiveWanted::new(vec![]).is_none());
        assert!(LiveWanted::new(vec![String::new()]).is_none());
        assert!(LiveWanted::new(vec!["/nix/store/x".into(), String::new()]).is_none());
        assert!(LiveWanted::new(vec!["/nix/store/x".into()]).is_some());
    }

    #[test]
    fn covered_over_witnessed_set_is_never_vacuous() {
        let w = lw(&["/nix/store/a"]);
        assert!(!success_covers_live_wanted(&[], &[], &w));
        assert!(success_covers_live_wanted(
            &["/nix/store/a".into()],
            &[],
            &w
        ));
    }

    #[test]
    fn leaf_and_holed_fall_to_arm_three() {
        for evidence in [ClosureEvidence::ChildlessLeaf, ClosureEvidence::Holed] {
            let routing = route_unobtainable(&RoutingInputs {
                missing_paths: &["/nix/store/a".into()],
                missing_references: &[],
                verified_paths: &[],
                live_wanted_paths: &lw(&["/nix/store/a"]),
                durable_evidence: evidence,
                prior_unobtainable_count: 1,
                reprobe: Some(ReprobeAnswer::ConfirmedMissing),
                pruned_origin: true,
                refusal: Refusal::None,
            });
            assert_eq!(routing, UnobtainableRouting::FailFast);
            let routing = route_unobtainable(&RoutingInputs {
                missing_paths: &["/nix/store/a".into()],
                missing_references: &[],
                verified_paths: &[],
                live_wanted_paths: &lw(&["/nix/store/a"]),
                durable_evidence: evidence,
                prior_unobtainable_count: 1,
                reprobe: Some(ReprobeAnswer::ConfirmedMissing),
                pruned_origin: false,
                refusal: Refusal::None,
            });
            assert_eq!(routing, UnobtainableRouting::ResolveFromSource);
        }
    }

    /// merged_bug_263 (bughunt-4): a trust-refused settlement at the
    /// leaf arm — the sig-blind Obtainable re-probe (HEAD sees the
    /// path PRESENT; presence was never the question) must not burn
    /// the one-shot on a ReArm doomed to the same refusal. Pre-fix
    /// red is compile-level (the result.rs:447 precedent): the typed
    /// refusal died at the proto boundary — neither
    /// `Unobtainable.trust_refused` nor the `RoutingInputs` axis
    /// existed, so the doomed ReArm was the only expressible shape.
    /// Retargeted to `Refusal::Trust` at the bug_084 alphabet close.
    #[test]
    fn routing_trust_refused_skips_sig_blind_rearm() {
        let routing = route_unobtainable(&RoutingInputs {
            missing_paths: &["/nix/store/a".into()],
            missing_references: &[],
            verified_paths: &[],
            live_wanted_paths: &lw(&["/nix/store/a"]),
            durable_evidence: ClosureEvidence::ChildlessLeaf,
            prior_unobtainable_count: 0,
            reprobe: Some(ReprobeAnswer::Obtainable),
            pruned_origin: false,
            refusal: Refusal::Trust,
        });
        // Old shape (trust axis unexpressed): ReArm — the doomed
        // one-shot burn.
        assert_eq!(routing, UnobtainableRouting::ResolveFromSource);
    }

    /// merged_bug_263: a PRUNED-origin trust-refused settlement never
    /// fail-fasts — the resubmit-directing error would send the user
    /// into the same refusal, unbounded; from-source bypasses the
    /// refused upstream artifact.
    #[test]
    fn routing_trust_refused_pruned_resolves_from_source() {
        for reprobe in [
            Some(ReprobeAnswer::ConfirmedMissing),
            Some(ReprobeAnswer::Obtainable),
            None,
        ] {
            let routing = route_unobtainable(&RoutingInputs {
                missing_paths: &["/nix/store/a".into()],
                missing_references: &[],
                verified_paths: &[],
                live_wanted_paths: &lw(&["/nix/store/a"]),
                durable_evidence: ClosureEvidence::Holed,
                prior_unobtainable_count: 1,
                reprobe,
                pruned_origin: true,
                refusal: Refusal::Trust,
            });
            assert_eq!(
                routing,
                UnobtainableRouting::ResolveFromSource,
                "reprobe={reprobe:?}"
            );
        }
    }

    /// bug_084: a CONTENT-refused settlement at the leaf arm with an
    /// Obtainable re-probe and an UNSPENT one-shot settles from-source.
    /// The re-probe is a presence-blind HEAD — the mismatched path IS
    /// present, so Obtainable confirms nothing the refusal disputed.
    /// Old shape (strawman disclosure — the red is compile-level, the
    /// merged_bug_263 precedent: pre-fix neither the proto field nor
    /// the `RoutingInputs` axis could express a content refusal, so
    /// this exact input routed ReArm and burned the one-shot on a
    /// re-attempt doomed to the same disagreeing bytes).
    #[test]
    fn routing_content_refused_skips_presence_blind_rearm() {
        let routing = route_unobtainable(&RoutingInputs {
            missing_paths: &["/nix/store/a".into()],
            missing_references: &[],
            verified_paths: &[],
            live_wanted_paths: &lw(&["/nix/store/a"]),
            durable_evidence: ClosureEvidence::ChildlessLeaf,
            prior_unobtainable_count: 0,
            reprobe: Some(ReprobeAnswer::Obtainable),
            pruned_origin: false,
            refusal: Refusal::Content,
        });
        // Old shape (content axis unexpressed): ReArm — the doomed
        // one-shot burn.
        assert_eq!(routing, UnobtainableRouting::ResolveFromSource);
    }

    /// bug_084: a PRUNED-origin content-refused settlement never
    /// fail-fasts, whatever the re-probe answered — the
    /// resubmit-directing error would send the user into an unbounded
    /// resubmit loop against the same disagreeing bytes; from-source
    /// bypasses the refused upstream artifact. Old shape (strawman
    /// disclosure, compile-level red as above): FailFast — the
    /// resubmit loop. The sibling cells ride the same exhaustive
    /// match: TrustAndContent and the conservative Unrecognized lane
    /// are asserted in the same sweep.
    #[test]
    fn routing_content_refused_pruned_never_failfasts() {
        for refusal in [
            Refusal::Content,
            Refusal::TrustAndContent,
            Refusal::Unrecognized,
        ] {
            for reprobe in [
                Some(ReprobeAnswer::ConfirmedMissing),
                Some(ReprobeAnswer::Obtainable),
                None,
            ] {
                let routing = route_unobtainable(&RoutingInputs {
                    missing_paths: &["/nix/store/a".into()],
                    missing_references: &[],
                    verified_paths: &[],
                    live_wanted_paths: &lw(&["/nix/store/a"]),
                    durable_evidence: ClosureEvidence::Holed,
                    prior_unobtainable_count: 1,
                    reprobe,
                    pruned_origin: true,
                    refusal,
                });
                assert_eq!(
                    routing,
                    UnobtainableRouting::ResolveFromSource,
                    "refusal={refusal:?} reprobe={reprobe:?}"
                );
            }
        }
    }

    /// merged_bug_193 (kernel half): a confirmed REFERENCE miss can
    /// never take the moot-completion arm, even with every live-wanted
    /// root verified. Pre-fix the inputs could not express the
    /// distinction (`missing_paths` lumped both): the covered check
    /// completed over a punctured closure.
    #[test]
    fn routing_reference_miss_never_completes() {
        // Covered live-wanted + a reference hole: arms 1/2 (Vouched/
        // Pending) route from-source; leaf/holed falls to arm 3.
        for (evidence, want) in [
            (
                ClosureEvidence::Vouched,
                UnobtainableRouting::ResolveFromSource,
            ),
            (
                ClosureEvidence::Pending,
                UnobtainableRouting::ResolveFromSource,
            ),
        ] {
            let routing = route_unobtainable(&RoutingInputs {
                missing_paths: &[],
                missing_references: &["/nix/store/dep".into()],
                verified_paths: &["/nix/store/a".into()],
                live_wanted_paths: &lw(&["/nix/store/a"]),
                durable_evidence: evidence,
                prior_unobtainable_count: 0,
                reprobe: None,
                pruned_origin: false,
                refusal: Refusal::None,
            });
            assert_ne!(routing, UnobtainableRouting::CompleteForLiveInterest);
            assert_eq!(routing, want);
        }
        // Same shape, leaf evidence + spent one-shot + pruned origin:
        // the hole fail-fasts instead of completing.
        let routing = route_unobtainable(&RoutingInputs {
            missing_paths: &[],
            missing_references: &["/nix/store/dep".into()],
            verified_paths: &["/nix/store/a".into()],
            live_wanted_paths: &lw(&["/nix/store/a"]),
            durable_evidence: ClosureEvidence::ChildlessLeaf,
            prior_unobtainable_count: 1,
            reprobe: Some(ReprobeAnswer::ConfirmedMissing),
            pruned_origin: true,
            refusal: Refusal::None,
        });
        assert_eq!(routing, UnobtainableRouting::FailFast);
        // And the no-hole twin still completes (the moot arm survives).
        let routing = route_unobtainable(&RoutingInputs {
            missing_paths: &[],
            missing_references: &[],
            verified_paths: &["/nix/store/a".into()],
            live_wanted_paths: &lw(&["/nix/store/a"]),
            durable_evidence: ClosureEvidence::Vouched,
            prior_unobtainable_count: 0,
            reprobe: None,
            pruned_origin: false,
            refusal: Refusal::None,
        });
        assert_eq!(routing, UnobtainableRouting::CompleteForLiveInterest);
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    fn any_evidence() -> ClosureEvidence {
        match kani::any::<u8>() {
            0 => ClosureEvidence::Vouched,
            1 => ClosureEvidence::Pending,
            2 => ClosureEvidence::ChildlessLeaf,
            _ => ClosureEvidence::Holed,
        }
    }

    fn any_reprobe() -> Option<ReprobeAnswer> {
        match kani::any::<u8>() {
            0 => None,
            1 => Some(ReprobeAnswer::Obtainable),
            _ => Some(ReprobeAnswer::ConfirmedMissing),
        }
    }

    fn any_refusal() -> Refusal {
        match kani::any::<u8>() {
            0 => Refusal::None,
            1 => Refusal::Trust,
            2 => Refusal::Content,
            3 => Refusal::TrustAndContent,
            _ => Refusal::Unrecognized,
        }
    }

    /// The shared body of the per-variant refusal settlement proofs
    /// (bug_084, the K6 lesson: split per ARM, never quantify a
    /// symbolic collection — each named variant gets its own harness
    /// over the full bounded domain of every OTHER axis): a typed
    /// refusal with anything actually missing NEVER ReArms (the
    /// re-probe is presence-blind) and NEVER FailFasts (a resubmit
    /// meets the same refusal) — every such settlement resolves
    /// from-source. Arm 0 (nothing missing) is the refusal-moot cell
    /// and keeps its complete/re-arm semantics; the per-harness covers
    /// pin all three lanes reachable (non-vacuity).
    fn refused_settles_from_source_body(refusal: Refusal) {
        let missing_live: bool = kani::any();
        let refs_missing: bool = kani::any();
        let routing = route_from_classes(
            missing_live,
            refs_missing,
            kani::any(),
            any_evidence(),
            kani::any(),
            any_reprobe(),
            kani::any(),
            refusal,
        );
        if missing_live || refs_missing {
            assert!(routing == UnobtainableRouting::ResolveFromSource);
        }
        kani::cover!(routing == UnobtainableRouting::ResolveFromSource);
        // The refusal-moot cell stays live under the axis.
        kani::cover!(routing == UnobtainableRouting::CompleteForLiveInterest);
        kani::cover!(routing == UnobtainableRouting::ReArm);
    }

    /// merged_bug_193/194 (the no-vacuous-completion law): the routing
    /// completes ONLY from the clean-and-covered cell — no live-wanted
    /// miss, no confirmed reference hole, full coverage. Every other
    /// cell is structurally unable to produce
    /// `CompleteForLiveInterest`. (The covered-over-nothing half is
    /// unrepresentable at the type level: `LiveWanted` witnesses a
    /// non-empty set.) Redrawn over the full `Refusal` alphabet at the
    /// bug_084 close — the law is refusal-independent.
    #[kani::proof]
    fn check_route_no_vacuous_complete() {
        let missing_live: bool = kani::any();
        let refs_missing: bool = kani::any();
        let covered: bool = kani::any();
        let routing = route_from_classes(
            missing_live,
            refs_missing,
            covered,
            any_evidence(),
            kani::any(),
            any_reprobe(),
            kani::any(),
            any_refusal(),
        );
        if routing == UnobtainableRouting::CompleteForLiveInterest {
            assert!(covered && !missing_live && !refs_missing);
        }
    }

    /// Totality + cell reachability: the core never panics over the
    /// full bounded input space, and every one of the four routing
    /// verdicts is reachable (a dead arm means an input axis collapsed
    /// — the 178-class catch-all regression shape). Redrawn over the
    /// full `Refusal` alphabet at the bug_084 close.
    #[kani::proof]
    fn check_route_total_and_cells_reachable() {
        let routing = route_from_classes(
            kani::any(),
            kani::any(),
            kani::any(),
            any_evidence(),
            kani::any(),
            any_reprobe(),
            kani::any(),
            any_refusal(),
        );
        kani::cover!(routing == UnobtainableRouting::CompleteForLiveInterest);
        kani::cover!(routing == UnobtainableRouting::ReArm);
        kani::cover!(routing == UnobtainableRouting::ResolveFromSource);
        kani::cover!(routing == UnobtainableRouting::FailFast);
    }

    /// merged_bug_263 (bughunt-4): the trust-refusal settlement law —
    /// retargeted to `Refusal::Trust` at the bug_084 alphabet close
    /// (count-neutral; the per-variant body documents the law).
    #[kani::proof]
    fn check_trust_refused_settles_from_source() {
        refused_settles_from_source_body(Refusal::Trust);
    }

    /// bug_084: the content-refusal settlement law — the axis whose
    /// omission was the finding (a content mismatch routed as a clean
    /// miss: the doomed ReArm, then the FailFast resubmit loop).
    #[kani::proof]
    fn check_refusal_content_settles_from_source() {
        refused_settles_from_source_body(Refusal::Content);
    }

    /// bug_084: the both-axes settlement law.
    #[kani::proof]
    fn check_refusal_trust_and_content_settles_from_source() {
        refused_settles_from_source_body(Refusal::TrustAndContent);
    }

    /// bug_084: the FUTURE-axis settlement law — an unknown wire value
    /// decodes `Unrecognized` and settles from-source like every
    /// refusal, so the next alphabet evolution cannot route through
    /// the clean lane (the conservative-decode half of the close).
    #[kani::proof]
    fn check_refusal_unrecognized_settles_from_source() {
        refused_settles_from_source_body(Refusal::Unrecognized);
    }

    /// bug_084, the architectural close stated as a proof: FailFast is
    /// reachable ONLY under `Refusal::None` (over the WHOLE domain —
    /// fail-fast lives in arm 3 alone), and an arm-3 ReArm (anything
    /// missing) likewise — the one-shot can only ever be spent on an
    /// unrefused re-attempt. Arm-0 ReArm (nothing missing, uncovered)
    /// stays refusal-independent by design and is excluded by the
    /// missing/refs guard.
    #[kani::proof]
    fn check_only_unrefused_settlements_rearm_or_failfast() {
        let missing_live: bool = kani::any();
        let refs_missing: bool = kani::any();
        let refusal = any_refusal();
        let routing = route_from_classes(
            missing_live,
            refs_missing,
            kani::any(),
            any_evidence(),
            kani::any(),
            any_reprobe(),
            kani::any(),
            refusal,
        );
        if routing == UnobtainableRouting::FailFast {
            assert!(refusal == Refusal::None);
        }
        if (missing_live || refs_missing) && routing == UnobtainableRouting::ReArm {
            assert!(refusal == Refusal::None);
        }
        kani::cover!(routing == UnobtainableRouting::FailFast);
        kani::cover!((missing_live || refs_missing) && routing == UnobtainableRouting::ReArm);
    }

    /// Finding 11 / the B2 walk equivalence, generalized: a NON-PRUNED
    /// job never fail-fasts, whatever its evidence, coverage, refusal,
    /// or re-probe answer — the childless leaf (broken by structure)
    /// is the named cover.
    #[kani::proof]
    fn check_childless_leaf_non_pruned_never_failfast() {
        let evidence = any_evidence();
        let routing = route_from_classes(
            kani::any(),
            kani::any(),
            kani::any(),
            evidence,
            kani::any(),
            any_reprobe(),
            false,
            any_refusal(),
        );
        assert!(routing != UnobtainableRouting::FailFast);
        kani::cover!(
            evidence == ClosureEvidence::ChildlessLeaf
                && routing == UnobtainableRouting::ResolveFromSource
        );
    }
}
