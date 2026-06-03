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
    match (node, kind) {
        (NodeDisposition::Cancelled | NodeDisposition::Absent, _) => {
            EstablishmentDisposition::CloseChargeFree
        }
        (NodeDisposition::WantedLive, PullKind::Materialization) => {
            EstablishmentDisposition::ChargeMaterializationInfra
        }
        (NodeDisposition::WantedLive, PullKind::Build) => match probe {
            ProbeEvidence::Unavailable => EstablishmentDisposition::Defer,
            ProbeEvidence::NoStoreConfigured => EstablishmentDisposition::ChargeExecutorCrash,
            ProbeEvidence::Verified(missing) => match verifiable_wanted {
                Some(wanted)
                    if !wanted.is_empty() && wanted.iter().all(|p| !missing.contains(*p)) =>
                {
                    EstablishmentDisposition::AdoptCompleted
                }
                _ => EstablishmentDisposition::ChargeExecutorCrash,
            },
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
    /// for every kind, every probe shape, and every verifiable-wanted
    /// shape, a `Cancelled` or `Absent` node yields exactly
    /// `CloseChargeFree`. Probe CONTENTS cannot matter — the row never
    /// inspects them — so the sweep enumerates the probe SHAPES with
    /// fixed small sets (the kani.nix bounded-representation
    /// discipline: no symbolic heap collections).
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
        let empty: HashSet<String> = HashSet::new();
        let probe = match kani::any::<u8>() {
            0 => ProbeEvidence::Verified(&empty),
            1 => ProbeEvidence::Unavailable,
            _ => ProbeEvidence::NoStoreConfigured,
        };
        let wanted_one = ["/nix/store/a"];
        let verifiable_wanted: Option<&[&str]> = match kani::any::<u8>() {
            0 => None,
            1 => Some(&[]),
            _ => Some(&wanted_one),
        };
        assert_eq!(
            establish_expired_attempt(kind, node, probe, verifiable_wanted),
            EstablishmentDisposition::CloseChargeFree
        );
    }
}
