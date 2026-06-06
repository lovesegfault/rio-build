//! `ExecutorError` / `ExecutionResult` → `CompletionReport` mapping.
//!
//! Separated from `spawn_build_task` so the cancelled / permanent /
//! infrastructure status decision and the SLI outcome label live next to
//! each other instead of inline in a 300-line spawned async block.

use tokio::sync::mpsc;

use rio_proto::types::{
    BuildResult as ProtoBuildResult, BuildResultStatus, CompletionReport, ResourceUsage,
};

use crate::executor::{BuildTaskMessage, ExecutionResult, ExecutorError};

/// Per-worker fields stamped onto every `CompletionReport` regardless of
/// outcome. Bundled so [`ok_completion`] / [`err_completion`] /
/// [`panic_completion`] don't each grow a parameter per ADR-023 field.
pub(super) struct CompletionStamp {
    pub node_name: Option<String>,
    /// `RIO_HW_CLASS` (controller-stamped pod annotation). Written
    /// through to `build_samples.hw_class` so the SLA fit can normalize
    /// this sample's wall-seconds to reference-seconds. `None` outside
    /// k8s / before the annotator stamps.
    pub hw_class: Option<String>,
    /// Cgroup-poll snapshot at completion time. `None` only if the
    /// caller has no `ResourceSnapshotHandle` (panic-catcher fallback).
    pub final_resources: Option<ResourceUsage>,
    /// Store-degraded evidence, one field per lane. Advisory — the
    /// assembly fns stamp `BuildResult.store_degraded` from it ONLY
    /// when the status is `InfrastructureFailure`: the evidence is a
    /// classification refinement of an infra failure ("the store was
    /// degraded, wait it out"), never a verdict of its own.
    pub store_evidence: StoreEvidenceSet,
}

/// bug_286: per-lane store-degraded evidence.
///
/// One boolean per store-coupled failure lane. Deliberately NO
/// `Default` impl: every construction site must name every lane, so a
/// future store-coupled lane (e.g. a substitute-fetch path) fails to
/// compile at each stamp site until its evidence is wired — the lane
/// set can never silently lag the failure surface.
pub(super) struct StoreEvidenceSet {
    /// bug_408 lane: the FUSE breaker open at completion, or its
    /// monotonic trip count rose during the build (the during-the-build
    /// half catches open-then-auto-closed).
    pub fuse_breaker: bool,
    /// bug_286 lane: the output upload exhausted its retries on a
    /// transport-unreachable store (`upload::is_store_unreachable` —
    /// `UploadExhausted` ending in `Unavailable`/`DeadlineExceeded`).
    pub upload_transport: bool,
}

impl StoreEvidenceSet {
    /// Any lane saw store-degraded evidence.
    pub(super) fn any(&self) -> bool {
        self.fuse_breaker || self.upload_transport
    }
}

// r[impl builder.outcome.store-degraded+1]
/// bug_408 + bug_286: stamp `store_degraded` onto an assembled
/// `ProtoBuildResult` iff the status is `InfrastructureFailure`. The
/// single chokepoint all three assembly paths (ok/err/panic) route
/// through, so a status added later cannot silently carry the flag.
/// Takes the COMBINED lane verdict (`StoreEvidenceSet::any`) — lanes
/// are named at stamp construction, never here.
fn apply_store_degraded(result: &mut ProtoBuildResult, stamp_degraded: bool) {
    result.store_degraded =
        stamp_degraded && result.status == i32::from(BuildResultStatus::InfrastructureFailure);
}

