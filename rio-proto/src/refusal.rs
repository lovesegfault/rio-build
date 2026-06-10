//! The ONE refusal-adjudication authority (merged_bug_013): what an
//! ANSWERED gRPC refusal disproves is a function of `(credential
//! regime, status code)` — mintable once, never per consumer.
//!
//! Round-8 forensics: "what does an auth refusal disprove" was decided
//! three ways across two crates — the store's pull-lane adjudication
//! typed `PermissionDenied | Unauthenticated` as presentation-judging
//! (client.rs `pull_once`, merged_bug_074), while the same store's
//! report leg (`is_fatal_rejection`) and the controller's exposure
//! classifier (`classify_append_status`) both treated the identical
//! codes as proof of futility and permanently gave up on the first
//! observation. Under the per-request service-token regime that ruling
//! is FALSE: every request carries a freshly minted HMAC token from the
//! rotating `rio-service-hmac` Secret, so one refusal judges one
//! presentation under one key observation — kubelet Secret propagation
//! lag means the next presentation may verify. This module is the
//! single source the consumers re-point at; divergent per-consumer
//! fatal sets are the defect class it retires (the `status.rs`
//! precedent: exhaustive cross-crate semantics, no wildcard arms).
//!
//! Per-arm rationale table (the [`judge_refusal`] law):
//!
//! | code | `PerRequestService` | `AttemptBound` | why |
//! |---|---|---|---|
//! | `InvalidArgument`, `Unimplemented` | `DisprovesRequest` | `DisprovesRequest` | the server rejected the request's CONTENT or lacks the RPC; the same bytes redeliver identically under any credential |
//! | `Unauthenticated`, `PermissionDenied` | `JudgesPresentation` | `DisprovesRequest` | per-request: a fresh mint (possibly under a re-read key) rides the next attempt — the refusal proves nothing about a future presentation; attempt-bound: re-presentation is byte-identical, so the refusal is stable for the attempt |
//! | everything else | `Undecided` | `Undecided` | transport/load/leadership shapes; the caller's own retry and pacing envelope governs |

use tonic::Code;

/// Which credential discipline signed the refused request — the axis
/// that decides whether an auth refusal can prove anything beyond the
/// one presentation it judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialRegime {
    /// The fleet service-token regime: a fresh HMAC `ServiceClaims`
    /// token is minted PER REQUEST (60 s expiry) from the mounted,
    /// rotating `rio-service-hmac` Secret
    /// (`rio_auth::hmac::ServiceTokenInterceptor`). Rotation skew —
    /// kubelet Secret propagation lag, per-pod observation lag — makes
    /// any single auth refusal evidence about ONE presentation under
    /// ONE key observation, never about the next mint.
    PerRequestService,
    /// The scheduler-minted executor/assignment token regime: the
    /// credential is fixed for the pod's lifetime (the builder's
    /// `executor_token`), so re-presentation is byte-identical and an
    /// auth refusal is stable for the whole attempt.
    AttemptBound,
}

/// What an ANSWERED refusal proves — the closed adjudication alphabet
/// every client-side consumer folds, instead of minting its own fatal
/// set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalJudgment {
    /// The refusal disproves the REQUEST: re-sending the same bytes
    /// cannot succeed under any credential observation. Permanent-exit
    /// lanes may act on a single observation.
    DisprovesRequest,
    /// The refusal judges the credential PRESENTATION only: it proves
    /// nothing about a future presentation (fresh mint, possibly
    /// re-read key). A permanent exit MUST ride a typed, violable
    /// observation budget — never one observation.
    JudgesPresentation,
    /// The refusal decides neither: transport, load, or leadership
    /// shapes the caller's own retry/pacing envelope governs.
    Undecided,
}

