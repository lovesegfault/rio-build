//! Substitute-failure classification — the dependency-free truth table
//! behind the store executor's walk error arm (bughunt wave A4,
//! merged_bug_178).
//!
//! The walk's pre-fix arm was a blanket `Err(e) => infra_failure(...)`:
//! every failure class — including the two the substituter itself
//! documents as transient-and-retryable (`Raced`: another uploader
//! holds the placeholder slot; `RateLimited`: upstream 429 with a
//! parsed `Retry-After`) — became a job-fatal `InfraFailure` that
//! charged the materialization budget. Three 429 waves parked a
//! healthy job.
//!
//! This module is the single decision surface: rio-store maps its
//! `SubstituteError` variants onto [`SubstituteFailureClass`]
//! EXHAUSTIVELY (no catch-all — a future error variant fails the build
//! at BOTH hops), and [`classify_substitute_failure`] is total over the
//! class alphabet. The kani harness sweeps the entire table; the
//! enumeration unit test pins each row.
//!
//! Scope narrowing (recorded in the substitution-replacement invariant
//! map): `Stalled` and `AdmissionSaturated` STAY `ChargeInfra` by
//! design — the stall-abort contract (`store.substitute.stall-abort`)
//! deliberately reports infrastructure failure so the strike ladder
//! and the park budget see stalls, and a saturated per-replica
//! admission gate is capacity evidence, not a politeness signal.

/// The walk-relevant failure classes of one substitution attempt.
/// rio-store maps `SubstituteError` → this enum exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubstituteFailureClass {
    /// Another uploader holds the placeholder slot (cross-replica
    /// race). The upload in flight will land; retrying later reaches
    /// `AlreadyComplete`.
    Raced,
    /// Upstream 429 (rate-limited), with the parsed `Retry-After`
    /// available to the caller.
    RateLimited,
    /// Owner-side download-stall abort (no body bytes for the stall
    /// window). Charges by design — see the module doc.
    Stalled,
    /// The per-replica admission gate timed out or closed. Charges by
    /// design — capacity evidence.
    AdmissionSaturated,
    /// Upstream fetch trouble (connect/TLS/5xx) or upstream-served
    /// garbage that failed parse.
    Fetch,
    /// Integrity violation: NAR hash/size mismatch, oversize caps.
    Integrity,
    /// Local ingest failure (metadata write-ahead, chunk upload).
    Ingest,
}

/// What the scheduler-facing outcome of a classified failure is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureDisposition {
    /// Close the attempt charge-free and retry the job after a short
    /// deferral — the failure is transient by the substituter's own
    /// contract and says nothing about the upstream's content or the
    /// store's health.
    RetryUncharged,
    /// Report `InfraFailure`: the charge ladder (and therefore the
    /// park budget) must see this.
    ChargeInfra,
}

/// The total classification table (no catch-all: adding a
/// [`SubstituteFailureClass`] variant breaks this build).
// r[impl store.materialize.executor+5]
pub fn classify_substitute_failure(class: SubstituteFailureClass) -> FailureDisposition {
    match class {
        SubstituteFailureClass::Raced | SubstituteFailureClass::RateLimited => {
            FailureDisposition::RetryUncharged
        }
        SubstituteFailureClass::Stalled
        | SubstituteFailureClass::AdmissionSaturated
        | SubstituteFailureClass::Fetch
        | SubstituteFailureClass::Integrity
        | SubstituteFailureClass::Ingest => FailureDisposition::ChargeInfra,
    }
}

/// The per-tenant settlement re-probe fold (bughunt wave A4,
/// merged_bug_028 / owner decision Q2 2026-06-03): the consumption
/// arm-3 re-probe asks every LIVE tenant's upstream view, and
/// `ConfirmedMissing` — the verdict that can fail-fast a pruned root
/// or settle a leaf from-source — requires EVERY tenant to confirm.
/// One obtainable answer (the path is present, substitutable, or even
/// indeterminate under that tenant) keeps the job armed: the job
/// fails only when NO interested tenant can obtain.
///
/// An EMPTY answer set folds to `Obtainable` (nothing was confirmed;
/// the caller's no-live-tenant shape has its own conservative arms).
/// RPC failures never reach this fold — the caller maps them to its
/// B3 ReArm before folding (same posture as `route_unobtainable`'s
/// `None` reprobe).
// r[impl sched.materialize.routing+5]
pub fn fold_tenant_reprobes(
    answers: &[crate::routing::ReprobeAnswer],
) -> crate::routing::ReprobeAnswer {
    use crate::routing::ReprobeAnswer;
    if !answers.is_empty()
        && answers
            .iter()
            .all(|a| matches!(a, ReprobeAnswer::ConfirmedMissing))
    {
        ReprobeAnswer::ConfirmedMissing
    } else {
        ReprobeAnswer::Obtainable
    }
}

