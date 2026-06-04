//! Total establishment decision kernel (§4.R2: ONE function, one home).
//!
//! When the establishment sweep finds an open pull-mode attempt past
//! its deadline + slack, exactly one of five dispositions follows. The
//! decision used to live inline in the sweep arm as a sequence of
//! early-return blocks, which made the axes implicit: the node's
//! wantedness was never consulted (a cancelled build's attempt — or an
//! attempt whose node left the DAG entirely — was charged
//! `executor_crash`, polluting the exclusion ledger and the OA2
//! clustering inputs with verdicts about work nobody wants), and probe
//! failure was conflated with verified evidence. This kernel makes the
//! decision total over (kind × node × probe × verifiable-wanted): no
//! catch-all arm, so a new axis value fails compilation here and in
//! every caller.
//!
//! Axis ownership (wave ruling §4.R2): the NODE axis (cancelled/absent
//! ⇒ charge-free close) is wired by the terminal-capture workstream;
//! the kind/probe axes by evidence-classification. Whichever lands
//! first creates the fn with all five variants; the second extends THE
//! SAME fn. Both kani harnesses target this kernel.

use std::collections::HashSet;

use crate::pull::PullKind;

/// The swept node's standing in the live DAG at sweep time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeDisposition {
    /// The node exists and some build still wants it.
    WantedLive,
    /// The node exists but is `Cancelled` — every interested build
    /// cancelled (or the per-build timeout / fail-fast path did).
    Cancelled,
    /// The node is gone from the DAG (build cleaned up, interest
    /// removed, or never recovered after failover).
    Absent,
}

/// What the batch store probe produced for this sweep pass.
#[derive(Debug, Clone, Copy)]
pub enum ProbeEvidence<'a> {
    /// The probe ran: `missing` is the authoritative absent-set for the
    /// probed paths.
    Verified(&'a HashSet<String>),
    /// The probe was attempted and failed (RPC error / timeout): there
    /// is NO evidence about the store, in either direction.
    Unavailable,
    /// No store client is configured: output presence can never be
    /// verified on this deployment.
    NoStoreConfigured,
}

/// The five establishment dispositions. Total — every swept attempt
/// gets exactly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EstablishmentDisposition {
    /// Build outputs are verifiably all present: adopt the completion,
    /// close the attempt, charge nothing.
    AdoptCompleted,
    /// Charge one scheduler-party `executor_crash` attempt row, close
    /// the assignment, requeue/poison per the retry verdict.
    ChargeExecutorCrash,
    /// Materialization attempts charge `materialization_infra` (their
    /// own budget; never `executor_crash`, never an adopt — a mid-walk
    /// crash leaves outputs present but the closure incomplete).
    ChargeMaterializationInfra,
    /// Leave the attempt open this pass (no charge, no close): the
    /// evidence needed to decide is unavailable, not negative.
    Defer,
    /// Close the assignment row with NO attempt row, NO exclusion, NO
    /// establishment metric: nobody wants this work any more
    /// (cancelled or absent node). Charging would seed the exclusion
    /// ledger and the OA2 wedge clustering with verdicts about
    /// abandoned work.
    CloseChargeFree,
}

/// Decide one expired open pull-mode attempt. Total over all four
/// axes; no catch-all.
///
/// Decision table (wave allocation §1.6, binding):
/// - `node ∈ {Cancelled, Absent}` ⇒ [`CloseChargeFree`] — regardless
///   of kind or probe: a charge needs a wanting node.
/// - else `kind == Materialization` ⇒ [`ChargeMaterializationInfra`].
/// - else (`Build`):
///   - probe [`Unavailable`] ⇒ [`Defer`] (absence of evidence is not
///     evidence of absence — the attempt stays open for a pass with a
///     working probe),
///   - probe [`Verified`] with a non-empty verifiable wanted set whose
///     paths are ALL present (none missing) ⇒ [`AdoptCompleted`],
///   - probe [`Verified`] otherwise (some path missing, or nothing is
///     verifiable — `None`/empty saturates to cannot-verify, never to
///     vacuously-present) ⇒ [`ChargeExecutorCrash`],
///   - probe [`NoStoreConfigured`] ⇒ [`ChargeExecutorCrash`] (this
///     deployment can never adopt; the unreported pod is the only
///     explanation left).
///
/// [`CloseChargeFree`]: EstablishmentDisposition::CloseChargeFree
/// [`ChargeMaterializationInfra`]: EstablishmentDisposition::ChargeMaterializationInfra
/// [`Defer`]: EstablishmentDisposition::Defer
/// [`Verified`]: ProbeEvidence::Verified
/// [`Unavailable`]: ProbeEvidence::Unavailable
/// [`NoStoreConfigured`]: ProbeEvidence::NoStoreConfigured
/// [`AdoptCompleted`]: EstablishmentDisposition::AdoptCompleted
/// [`ChargeExecutorCrash`]: EstablishmentDisposition::ChargeExecutorCrash
// r[impl sched.attempt.cancel-close-driven]
pub fn establish_expired_attempt(
    kind: PullKind,
    node: NodeDisposition,
    probe: ProbeEvidence<'_>,
    verifiable_wanted: Option<&[&str]>,
) -> EstablishmentDisposition {
    establish_from_classes(kind, node, classify_probe(probe, verifiable_wanted))
}