// r[impl sec.authz.refusal-adjudication]
/// THE adjudication law: total over all 17 [`tonic::Code`] variants ×
/// both [`CredentialRegime`]s, no wildcard arm — a tonic code addition
/// is a compile error here, so the census is compiler-derived (the
/// `classify_append_status` precedent). Consumers MAY extend a
/// `DisprovesRequest`/`Undecided` ruling with call-specific knowledge
/// (e.g. an RPC whose validation gate also emits `OutOfRange`), but
/// only on codes this law leaves `Undecided` — never by contradicting
/// a `JudgesPresentation` ruling.
pub const fn judge_refusal(regime: CredentialRegime, code: Code) -> RefusalJudgment {
    match code {
        // The request shape can never succeed: validation rejected the
        // CONTENT, or the server lacks the RPC. Credential-independent.
        Code::InvalidArgument | Code::Unimplemented => RefusalJudgment::DisprovesRequest,
        // The auth pair: regime decides. Per-request credentials are
        // re-minted every send (rotation skew: the serving key set and
        // the client's mounted key may converge on the NEXT
        // observation); attempt-bound credentials re-present the same
        // bytes, so the refusal is stable for the attempt.
        Code::Unauthenticated | Code::PermissionDenied => match regime {
            CredentialRegime::PerRequestService => RefusalJudgment::JudgesPresentation,
            CredentialRegime::AttemptBound => RefusalJudgment::DisprovesRequest,
        },
        // Transport, load, leadership, server-fault, or
        // not-this-law's-business shapes: the caller's own
        // retry/pacing envelope governs.
        Code::Ok
        | Code::Cancelled
        | Code::Unknown
        | Code::DeadlineExceeded
        | Code::NotFound
        | Code::AlreadyExists
        | Code::ResourceExhausted
        | Code::FailedPrecondition
        | Code::Aborted
        | Code::OutOfRange
        | Code::Internal
        | Code::Unavailable
        | Code::DataLoss => RefusalJudgment::Undecided,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // r[verify sec.authz.refusal-adjudication]
    /// R-1A: pins all 17 codes × 2 regimes — every cell, table-driven.
    /// Certifies: the authority's totality and the regime split itself
    /// (the auth pair judges the presentation under the per-request
    /// regime and disproves the request under the attempt-bound one;
    /// the content pair disproves under both; everything else is
    /// Undecided under both). The expectation table is hand-written
    /// from the spec sentence (`sec.authz.refusal-adjudication`),
    /// never computed from the implementation — the T3 independent
    /// oracle. New surface; disclosed no-red (the law did not exist at
    /// the pre-fix tree; its consumers' reds are R-1B/R-1E).
    #[test]
    fn judge_refusal_total_over_codes_and_regimes() {
        use CredentialRegime::{AttemptBound, PerRequestService};
        use RefusalJudgment::{DisprovesRequest, JudgesPresentation, Undecided};
        // (code, per-request expectation, attempt-bound expectation) —
        // all 17 variants, transcribed by hand from the rule text.
        let law: [(Code, RefusalJudgment, RefusalJudgment); 17] = [
            (Code::Ok, Undecided, Undecided),
            (Code::Cancelled, Undecided, Undecided),
            (Code::Unknown, Undecided, Undecided),
            (Code::InvalidArgument, DisprovesRequest, DisprovesRequest),
            (Code::DeadlineExceeded, Undecided, Undecided),
            (Code::NotFound, Undecided, Undecided),
            (Code::AlreadyExists, Undecided, Undecided),
            (Code::PermissionDenied, JudgesPresentation, DisprovesRequest),
            (Code::ResourceExhausted, Undecided, Undecided),
            (Code::FailedPrecondition, Undecided, Undecided),
            (Code::Aborted, Undecided, Undecided),
            (Code::OutOfRange, Undecided, Undecided),
            (Code::Unimplemented, DisprovesRequest, DisprovesRequest),
            (Code::Internal, Undecided, Undecided),
            (Code::Unavailable, Undecided, Undecided),
            (Code::DataLoss, Undecided, Undecided),
            (Code::Unauthenticated, JudgesPresentation, DisprovesRequest),
        ];
        for (code, per_request, attempt_bound) in law {
            assert_eq!(
                judge_refusal(PerRequestService, code),
                per_request,
                "PerRequestService × {code:?}"
            );
            assert_eq!(
                judge_refusal(AttemptBound, code),
                attempt_bound,
                "AttemptBound × {code:?}"
            );
        }
    }
}
