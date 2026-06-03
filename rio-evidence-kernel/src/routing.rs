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
}

// r[impl sched.materialize.routing+4]
/// The four-arm routing core. PURE (no IO, no clocks); the FMP
/// re-probe answer is an input.
///
/// The probe-failure case: the consumption HANDLER maps "the re-probe
/// RPC itself failed/timed out" to ReArm before calling this core (B3:
/// an indeterminate answer never fail-fasts) — the core's `None`
/// reprobe arm is therefore only reachable as one-shot-spent.
pub fn route_unobtainable(inputs: &RoutingInputs<'_>) -> UnobtainableRouting {
    let missing_live: bool = inputs
        .missing_paths
        .iter()
        .any(|p| inputs.live_wanted_paths.contains(p));
    // Arm 0 — moot-failure (the C3 arm). The witness type guarantees
    // the wanted set is non-empty, so `covered` cannot be vacuous.
    // merged_bug_193: arm 0 additionally requires NO confirmed
    // reference miss — a hole in the dependency closure is never moot,
    // even when every live-wanted root is itself covered (the covered
    // root's closure is exactly what the reference miss punctured).
    // With a reference miss the decision falls through to arms 1–3,
    // where Vouched/Pending route from-source and the leaf/holed arm
    // settles on the origin discriminator.
    if !missing_live && inputs.missing_references.is_empty() {
        let covered = inputs
            .live_wanted_paths
            .paths()
            .iter()
            .all(|w| inputs.verified_paths.contains(w));
        return if covered {
            UnobtainableRouting::CompleteForLiveInterest
        } else {
            UnobtainableRouting::ReArm
        };
    }
    // Arms 1/2 — durable Vouched / Pending: from-source.
    match inputs.durable_evidence {
        ClosureEvidence::Vouched | ClosureEvidence::Pending => {
            return UnobtainableRouting::ResolveFromSource;
        }
        ClosureEvidence::ChildlessLeaf | ClosureEvidence::Holed => {}
    }
    // Arm 3 — leaf/holed evidence + live-wanted missing: the re-probe
    // gate.
    match inputs.reprobe {
        Some(ReprobeAnswer::Obtainable) if inputs.prior_unobtainable_count == 0 => {
            UnobtainableRouting::ReArm
        }
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
        _ if inputs.pruned_origin => UnobtainableRouting::FailFast,
        _ => UnobtainableRouting::ResolveFromSource,
    }
}

/// Success-consumption coverage check (the CE-17 closer): the live
/// wanted set is covered by what the execution ingested or verified.
/// The witness type makes the check non-vacuous by construction.
// r[impl sched.materialize.routing+4]
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
            });
            assert_eq!(routing, UnobtainableRouting::ResolveFromSource);
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
        });
        assert_eq!(routing, UnobtainableRouting::CompleteForLiveInterest);
    }
}