/// The probe axis collapsed to its decision-relevant classes — the
/// set/slice inputs of [`establish_expired_attempt`] reduce to exactly
/// these five cells, and the CBMC proof sweeps THIS alphabet (no heap
/// collections under kani — the `nix/kani.nix` bounded-representation
/// discipline; `HashSet` construction reaches `getrandom`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeClass {
    /// Probe ran; a non-empty verifiable wanted set is ALL present.
    VerifiedAllWantedPresent,
    /// Probe ran; some verifiable wanted path is missing.
    VerifiedMissingWanted,
    /// Probe ran but nothing is verifiable (`None`/empty saturates to
    /// cannot-verify, never to vacuously-present — the
    /// `verifiable_wanted_paths` None-on-empty contract).
    VerifiedNothingVerifiable,
    /// Probe attempted and failed: no evidence in either direction.
    Unavailable,
    /// No store client configured: presence can never be verified.
    NoStoreConfigured,
}

/// Total projection from the caller-shaped probe inputs to the
/// decision alphabet. Pure; pinned by the unit decision table over
/// real sets.
pub fn classify_probe(probe: ProbeEvidence<'_>, verifiable_wanted: Option<&[&str]>) -> ProbeClass {
    match probe {
        ProbeEvidence::Unavailable => ProbeClass::Unavailable,
        ProbeEvidence::NoStoreConfigured => ProbeClass::NoStoreConfigured,
        ProbeEvidence::Verified(missing) => match verifiable_wanted {
            Some(wanted) if !wanted.is_empty() => {
                if wanted.iter().all(|p| !missing.contains(*p)) {
                    ProbeClass::VerifiedAllWantedPresent
                } else {
                    ProbeClass::VerifiedMissingWanted
                }
            }
            _ => ProbeClass::VerifiedNothingVerifiable,
        },
    }
}

