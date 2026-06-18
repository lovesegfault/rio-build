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
    /// Wire zero value — reason not stated.
    Unspecified,
    /// The cgroup OOM-killed the build.
    OomKilled,
    /// Evicted under NODE-CONDITION disk pressure (the ambient
    /// `DiskPressure` / hyphenated `ephemeral-storage` shapes). The
    /// pod-attributed shapes split to
    /// [`Self::EvictedEmptyDirSizeLimit`] at sh-039.
    EvictedDiskPressure,
    /// live060-f (A2): a POD-ATTRIBUTED emptyDir-sizeLimit eviction —
    /// kubelet's own per-pod statement that THIS build exceeded ITS
    /// declared disk (the "Usage of EmptyDir volume … exceeds the
    /// limit" / "ephemeral local storage" shapes), categorically
    /// unlike the ambient node-condition shapes above. I-199's
    /// ClassifyOnly ruling rests on AMBIGUITY ("a node-condition
    /// eviction says nothing about THIS build's disk use"); this
    /// sub-shape carries none, so splitting it is a refinement of the
    /// ruling's own rationale, never a reversal of its conclusion.
    ///
    /// PRODUCED at sh-039: the controller→scheduler carrier
    /// `AttemptTerminalReason` gained the additive value (the
    /// live060-f deferral's named revive trigger — "the next
    /// eviction-shaped sizing incident" — discharged), and the
    /// PROMOTE path (this letter feeding the disk floor through the
    /// corroboration chokepoint as a scheduler-verifiable witness) is
    /// live: `witnessed_disposition` maps it to `PromoteDiskFloor`
    /// while [`Self::EvictedDiskPressure`] stays classify-only.
    EvictedEmptyDirSizeLimit,
    /// Evicted for any non-disk reason.
    EvictedOther,
    /// Finished and reported a result.
    Completed,
    /// Failed with a build error.
    Error,
    /// The attempt deadline elapsed.
    DeadlineExceeded,
    /// Cancelled by its owner.
    Cancelled,
    /// Preempted (spot reclaim or node scale-down).
    Preempted,
    /// Reaped by the controller's excess/orphan sweeps.
    Reaped,
    /// No eligible substitution source remained.
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
        K::EvictedEmptyDirSizeLimit => "evicted_empty_dir_size_limit",
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

/// Human rendering of an [`attempt_terminal_reason_label`] string for
/// the gateway's `rio: retry <reason>` re-dispatch marker (sh-042).
///
/// Input is the snake_case label STRING (carried on the wire as
/// `StartedPredecessor.termination_reason`), not [`AttemptTerminalKind`]:
/// the proto field is a string to avoid a cross-file enum import for a
/// display-only payload. The fallback arm (raw label) makes the fn
/// total — a label rename therefore degrades silently to the raw
/// string instead of failing; the `human_alphabet_pinned` test pins
/// each `(label, human)` pair explicitly so a rename or dropped match
/// arm fails CI rather than degrading.
pub fn attempt_terminal_reason_human(label: &str) -> std::borrow::Cow<'static, str> {
    match label {
        "evicted_empty_dir_size_limit" => "evicted (emptyDir disk limit)".into(),
        "oom_killed" => "OOM-killed".into(),
        "preempted" => "preempted (spot reclaim)".into(),
        "evicted_disk_pressure" => "evicted (node disk pressure)".into(),
        "evicted_other" => "evicted".into(),
        "reaped" => "pod reaped".into(),
        "deadline_exceeded" => "deadline exceeded".into(),
        // The establishment-sweep no-witness backstop (sh-042 v3):
        // housekeeping.rs hardcodes `"unreported"`; the witnessed mark
        // feeds the floor bump only and never overwrites the row.
        "unreported" => "executor unreachable (unreported)".into(),
        // Fallback: an unknown label (or one whose human form IS the
        // raw label) renders as-is. Never empty unless the label is.
        other => other.to_string().into(),
    }
}

/// Which resource floor a closed attempt's reason promotes — the
/// inverse of `witnessed_disposition`'s promote arms
/// (`rio-scheduler/src/actor/floor.rs`): only `OomKilled →
/// PromoteMemFloor` and `EvictedEmptyDirSizeLimit → PromoteDiskFloor`
/// do; every other reason (including `DeadlineExceeded`, which is
/// `ClassifyOnly`) is `None`.
///
/// `None` ↔ proto `PREDECESSOR_FLOOR_AXIS_NONE` (no sizing suffix on
/// the `rio: retry` marker). The DEADLINE axis is intentionally absent
/// — see the `reserved 3` comment in `dag.proto`.
pub fn axis_for_reason_label(label: &str) -> Option<FloorAxis> {
    match label {
        "oom_killed" => Some(FloorAxis::Mem),
        "evicted_empty_dir_size_limit" => Some(FloorAxis::Disk),
        _ => None,
    }
}