/// The post-loop verdict of one `do_substitute` upstream iteration that
/// produced no hit (bughunt wave A4, bug_081).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubstituteLoopVerdict {
    /// Every upstream gave a definitive miss — cacheable.
    CleanMiss,
    /// ≥1 upstream stalled (and none served). The stall dominates a
    /// concurrent 429: its strike is already durably recorded and the
    /// executor's classification charges it as infrastructure
    /// evidence — surfacing `RateLimited` instead would hide the
    /// recorded strike behind back-off advice.
    Stalled {
        /// The widest stall window observed across upstreams.
        window: core::time::Duration,
    },
    /// ≥1 upstream 429'd (and none served, none stalled). Uncached so
    /// a retry can re-ask the rate-limited upstream.
    RateLimited {
        /// Max parsed `Retry-After` across the 429ing upstreams.
        retry_after: Option<core::time::Duration>,
    },
}

/// bug_081's pure post-loop fold: a stall on ONE upstream is an
/// upstream-local failure — the loop records it and fails over
/// (mirroring the 429 arm); only after every upstream has been tried
/// does the recorded evidence pick the attempt outcome. Total over
/// both observation axes; precedence `Stalled > RateLimited >
/// CleanMiss` (charging evidence outranks back-off advice outranks a
/// cacheable miss).
// r[impl store.substitute.stall-abort+2]
pub fn fold_substitute_loop(
    any_stall: Option<core::time::Duration>,
    any_429: Option<Option<core::time::Duration>>,
) -> SubstituteLoopVerdict {
    match (any_stall, any_429) {
        (Some(window), _) => SubstituteLoopVerdict::Stalled { window },
        (None, Some(retry_after)) => SubstituteLoopVerdict::RateLimited { retry_after },
        (None, None) => SubstituteLoopVerdict::CleanMiss,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::ReprobeAnswer;

    /// bug_081: the post-loop fold's precedence, all four cells.
    #[test]
    fn substitute_loop_fold_precedence() {
        use core::time::Duration;
        assert_eq!(
            fold_substitute_loop(None, None),
            SubstituteLoopVerdict::CleanMiss
        );
        assert_eq!(
            fold_substitute_loop(None, Some(Some(Duration::from_secs(7)))),
            SubstituteLoopVerdict::RateLimited {
                retry_after: Some(Duration::from_secs(7))
            }
        );
        assert_eq!(
            fold_substitute_loop(Some(Duration::from_secs(180)), None),
            SubstituteLoopVerdict::Stalled {
                window: Duration::from_secs(180)
            }
        );
        // Both observed → the stall dominates.
        assert_eq!(
            fold_substitute_loop(Some(Duration::from_secs(180)), Some(None)),
            SubstituteLoopVerdict::Stalled {
                window: Duration::from_secs(180)
            }
        );
    }

    /// The full table, row by row (merged_bug_178's enumeration pin).
    #[test]
    fn classification_table() {
        use FailureDisposition::*;
        use SubstituteFailureClass::*;
        for (class, want) in [
            (Raced, RetryUncharged),
            (RateLimited, RetryUncharged),
            (Stalled, ChargeInfra),
            (AdmissionSaturated, ChargeInfra),
            (Fetch, ChargeInfra),
            (Integrity, ChargeInfra),
            (Ingest, ChargeInfra),
        ] {
            assert_eq!(classify_substitute_failure(class), want, "{class:?}");
        }
    }

    /// merged_bug_028: ConfirmedMissing is the ALL-tenant conjunction;
    /// empty never confirms.
    #[test]
    fn reprobe_fold_is_all_tenant_conjunction() {
        use ReprobeAnswer::*;
        assert_eq!(fold_tenant_reprobes(&[]), Obtainable);
        assert_eq!(fold_tenant_reprobes(&[Obtainable]), Obtainable);
        assert_eq!(fold_tenant_reprobes(&[ConfirmedMissing]), ConfirmedMissing);
        assert_eq!(
            fold_tenant_reprobes(&[ConfirmedMissing, Obtainable]),
            Obtainable
        );
        assert_eq!(
            fold_tenant_reprobes(&[ConfirmedMissing, ConfirmedMissing]),
            ConfirmedMissing
        );
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;
    use crate::routing::ReprobeAnswer;

    /// merged_bug_178: the classification table, swept — transient by
    /// the substituter's own contract (Raced/RateLimited) closes
    /// uncharged; everything else (including the two deliberate
    /// scope-narrowings, Stalled and AdmissionSaturated) charges. Total
    /// over the class alphabet by construction (no catch-all).
    #[kani::proof]
    fn check_substitute_failure_truth_table() {
        let class = match kani::any::<u8>() {
            0 => SubstituteFailureClass::Raced,
            1 => SubstituteFailureClass::RateLimited,
            2 => SubstituteFailureClass::Stalled,
            3 => SubstituteFailureClass::AdmissionSaturated,
            4 => SubstituteFailureClass::Fetch,
            5 => SubstituteFailureClass::Integrity,
            _ => SubstituteFailureClass::Ingest,
        };
        let disposition = classify_substitute_failure(class);
        let transient = matches!(
            class,
            SubstituteFailureClass::Raced | SubstituteFailureClass::RateLimited
        );
        assert_eq!(disposition == FailureDisposition::RetryUncharged, transient);
        // bug_081's loop fold rides the same table: Stalled dominates
        // RateLimited dominates CleanMiss, total over both axes.
        let any_stall = if kani::any() {
            Some(core::time::Duration::from_secs(kani::any::<u32>() as u64))
        } else {
            None
        };
        let any_429 = if kani::any() {
            Some(if kani::any() {
                Some(core::time::Duration::from_secs(kani::any::<u32>() as u64))
            } else {
                None
            })
        } else {
            None
        };
        let verdict = fold_substitute_loop(any_stall, any_429);
        match (any_stall, any_429) {
            (Some(w), _) => assert_eq!(verdict, SubstituteLoopVerdict::Stalled { window: w }),
            (None, Some(ra)) => {
                assert_eq!(
                    verdict,
                    SubstituteLoopVerdict::RateLimited { retry_after: ra }
                )
            }
            (None, None) => assert_eq!(verdict, SubstituteLoopVerdict::CleanMiss),
        }
    }

    /// merged_bug_028 / owner Q2: `ConfirmedMissing` is the ALL-tenant
    /// conjunction over a NON-EMPTY answer set — one obtainable tenant
    /// view (or an empty set) keeps the job armed. Swept over every
    /// answer vector up to four tenants (bitmask representation; stack
    /// array — no heap under CBMC).
    #[kani::proof]
    fn check_confirmed_missing_is_all_tenant_conjunction() {
        let mask: u8 = kani::any();
        let mk = |i: usize| {
            if mask & (1 << i) != 0 {
                ReprobeAnswer::ConfirmedMissing
            } else {
                ReprobeAnswer::Obtainable
            }
        };
        let arr = [mk(0), mk(1), mk(2), mk(3)];
        // One concrete-length call per width (0..=4): symbolic slice
        // lengths make CBMC's fat-pointer + iterator reasoning blow up
        // (observed: the symbolic-len form ran past 19 CPU-minutes;
        // this form completes in seconds). The mask still sweeps every
        // answer VECTOR at each width — 2^4 × 5 widths = the full
        // bounded space.
        let confirmed = |i: usize| mask & (1 << i) != 0;
        assert_eq!(
            fold_tenant_reprobes(&arr[..0]) == ReprobeAnswer::ConfirmedMissing,
            false
        );
        assert_eq!(
            fold_tenant_reprobes(&arr[..1]) == ReprobeAnswer::ConfirmedMissing,
            confirmed(0)
        );
        assert_eq!(
            fold_tenant_reprobes(&arr[..2]) == ReprobeAnswer::ConfirmedMissing,
            confirmed(0) && confirmed(1)
        );
        assert_eq!(
            fold_tenant_reprobes(&arr[..3]) == ReprobeAnswer::ConfirmedMissing,
            confirmed(0) && confirmed(1) && confirmed(2)
        );
        assert_eq!(
            fold_tenant_reprobes(&arr[..4]) == ReprobeAnswer::ConfirmedMissing,
            confirmed(0) && confirmed(1) && confirmed(2) && confirmed(3)
        );
    }
}