/// The set-free decision core: total over
/// (kind × node × probe-class), no catch-all. This is the function the
/// kani harness sweeps exhaustively.
pub fn establish_from_classes(
    kind: PullKind,
    node: NodeDisposition,
    probe: ProbeClass,
) -> EstablishmentDisposition {
    match (node, kind) {
        (NodeDisposition::Cancelled | NodeDisposition::Absent, _) => {
            EstablishmentDisposition::CloseChargeFree
        }
        (NodeDisposition::WantedLive, PullKind::Materialization) => {
            EstablishmentDisposition::ChargeMaterializationInfra
        }
        (NodeDisposition::WantedLive, PullKind::Build) => match probe {
            ProbeClass::Unavailable => EstablishmentDisposition::Defer,
            ProbeClass::NoStoreConfigured => EstablishmentDisposition::ChargeExecutorCrash,
            ProbeClass::VerifiedAllWantedPresent => EstablishmentDisposition::AdoptCompleted,
            ProbeClass::VerifiedMissingWanted | ProbeClass::VerifiedNothingVerifiable => {
                EstablishmentDisposition::ChargeExecutorCrash
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn missing(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    /// The full §1.6 decision table, row by row.
    #[test]
    fn decision_table() {
        use EstablishmentDisposition::*;
        use NodeDisposition::*;
        use ProbeEvidence::*;
        let none: HashSet<String> = HashSet::new();
        let one = missing(&["/nix/store/a"]);

        // Cancelled/Absent dominate every other axis.
        for kind in [PullKind::Build, PullKind::Materialization] {
            for node in [Cancelled, Absent] {
                for probe in [Verified(&one), Unavailable, NoStoreConfigured] {
                    for wanted in [None, Some(&["/nix/store/a"][..])] {
                        assert_eq!(
                            establish_expired_attempt(kind, node, probe, wanted),
                            CloseChargeFree,
                            "{kind:?}/{node:?} must close charge-free"
                        );
                    }
                }
            }
        }

        // Live materialization: its own charge class.
        assert_eq!(
            establish_expired_attempt(PullKind::Materialization, WantedLive, Unavailable, None),
            ChargeMaterializationInfra
        );

        // Live build × probe shapes.
        assert_eq!(
            establish_expired_attempt(PullKind::Build, WantedLive, Unavailable, None),
            Defer
        );
        assert_eq!(
            establish_expired_attempt(PullKind::Build, WantedLive, NoStoreConfigured, None),
            ChargeExecutorCrash
        );
        // All verifiable wanted present → adopt.
        assert_eq!(
            establish_expired_attempt(
                PullKind::Build,
                WantedLive,
                Verified(&none),
                Some(&["/nix/store/a"])
            ),
            AdoptCompleted
        );
        // A wanted path is missing → charge.
        assert_eq!(
            establish_expired_attempt(
                PullKind::Build,
                WantedLive,
                Verified(&one),
                Some(&["/nix/store/a"])
            ),
            ChargeExecutorCrash
        );
        // Nothing verifiable (None or empty) saturates to cannot-verify.
        assert_eq!(
            establish_expired_attempt(PullKind::Build, WantedLive, Verified(&none), None),
            ChargeExecutorCrash
        );
        assert_eq!(
            establish_expired_attempt(PullKind::Build, WantedLive, Verified(&none), Some(&[])),
            ChargeExecutorCrash
        );
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Cancelled/absent nodes are NEVER charged (and never adopted):
    /// for every kind and EVERY probe class, a `Cancelled` or `Absent`
    /// node yields exactly `CloseChargeFree`. The sweep targets the
    /// set-free decision core (`establish_from_classes`) — the public
    /// wrapper only collapses its set/slice inputs to `ProbeClass`
    /// (a pure projection pinned by the unit decision table), and heap
    /// collections under CBMC reach `getrandom` (the kani.nix
    /// bounded-representation discipline).
    #[kani::proof]
    fn check_cancelled_never_charged() {
        let kind = if kani::any() {
            PullKind::Build
        } else {
            PullKind::Materialization
        };
        let node = if kani::any() {
            NodeDisposition::Cancelled
        } else {
            NodeDisposition::Absent
        };
        let probe = match kani::any::<u8>() {
            0 => ProbeClass::VerifiedAllWantedPresent,
            1 => ProbeClass::VerifiedMissingWanted,
            2 => ProbeClass::VerifiedNothingVerifiable,
            3 => ProbeClass::Unavailable,
            _ => ProbeClass::NoStoreConfigured,
        };
        assert_eq!(
            establish_from_classes(kind, node, probe),
            EstablishmentDisposition::CloseChargeFree
        );
    }
}

#[cfg(kani)]
mod establishment_window_proofs {
    use super::*;

    fn any_kind() -> PullKind {
        if kani::any() {
            PullKind::Build
        } else {
            PullKind::Materialization
        }
    }

    fn any_node() -> NodeDisposition {
        match kani::any::<u8>() {
            0 => NodeDisposition::WantedLive,
            1 => NodeDisposition::Cancelled,
            _ => NodeDisposition::Absent,
        }
    }

    fn any_probe() -> ProbeClass {
        match kani::any::<u8>() {
            0 => ProbeClass::VerifiedAllWantedPresent,
            1 => ProbeClass::VerifiedMissingWanted,
            2 => ProbeClass::VerifiedNothingVerifiable,
            3 => ProbeClass::Unavailable,
            _ => ProbeClass::NoStoreConfigured,
        }
    }

    /// A4 step 3 (the C1 fix's kernel pin): a live build attempt whose
    /// store probe came back UNAVAILABLE defers — never a charge,
    /// never an adoption (the probe said nothing about the store's
    /// content; B3: unknown never demotes).
    #[kani::proof]
    fn check_establishment_unavailable_defers() {
        assert_eq!(
            establish_from_classes(
                PullKind::Build,
                NodeDisposition::WantedLive,
                ProbeClass::Unavailable
            ),
            EstablishmentDisposition::Defer
        );
    }

    /// The materialization row of the establishment table, swept over
    /// EVERY node disposition and probe class: a materialization
    /// attempt's expiry NEVER adopts a completion and NEVER charges
    /// the executor-crash (build) ledger — its only verdicts are the
    /// materialization-infra charge (live) and the charge-free close
    /// (cancelled/absent).
    #[kani::proof]
    fn check_establishment_materialization_never_adopts_or_crash_charges() {
        let node = any_node();
        let probe = any_probe();
        let disposition = establish_from_classes(PullKind::Materialization, node, probe);
        assert!(disposition != EstablishmentDisposition::AdoptCompleted);
        assert!(disposition != EstablishmentDisposition::ChargeExecutorCrash);
        // Both reachable cells covered.
        kani::cover!(disposition == EstablishmentDisposition::ChargeMaterializationInfra);
        kani::cover!(disposition == EstablishmentDisposition::CloseChargeFree);
        // Totality note: any_kind/any_node sweeps keep the two helper
        // fns live under cfg(kani).
        let _ = any_kind();
    }
}