/// The two floor axes the `rio: retry` marker's sizing suffix can
/// name. Crate-neutral mirror of `rio_proto::types::PredecessorFloorAxis`
/// minus `NONE` (which is [`Option::None`] here) — rio-common does not
/// depend on rio-proto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloorAxis {
    /// Memory (`SolvedIntent.mem_bytes`).
    Mem,
    /// Disk (`SolvedIntent.disk_bytes`).
    Disk,
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
            (
                AttemptTerminalKind::EvictedEmptyDirSizeLimit,
                "evicted_empty_dir_size_limit",
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

    /// sh-042: pin each `(label, expected_human)` pair explicitly.
    /// [`attempt_terminal_reason_human`] has a fallback arm (raw
    /// label) so a domain-coverage assertion would be vacuous — a
    /// label rename or a dropped match arm would still pass it. The
    /// pinned-pairs shape is the same as [`label_alphabet_pinned`]'s.
    /// Labels whose human form IS the raw label are stated explicitly
    /// (the fallback is intentional, not an omission).
    #[test]
    fn human_alphabet_pinned() {
        let pinned = [
            (
                "evicted_empty_dir_size_limit",
                "evicted (emptyDir disk limit)",
            ),
            ("oom_killed", "OOM-killed"),
            ("preempted", "preempted (spot reclaim)"),
            ("evicted_disk_pressure", "evicted (node disk pressure)"),
            ("evicted_other", "evicted"),
            ("reaped", "pod reaped"),
            ("deadline_exceeded", "deadline exceeded"),
            ("unreported", "executor unreachable (unreported)"),
            // Intentional raw fallthrough (no dedicated human form):
            ("cancelled", "cancelled"),
            ("pod_error", "pod_error"),
            ("pod_completed", "pod_completed"),
            ("unspecified", "unspecified"),
            ("no_eligible_source", "no_eligible_source"),
        ];
        for (label, human) in pinned {
            assert_eq!(
                attempt_terminal_reason_human(label),
                human,
                "human rendering of label {label:?}"
            );
        }
        // The axis inverse pins the two promote arms only.
        assert_eq!(axis_for_reason_label("oom_killed"), Some(FloorAxis::Mem));
        assert_eq!(
            axis_for_reason_label("evicted_empty_dir_size_limit"),
            Some(FloorAxis::Disk)
        );
        assert_eq!(axis_for_reason_label("deadline_exceeded"), None);
        assert_eq!(axis_for_reason_label("preempted"), None);
        assert_eq!(axis_for_reason_label(""), None);
    }
}

// ---------------------------------------------------------------
// merged_bug_052 (S7, bughunt-5): the gc phase-3 outcome contract.
// THE one executable home of the rio-store -> rio-cli exit-posture
// contract for `rio-cli gc`. The retired shape was the failure-frame
// prefixes hand-mirrored as free text on both sides of the wire
// (producer format! in rio-store/src/gc/mod.rs, matcher literals in
// rio-cli/src/gc.rs) tied only by a comment: a store-side reword kept
// both test suites green while a failed destructive collect cycle
// exited 0. rio-cli does not depend on rio-store, so this frozen R7
// module — a dependency of both — is the contract's only shared home.
// [GEN-SET] prefix-literal census: docs/gen/sweeps/bughunt5-s7.md.
// ---------------------------------------------------------------

/// Crate-neutral closure set of the gc phase-3 (chunk-collect cycle)
/// outcome alphabet rendered into the terminal `GcProgress.current_path`
/// frame (the store's `Phase3Render` mirror; the exhaustive `From` impl
/// lives next to that enum, the [`AttemptTerminalKind`] precedent). A
/// new variant cannot compile without taking an exit-posture position
/// in [`Self::failure_prefix`] and joining [`Self::ALL`] (the pin test
/// counts membership through an exhaustive index match).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcPhase3Outcome {
    /// The cycle drained and its durable commit landed.
    Committed,
    /// The cycle drained but the durable commit provably did not land
    /// (degraded bookkeeping on a completed collection — exit 0).
    CommitLost,
    /// The cycle drained but the commit outcome is unprovable either
    /// way — it may or may not have landed (merged_bug_022; degraded
    /// bookkeeping on a completed collection — exit 0).
    CommitIndeterminate,
    /// Dry run whose durable observation was withheld (degraded
    /// bookkeeping — exit 0).
    PreviewOnly,
    /// Fail-closed suspension: ALL chunk collection is suspended.
    Suspended,
    /// The cycle failed mid-drain; partial destructive work may
    /// already be committed.
    Failed,
}

