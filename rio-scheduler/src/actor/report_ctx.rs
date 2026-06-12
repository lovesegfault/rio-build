//! Constructor-gated failure report context (bughunt-2 slot 3, C3).
//!
//! The module boundary makes field privacy real: `store_degraded` is
//! private to this file, so only the two constructors below can set
//! it — and [`FailureReportCtx::non_infra`] **has no degraded
//! parameter**. A non-infrastructure completion arm structurally
//! cannot forward the worker's wire bit into the uncharged
//! `store_degraded` pacing class (merged_bug_072 / bug_096): a future
//! permanent arm calling `infra(…, result.store_degraded)` directly is
//! caught by the literal-construction policy test in
//! `actor/tests/completion.rs`, which pins `failure_ctx_for` as the
//! sole non-test producer.

/// Report-carried context for the worker-reported failure handlers
/// (E1–E4): the fields of the triggering `CompletionReport` the failure
/// paths consume, borrowed at the `handle_completion` routing match.
///
/// `final_line_count` is already in the stamp's `Option` form (the
/// proto's `0` "not reported" sentinel and out-of-range values become
/// `None` — same conversion as the success path), so a failure-terminal
/// path can hand it straight to the `drv_executions` stamp. Reportless
/// exit paths (disconnect, controller reports, backstop, recovery)
/// never construct one — they keep stamping `NULL`.
#[derive(Debug, Clone, Copy)]
pub(super) struct FailureReportCtx<'a> {
    /// `CompletionReport.final_line_count`, `None` when not reported.
    pub(super) final_line_count: Option<i64>,
    /// Worker-provided error message (may be empty). DISPLAY/NARRATION
    /// ONLY (bug_090): no decision in any failure handler may dispatch
    /// on this text — sizing classifications ride [`Self::sizing`].
    pub(super) error_msg: &'a str,
    /// bug_408: `BuildResult.store_degraded` — the builder's FUSE
    /// breaker attributed this infrastructure failure to a degraded
    /// store. Routes the report to the uncharged `store_degraded`
    /// class (`sched.retry.store-degraded-uncharged`). Private:
    /// settable only by [`Self::infra`].
    store_degraded: bool,
    /// bug_090: the typed sizing claim (class + corroborating
    /// telemetry + the report's memory peak — the oom axis's
    /// corroborant). Private and settable only by [`Self::infra`]:
    /// a non-infrastructure arm structurally cannot express a sizing
    /// claim (the same C3 module-boundary law as `store_degraded`).
    sizing: Option<SizingClaim>,
}

/// bug_090: the typed sizing claim the floor gate consumes — a
/// Copy-flattened carry of `BuildResult.failure_classification` plus
/// the report-level `peak_memory_bytes` (the CGROUP_OOM corroborant).
#[derive(Debug, Clone, Copy)]
pub(super) struct SizingClaim {
    /// The wire class (decoded; `Unspecified` never constructs a
    /// claim — see [`FailureReportCtx::infra`]).
    pub(super) class: rio_proto::types::FailureClass,
    /// DISK_FULL corroboration triple, when carried.
    pub(super) quota: Option<rio_proto::types::QuotaTelemetry>,
    /// `CompletionReport.peak_memory_bytes` — the oom corroborant
    /// (memory.peak saturates at memory.max under an oom kill).
    pub(super) peak_memory_bytes: u64,
}

impl<'a> FailureReportCtx<'a> {
    /// Context for the `InfrastructureFailure` dispatch arm — the only
    /// arm allowed to carry the worker's store-degraded attribution
    /// and the typed sizing claim. An absent or `Unspecified`
    /// classification constructs NO claim (the Q6 legacy reading:
    /// classify-only).
    pub(super) fn infra(
        final_line_count: Option<i64>,
        error_msg: &'a str,
        store_degraded: bool,
        classification: Option<&rio_proto::types::FailureClassification>,
        peak_memory_bytes: u64,
    ) -> Self {
        let sizing = classification.and_then(|fc| {
            let class = rio_proto::types::FailureClass::try_from(fc.class)
                .unwrap_or(rio_proto::types::FailureClass::Unspecified);
            (class != rio_proto::types::FailureClass::Unspecified).then_some(SizingClaim {
                class,
                quota: fc.quota,
                peak_memory_bytes,
            })
        });
        Self {
            final_line_count,
            error_msg,
            store_degraded,
            sizing,
        }
    }

    /// Context for every non-infrastructure failure arm. No degraded
    /// and no sizing parameter exists: these arms cannot express
    /// store evidence or a sizing claim.
    pub(super) fn non_infra(final_line_count: Option<i64>, error_msg: &'a str) -> Self {
        Self {
            final_line_count,
            error_msg,
            store_degraded: false,
            sizing: None,
        }
    }

    /// Whether the (infrastructure) report attributed the failure to a
    /// degraded store.
    pub(super) fn store_degraded(&self) -> bool {
        self.store_degraded
    }

    /// The typed sizing claim, if the (infrastructure) report carried
    /// one.
    pub(super) fn sizing(&self) -> Option<SizingClaim> {
        self.sizing
    }
}
