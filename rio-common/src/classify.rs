//! Shared classification predicates (R7, bughunt-4): every
//! classification law has exactly ONE executable source consumed by
//! all surfaces, so sibling call sites cannot fork an alphabet
//! (bug_178's class: two lanes open-coding `Unavailable |
//! DeadlineExceeded` while the canonical transient alphabet documents
//! `Unknown` as mid-RPC peer death).
//!
//! Module shape is the frozen R7 convention (`rio-common/src/
//! classify.rs`); sibling slots extend it with their own predicates
//! (keep-both at rebase).

/// True if a gRPC status code is evidence of a TRANSPORT-UNREACHABLE
/// store on a store-RPC lane — the store-degraded lane alphabet
/// (`builder.outcome.store-degraded+3`).
///
/// - `Unavailable` — server explicitly down (pod restarting,
///   follower-reject, connection refused).
/// - `Unknown` — mid-RPC peer death: h2 connection reset, TLS close
///   mid-stream; what tonic surfaces when the peer goes away without
///   a gRPC-level status (the [`crate::grpc::is_transient`] alphabet
///   doc is the canonical description). A store pod dying mid-RPC is
///   transport unreachability, not a verdict.
/// - `DeadlineExceeded` — the peer hung past the caller's timeout.
///
/// DIVERGENCE from [`crate::grpc::is_transient`], argued: that
/// predicate answers "might a retry succeed?" — `DeadlineExceeded` is
/// deliberately NOT transient there (retrying the same timeout
/// compounds the wait) and `ResourceExhausted`/`Aborted` ARE (the
/// store said "retry"). THIS predicate answers "did the store look
/// unreachable?" — `DeadlineExceeded` IS unreachability evidence (the
/// peer never answered), while `ResourceExhausted`/`Aborted` are the
/// store ANSWERING (pool full / PG conflict): a reachable store under
/// load, not a degraded one. Per-input verdicts (`NotFound`,
/// `Internal`) are neither.
pub fn is_store_unreachable_code(code: tonic::Code) -> bool {
    matches!(
        code,
        tonic::Code::Unavailable | tonic::Code::Unknown | tonic::Code::DeadlineExceeded
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bug_178: the full tonic::Code alphabet, every member named —
    /// a new code cannot silently default into either answer without
    /// this table taking a position on it.
    #[test]
    fn store_unreachable_code_alphabet_total() {
        use tonic::Code::*;
        let cases = [
            (Ok, false),
            (Cancelled, false),
            (Unknown, true),
            (InvalidArgument, false),
            (DeadlineExceeded, true),
            (NotFound, false),
            (AlreadyExists, false),
            (PermissionDenied, false),
            (ResourceExhausted, false),
            (FailedPrecondition, false),
            (Aborted, false),
            (OutOfRange, false),
            (Unimplemented, false),
            (Internal, false),
            (Unavailable, true),
            (DataLoss, false),
            (Unauthenticated, false),
        ];
        for (code, want) in cases {
            assert_eq!(is_store_unreachable_code(code), want, "code={code:?}");
        }
    }
}

// ---------------------------------------------------------------
// bug_255 (S1a): the attempt-terminal label vocabulary. One mapping
// from the attempt-terminal alphabet to the Prometheus label strings
// BOTH planes emit (scheduler termination_reason row + series labels;
// controller OA1 report-interval histogram). The retired shape was two
// hand-mirrored matches disagreeing on EvictedDiskPressure
// (disk_pressure vs evicted_disk_pressure). [GEN-SET] consumer list +
// command: see the module doc of the introducing commit / the sweeps
// file docs/gen/sweeps/bughunt4-s3.md pattern.
// ---------------------------------------------------------------

/// Crate-neutral mirror of the wire `AttemptTerminalReason` alphabet
/// (`rio-proto` depends on `rio-common`, so the proto enum cannot
/// appear here; the exhaustive `From` impl lives next to the enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptTerminalKind {
    Unspecified,
    OomKilled,
    EvictedDiskPressure,
    EvictedOther,
    Completed,
    Error,
    DeadlineExceeded,
    Cancelled,
    Preempted,
    Reaped,
    NoEligibleSource,
}

/// THE `termination_reason`/`reason` label vocabulary. The scheduler's
/// persisted strings are canonical (durable rows + recorded HELP
/// outlive a metrics-only rename), so the controller side adopted
/// `evicted_disk_pressure` at the bug_255 close.
pub fn attempt_terminal_reason_label(kind: AttemptTerminalKind) -> &'static str {
    use AttemptTerminalKind as K;
    match kind {
        K::Unspecified => "unspecified",
        K::OomKilled => "oom_killed",
        K::EvictedDiskPressure => "evicted_disk_pressure",
        K::EvictedOther => "evicted_other",
        K::Completed => "pod_completed",
        K::Error => "pod_error",
        K::DeadlineExceeded => "deadline_exceeded",
        K::Cancelled => "cancelled",
        K::Preempted => "preempted",
        K::Reaped => "reaped",
        K::NoEligibleSource => "no_eligible_source",
    }
}

#[cfg(test)]
mod label_tests {
    use super::*;

    /// The canonical strings are load-bearing (persisted rows join on
    /// them); pin the full alphabet.
    #[test]
    fn label_alphabet_pinned() {
        let all = [
            (AttemptTerminalKind::Unspecified, "unspecified"),
            (AttemptTerminalKind::OomKilled, "oom_killed"),
            (
                AttemptTerminalKind::EvictedDiskPressure,
                "evicted_disk_pressure",
            ),
            (AttemptTerminalKind::EvictedOther, "evicted_other"),
            (AttemptTerminalKind::Completed, "pod_completed"),
            (AttemptTerminalKind::Error, "pod_error"),
            (AttemptTerminalKind::DeadlineExceeded, "deadline_exceeded"),
            (AttemptTerminalKind::Cancelled, "cancelled"),
            (AttemptTerminalKind::Preempted, "preempted"),
            (AttemptTerminalKind::Reaped, "reaped"),
            (AttemptTerminalKind::NoEligibleSource, "no_eligible_source"),
        ];
        for (kind, label) in all {
            assert_eq!(attempt_terminal_reason_label(kind), label);
        }
    }
}
