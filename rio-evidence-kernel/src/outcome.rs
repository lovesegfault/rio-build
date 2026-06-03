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
// r[impl store.materialize.executor+4]
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
// r[impl sched.materialize.routing+4]
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::ReprobeAnswer;

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