/// Map a successful `ExecutionResult` to its `CompletionReport`. Resource
/// fields flow from the executor (cgroup `memory.peak` + polled `cpu.stat`).
/// `r.fixture_resources` (test-fixtures only) overrides the cgroup-poll
/// snapshot so scripted builds can inject `cpu_limit_cores`/`cpu_seconds`.
pub(super) fn ok_completion(r: ExecutionResult, mut stamp: CompletionStamp) -> CompletionReport {
    // bug_286: the upload lane's evidence rides the ExecutionResult
    // (BuildOutputs -> ExecutionResult -> here); fold it into the stamp
    // so the chokepoint sees the full lane set.
    stamp.store_evidence.upload_transport |= r.store_unreachable;
    let mut result = r.result;
    apply_store_degraded(&mut result, stamp.store_evidence.any());
    CompletionReport {
        drv_path: r.drv_path,
        result: Some(result),
        assignment_token: r.assignment_token,
        peak_memory_bytes: r.peak_memory_bytes,
        peak_cpu_cores: r.peak_cpu_cores,
        node_name: stamp.node_name,
        hw_class: stamp.hw_class,
        final_resources: r.fixture_resources.or(stamp.final_resources),
        // Placeholder. `spawn_build_task` overwrites this with the
        // worker line-number high-water mark after the footer (header +
        // body + footer) before sending — the count isn't known until
        // the once-per-assignment footer send, which happens after this
        // function runs. 0 survives only on paths that never emitted a
        // line ("not reported"; the store maps it to SQL NULL).
        final_line_count: 0,
    }
}

/// Map an `ExecutorError` to a `CompletionReport`.
///
/// `was_cancelled` is read from the build's cancel flag BEFORE deciding
/// the status — `try_cancel_build` sets it BEFORE writing `cgroup.kill`;
/// the kill → SIGKILL → stdout-EOF → `Err` path has some latency, so by
/// the time the result is observed the flag is set. Three buckets:
///
/// - `was_cancelled` → `Cancelled`. Expected outcome of any cancel
///   source: `CancelBuild`, the scheduler's backstop timeout, or
///   `DrainExecutor(force)`. Not an error — info-logged. Scheduler's
///   completion handler treats `Cancelled` as a no-op (by the time the
///   report arrives it has already moved the derivation on — to
///   `Cancelled` on the cancel path, back to `Ready` on the
///   backstop/force-drain paths).
/// - `e.is_permanent()` → `InputRejected`. Deterministic per-derivation
///   under the scheduler's routing (`WrongKind`, `.drv` parse failure).
///   Another pod *of the kind the scheduler will pick* fails
///   identically; surface so the scheduler stops burning ephemeral
///   cold-starts before the poison threshold trips.
/// - else → `InfrastructureFailure`. Node- or network-local executor
///   failure (overlay mount, daemon crash, gRPC, IO). Another pod might
///   succeed → reassign.
pub(super) fn err_completion(
    e: &ExecutorError,
    drv_path: String,
    assignment_token: String,
    was_cancelled: bool,
    mut stamp: CompletionStamp,
    peak_memory_bytes: u64,
    peak_cpu_cores: f64,
) -> CompletionReport {
    // bug_286: an upload error that propagated as ExecutorError (instead
    // of the collect_outputs mapped arm) carries the same upload-lane
    // evidence — classify it here so both error shapes converge.
    if let ExecutorError::Upload(upload_err) = e {
        stamp.store_evidence.upload_transport |= crate::upload::is_store_unreachable(upload_err);
    }
    let status = if was_cancelled {
        tracing::info!(drv_path = %drv_path, "build cancelled (cgroup.kill)");
        BuildResultStatus::Cancelled
    } else if e.is_permanent() {
        tracing::error!(drv_path = %drv_path, error = %e, "build execution failed");
        BuildResultStatus::InputRejected
    } else {
        tracing::error!(drv_path = %drv_path, error = %e, "build execution failed");
        BuildResultStatus::InfrastructureFailure
    };
    let mut result = ProtoBuildResult {
        status: status.into(),
        error_msg: if was_cancelled {
            "cancelled by scheduler".into()
        } else {
            e.to_string()
        },
        ..Default::default()
    };
    apply_store_degraded(&mut result, stamp.store_evidence.any());
    CompletionReport {
        drv_path,
        result: Some(result),
        assignment_token,
        // r[impl builder.cgroup.memory-peak+2]
        // 0 only for pre-cgroup setup errors (drv parse, WrongKind,
        // overlay, daemon-spawn). Populated for CgroupOom /
        // post-handshake Wire / Upload / BuildFailed — exactly the
        // cases where memory.peak ≈ memory.max is the most actionable
        // sizing signal. ExecuteOutcome carries these across the
        // inner-Err boundary; previously hardcoded 0 here defeated that.
        peak_memory_bytes,
        peak_cpu_cores,
        node_name: stamp.node_name,
        hw_class: stamp.hw_class,
        // Carry the snapshot even on executor error: cpu_seconds_total
        // and peak_disk_bytes from a build that OOMed are exactly what
        // the SLA model needs to bump resource_floor next time.
        final_resources: stamp.final_resources,
        // Placeholder; overwritten by the caller (see ok_completion).
        final_line_count: 0,
    }
}