/// Failure-frame prefix for a fail-closed suspension, colon included —
/// the byte sequence the store renders and the CLI exit posture keys
/// on. A reword is a one-site edit here, never a silent divergence.
pub const GC_CHUNK_COLLECT_SUSPENDED_PREFIX: &str = "chunk collect SUSPENDED:";
/// Failure-frame prefix for a mid-drain cycle failure, colon included.
pub const GC_CHUNK_COLLECT_FAILED_PREFIX: &str = "chunk collect FAILED:";

impl GcPhase3Outcome {
    /// Every variant exactly once (pinned by
    /// `gc_failure_prefix_alphabet_pinned`); the executable predicate
    /// below derives from this set, so an outcome absent here cannot
    /// silently fall out of the exit posture.
    pub const ALL: [GcPhase3Outcome; 6] = [
        GcPhase3Outcome::Committed,
        GcPhase3Outcome::CommitLost,
        GcPhase3Outcome::CommitIndeterminate,
        GcPhase3Outcome::PreviewOnly,
        GcPhase3Outcome::Suspended,
        GcPhase3Outcome::Failed,
    ];

    /// The exit-posture table: a failure-bearing outcome names its
    /// rendered prefix; every non-failure variant is NAMED in the
    /// `None` arm (no wildcard — a new variant must take a position
    /// here or the build breaks).
    pub const fn failure_prefix(self) -> Option<&'static str> {
        match self {
            GcPhase3Outcome::Suspended => Some(GC_CHUNK_COLLECT_SUSPENDED_PREFIX),
            GcPhase3Outcome::Failed => Some(GC_CHUNK_COLLECT_FAILED_PREFIX),
            GcPhase3Outcome::Committed
            | GcPhase3Outcome::CommitLost
            | GcPhase3Outcome::CommitIndeterminate
            | GcPhase3Outcome::PreviewOnly => None,
        }
    }
}

/// THE one executable exit-posture source for `rio-cli gc` (S6b
/// decision provenance lives at the CLI call site): true iff the
/// terminal frame's render begins with a failure-frame prefix.
/// Derived by iterating [`GcPhase3Outcome::ALL`] so the predicate and
/// the posture table cannot disagree.
pub fn gc_render_is_chunk_collect_failure(render: &str) -> bool {
    GcPhase3Outcome::ALL
        .iter()
        .filter_map(|o| o.failure_prefix())
        .any(|prefix| render.starts_with(prefix))
}

#[cfg(test)]
mod gc_phase3_tests {
    use super::*;

    /// merged_bug_052: pin the failure-frame alphabet byte-exact,
    /// colon included — the colon was exactly the half the retired
    /// store-side asserts dropped (`starts_with("chunk collect
    /// SUSPENDED")` stayed green through a reword that broke the CLI
    /// matcher). Membership is counted through an exhaustive index
    /// match, so a new variant cannot ship without joining ALL and
    /// this table.
    #[test]
    fn gc_failure_prefix_alphabet_pinned() {
        // Closure-set census: every variant appears in ALL exactly once.
        fn index(o: GcPhase3Outcome) -> usize {
            match o {
                GcPhase3Outcome::Committed => 0,
                GcPhase3Outcome::CommitLost => 1,
                GcPhase3Outcome::CommitIndeterminate => 2,
                GcPhase3Outcome::PreviewOnly => 3,
                GcPhase3Outcome::Suspended => 4,
                GcPhase3Outcome::Failed => 5,
            }
        }
        let mut seen = [0u8; GcPhase3Outcome::ALL.len()];
        for o in GcPhase3Outcome::ALL {
            seen[index(o)] += 1;
        }
        assert_eq!(seen, [1; GcPhase3Outcome::ALL.len()], "ALL is the alphabet");

        // The posture table, byte-exact.
        let table = [
            (GcPhase3Outcome::Committed, None),
            (GcPhase3Outcome::CommitLost, None),
            (GcPhase3Outcome::CommitIndeterminate, None),
            (GcPhase3Outcome::PreviewOnly, None),
            (GcPhase3Outcome::Suspended, Some("chunk collect SUSPENDED:")),
            (GcPhase3Outcome::Failed, Some("chunk collect FAILED:")),
        ];
        for (outcome, want) in table {
            assert_eq!(outcome.failure_prefix(), want, "outcome={outcome:?}");
        }

        // Colon-included: a prefix that loses its colon re-opens the
        // weak-assert hole.
        for o in GcPhase3Outcome::ALL {
            if let Some(p) = o.failure_prefix() {
                assert!(p.ends_with(':'), "prefix must keep the colon: {p:?}");
                assert!(
                    gc_render_is_chunk_collect_failure(&format!("{p} details")),
                    "the predicate accepts its own alphabet"
                );
            }
        }

        // Non-failure shapes pass.
        for render in [
            "complete: 3 paths deleted",
            "dry run: would delete 3 paths",
            "already running (concurrent GC in progress)",
            "",
        ] {
            assert!(!gc_render_is_chunk_collect_failure(render));
        }
    }
}
