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
    /// Worker-provided error message (may be empty).
    pub(super) error_msg: &'a str,
    /// bug_408: `BuildResult.store_degraded` — the builder's FUSE
    /// breaker attributed this infrastructure failure to a degraded
    /// store. Routes the report to the uncharged `store_degraded`
    /// class (`sched.retry.store-degraded-uncharged`). Private:
    /// settable only by [`Self::infra`].
    store_degraded: bool,
}

impl<'a> FailureReportCtx<'a> {
    /// Context for the `InfrastructureFailure` dispatch arm — the only
    /// arm allowed to carry the worker's store-degraded attribution.
    pub(super) fn infra(
        final_line_count: Option<i64>,
        error_msg: &'a str,
        store_degraded: bool,
    ) -> Self {
        Self {
            final_line_count,
            error_msg,
            store_degraded,
        }
    }

    /// Context for every non-infrastructure failure arm. No degraded
    /// parameter exists: these arms cannot express store evidence.
    pub(super) fn non_infra(final_line_count: Option<i64>, error_msg: &'a str) -> Self {
        Self {
            final_line_count,
            error_msg,
            store_degraded: false,
        }
    }

    /// Whether the (infrastructure) report attributed the failure to a
    /// degraded store.
    pub(super) fn store_degraded(&self) -> bool {
        self.store_degraded
    }
}