/// Build-task-panicked `CompletionReport`. Sent by the panic-catcher so
/// the scheduler doesn't leave the derivation stuck in `Running`.
pub(super) fn panic_completion(
    drv_path: String,
    assignment_token: String,
    stamp: CompletionStamp,
) -> CompletionReport {
    let mut result = ProtoBuildResult {
        status: BuildResultStatus::InfrastructureFailure.into(),
        error_msg: "worker build task panicked".into(),
        ..Default::default()
    };
    apply_store_degraded(&mut result, stamp.store_evidence.any());
    CompletionReport {
        drv_path,
        result: Some(result),
        assignment_token,
        // Panic = no telemetry path (panic-catcher has no ExecuteOutcome
        // in scope; the build task's stack is gone). 0 = no-signal.
        peak_memory_bytes: 0,
        peak_cpu_cores: 0.0,
        node_name: stamp.node_name,
        hw_class: stamp.hw_class,
        final_resources: stamp.final_resources,
        // Panic path: the line count died with the build task's stack.
        final_line_count: 0,
    }
}

/// The banner footer's `result` string for the final once-per-assignment
/// send, after the cancel override.
///
/// `was_cancelled` (the build's cancel flag, set by `try_cancel_build`
/// before it writes `cgroup.kill`) decides `cancelled` — not the error
/// variant. The post-cgroup cancel kills the daemon, which surfaces as
/// `Wire(Io(UnexpectedEof))`, indistinguishable from a daemon crash at
/// the variant level; only the flag knows the kill was intentional. Same
/// rule as [`err_completion`]'s status decision for the same assignment.
///
/// The override applies to a successful attempt's `ok` too: a cancel
/// that lands after the daemon exits but before the footer send reports
/// the assignment's disposition, not the daemon's exit status. The flag
/// is set by `try_cancel_build` on any cancel (the senders
/// behind [`err_completion`]'s `was_cancelled`); every sender abandons
/// this execution on the scheduler side, so `cancelled` is the right
/// footer regardless of how the attempt ended.
///
/// `None` stays `None` even when cancelled: a pre-cgroup-cancelled build
/// never ran a daemon and header-without-footer is the documented "build
/// never started" signal — fabricating a footer would erase it.
///
/// Best-effort, like the rest of the banner: on the scheduler-initiated
/// cancel path the footer is dropped by the scheduler's cancel-path seal
/// and never reaches the stored log (see `terminal_log_epilogue`'s
/// sequencing note in rio-scheduler); it is still observable on the
/// force-drain/backstop cancel paths.
pub(super) fn final_footer_result(
    last_footer_result: Option<&str>,
    was_cancelled: bool,
) -> Option<&str> {
    match last_footer_result {
        None => None,
        Some(_) if was_cancelled => Some("cancelled"),
        Some(s) => Some(s),
    }
}

/// SLI outcome label for `rio_builder_builds_total`.
///
/// `Ok(exec)` doesn't mean success — check the proto status.
/// `Err(ExecutorError)` is infra failure OR cancelled; the "cancelled"
/// bucket is a distinct label so SLIs don't count user-initiated cancels
/// as failures.
pub(super) fn outcome_label(completion: &CompletionReport) -> &'static str {
    match &completion.result {
        Some(r) => match BuildResultStatus::try_from(r.status) {
            Ok(BuildResultStatus::Built) => "success",
            Ok(BuildResultStatus::Cancelled) => "cancelled",
            // Operationally distinct: means "raise the limit," not
            // "the build is broken." Separate label so SLI queries
            // can exclude these from failure-rate denominators.
            Ok(BuildResultStatus::TimedOut) => "timed_out",
            Ok(BuildResultStatus::LogLimitExceeded) => "log_limit",
            Ok(BuildResultStatus::InfrastructureFailure) => "infra_failure",
            _ => "failure",
        },
        None => "failure",
    }
}

/// Single chokepoint for delivering a `CompletionReport` to the
/// build-task sink. Every terminal build outcome (success, error,
/// cancel, panic) MUST go through here so
/// `rio_builder_builds_total{outcome}` increments exactly once per
/// build (observability.typ SLI). bug_174: the panic-catcher
/// previously open-coded the send and skipped the counter, so a
/// worker that panicked on 1/100 builds reported the same success
/// rate as a healthy one.
///
/// The sink's consumer is the pull loop, which forwards the report
/// through `ReportOutcome` until the scheduler acknowledges it
/// (`r[builder.pull.retry-loop+2]`); the pod's exit code is the only
/// other delivery signal (`r[builder.pull.exit-codes+1]`).
// r[impl builder.completion.exactly-once-or-death+2]
pub(super) async fn send_completion(
    stream_tx: &mpsc::Sender<BuildTaskMessage>,
    completion: CompletionReport,
) {
    metrics::counter!("rio_builder_builds_total", "outcome" => outcome_label(&completion))
        .increment(1);
    let msg = BuildTaskMessage::Completion(Box::new(completion));
    if let Err(e) = stream_tx.send(msg).await {
        tracing::error!(error = %e, "failed to send completion report");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::metrics::CountingRecorder;

    fn stamp() -> CompletionStamp {
        CompletionStamp {
            node_name: None,
            hw_class: None,
            final_resources: None,
            store_evidence: StoreEvidenceSet {
                fuse_breaker: false,
                upload_transport: false,
            },
        }
    }

    fn degraded_stamp() -> CompletionStamp {
        CompletionStamp {
            store_evidence: StoreEvidenceSet {
                fuse_breaker: true,
                upload_transport: false,
            },
            ..stamp()
        }
    }

    /// bug_286 (R6): `is_store_unreachable` truth table — transport
    /// unreachability only; store verdicts, NAR-wrapping Internal,
    /// local IO, and builder bugs are all explicitly NOT evidence.
    #[test]
    fn store_unreachable_truth_table() {
        use crate::upload::{UploadError, is_store_unreachable};
        let exhausted = |status: tonic::Status| UploadError::UploadExhausted {
            path: "/nix/store/x".into(),
            source: status,
        };
        let cases = [
            (exhausted(tonic::Status::unavailable("conn refused")), true),
            (exhausted(tonic::Status::deadline_exceeded("hang")), true),
            (
                exhausted(tonic::Status::internal("NAR serialization")),
                false,
            ),
            (exhausted(tonic::Status::resource_exhausted("quota")), false),
            (exhausted(tonic::Status::invalid_argument("bad")), false),
            (UploadError::Io(std::io::Error::other("disk gone")), false),
            (UploadError::InvalidReference { path: "p".into() }, false),
        ];
        for (err, want) in cases {
            assert_eq!(is_store_unreachable(&err), want, "err={err:?}");
        }
    }

    /// bug_286 (R6, stamp rows): the chokepoint treats the two lanes
    /// identically — upload-transport evidence alone stamps an infra
    /// failure, and never any other status.
    #[test]
    fn upload_lane_stamps_like_fuse_lane() {
        let upload_stamp = || CompletionStamp {
            store_evidence: StoreEvidenceSet {
                fuse_breaker: false,
                upload_transport: true,
            },
            ..stamp()
        };
        for (status, want) in [
            (BuildResultStatus::InfrastructureFailure, true),
            (BuildResultStatus::TransientFailure, false),
            (BuildResultStatus::Built, false),
        ] {
            let mut result = ProtoBuildResult {
                status: status.into(),
                ..Default::default()
            };
            apply_store_degraded(&mut result, upload_stamp().store_evidence.any());
            assert_eq!(result.store_degraded, want, "status={status:?}");
        }
    }

    // r[verify builder.outcome.store-degraded+1]
    /// The stamp chokepoint marks `store_degraded` iff the status is
    /// `InfrastructureFailure` AND the breaker verdict was set: a
    /// transient (build-ran) failure with an open breaker stays false
    /// (the build's own exit is attributable), and an infra failure
    /// with a healthy breaker stays false (no degraded-store evidence).
    /// Pre-fix red: compile-level — neither `BuildResult.store_degraded`
    /// nor `CompletionStamp::store_degraded` existed, so the misclass
    /// (degraded-store EIO folded as chargeable WorkerInfra) was the
    /// only expressible shape.
    #[test]
    fn store_degraded_stamps_infra_failures_only() {
        let cases = [
            (BuildResultStatus::InfrastructureFailure, true, true),
            (BuildResultStatus::InfrastructureFailure, false, false),
            (BuildResultStatus::TransientFailure, true, false),
            (BuildResultStatus::Built, true, false),
            (BuildResultStatus::Cancelled, true, false),
        ];
        for (status, degraded, want) in cases {
            let mut result = ProtoBuildResult {
                status: status.into(),
                ..Default::default()
            };
            apply_store_degraded(&mut result, degraded);
            assert_eq!(
                result.store_degraded, want,
                "status={status:?} degraded={degraded}"
            );
        }
    }

    /// bug_286 (R6, exhausted carry): an `ExecutorError::Upload`
    /// (`UploadExhausted{Unavailable}`) infra failure with a HEALTHY
    /// breaker carries the flag through `err_completion`'s classifier.
    /// RED pre-fix: `assertion failed: report.result.unwrap()
    /// .store_degraded` — the upload lane produced zero evidence.
    #[test]
    fn upload_transport_exhaustion_is_store_degraded() {
        let status = tonic::Status::unavailable("connect refused");
        let report = err_completion(
            &ExecutorError::Upload(crate::upload::UploadError::UploadExhausted {
                path: "/nix/store/x".into(),
                source: status,
            }),
            "/nix/store/x.drv".into(),
            "tok".into(),
            false,
            stamp(), // fuse lane false
            0,
            0.0,
        );
        assert!(report.result.unwrap().store_degraded);
    }

    /// `err_completion` routes through the chokepoint: a non-permanent
    /// executor error under a degraded-store stamp carries the flag.
    #[test]
    fn err_completion_carries_store_degraded() {
        let report = err_completion(
            &ExecutorError::CgroupOom,
            "/nix/store/x.drv".into(),
            "tok".into(),
            false,
            degraded_stamp(),
            0,
            0.0,
        );
        let result = report.result.unwrap();
        assert_eq!(
            result.status,
            i32::from(BuildResultStatus::InfrastructureFailure)
        );
        assert!(result.store_degraded);
    }

    /// r[verify builder.cgroup.memory-peak+2]
    /// `CgroupOom` is the case where `memory.peak` ≈ `memory.max` is the
    /// single most actionable sizing signal. `err_completion` MUST report
    /// the peaks `ExecuteOutcome` carried across the inner-Err boundary,
    /// not 0. Regression: previously hardcoded 0 with the false comment
    /// "Executor error → cgroup never populated" — defeating
    /// `DaemonOutcome`'s whole purpose.
    #[test]
    fn test_err_completion_carries_peaks_for_cgroup_oom() {
        let r = err_completion(
            &ExecutorError::CgroupOom,
            "/nix/store/x.drv".into(),
            "tok".into(),
            false,
            stamp(),
            4_294_967_296,
            3.8,
        );
        assert_eq!(
            r.peak_memory_bytes, 4_294_967_296,
            "OOM'd build's memory.peak ≈ memory.max MUST reach CompletionReport"
        );
        assert_eq!(r.peak_cpu_cores, 3.8);
        assert_eq!(
            r.result.as_ref().map(|b| b.status),
            Some(BuildResultStatus::InfrastructureFailure.into()),
            "CgroupOom is infra (bump resource_floor), not permanent"
        );
    }

    /// Pre-cgroup setup errors (here `WrongKind`) genuinely never
    /// populated the cgroup. Caller passes 0; `err_completion` reports 0
    /// — not a special case, just the parameter value.
    #[test]
    fn test_err_completion_pre_cgroup_zero_peaks() {
        let r = err_completion(
            &ExecutorError::WrongKind {
                is_fod: true,
                executor_kind: rio_proto::types::ExecutorKind::Builder,
            },
            "/nix/store/x.drv".into(),
            "tok".into(),
            false,
            stamp(),
            0,
            0.0,
        );
        assert_eq!(r.peak_memory_bytes, 0);
        assert_eq!(
            r.result.as_ref().map(|b| b.status),
            Some(BuildResultStatus::InputRejected.into())
        );
    }

    /// bug_174 regression: the panic path's `InfrastructureFailure`
    /// completion MUST increment `rio_builder_builds_total{outcome=
    /// "infra_failure"}`. Pre-fix, only the `executor_future` send
    /// site (mod.rs:359) incremented; the panic-catcher (mod.rs:382)
    /// open-coded the send without the counter, so panicked builds
    /// were invisible to the worker-side SLI while the scheduler
    /// correctly received `InfrastructureFailure`.
    ///
    /// Tests the chokepoint directly: both call sites now route
    /// through `send_completion`, so a counter assertion here covers
    /// both. `with_local_recorder` is sync — call `send_completion`
    /// inside the closure via `block_in_place` would need a runtime;
    /// instead use `set_default_local_recorder` (guard-scoped, visible
    /// across `.await` on current_thread).
    #[tokio::test]
    async fn send_completion_increments_counter_for_panic_status() {
        let rec = CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&rec);

        let (tx, mut rx) = mpsc::channel(4);
        let completion = panic_completion("/nix/store/aaa-x.drv".into(), "tok".into(), stamp());

        send_completion(&tx, completion).await;

        assert_eq!(
            rec.get("rio_builder_builds_total{outcome=infra_failure}"),
            1,
            "panic-path completion must increment the SLI counter \
             (saw keys: {:?})",
            rec.all_keys()
        );
        // Message actually landed in the sink.
        let msg = rx.recv().await.unwrap();
        assert!(matches!(msg, BuildTaskMessage::Completion(_)));
    }

    /// The cancel flag — not the executor error variant — decides the
    /// `cancelled` footer, and a build that never ran a daemon gets no
    /// footer even when cancelled. The `Some("failed ...")+cancelled`
    /// fixture is the exact production shape: a post-cgroup cancel kills
    /// the daemon, the per-attempt mapper renders the resulting
    /// `Wire(UnexpectedEof)` as `failed (executor error)`, and the override
    /// corrects it here. The `Some("ok")+cancelled` case pins that the
    /// override is about the assignment's disposition, not the attempt's
    /// error-ness.
    #[test]
    fn final_footer_result_cancel_overrides_variant() {
        // Post-cgroup cancel: daemon ran, was killed, flag set.
        assert_eq!(
            final_footer_result(Some("failed (executor error)"), true),
            Some("cancelled")
        );
        // Cancel lands after a successful daemon exit but before the
        // footer send: the assignment was cancelled, the footer says so.
        assert_eq!(final_footer_result(Some("ok"), true), Some("cancelled"));
        // Pre-cgroup cancel: no daemon ran → no footer, even though the
        // flag is set. Header-without-footer = "build never started".
        assert_eq!(final_footer_result(None, true), None);
        // Not cancelled: passthrough.
        assert_eq!(final_footer_result(Some("ok"), false), Some("ok"));
        assert_eq!(final_footer_result(None, false), None);
    }
}
