//! `ExecutorError` / `ExecutionResult` → `CompletionReport` mapping.
//!
//! Separated from `spawn_build_task` so the cancelled / permanent /
//! infrastructure status decision and the SLI outcome label live next to
//! each other instead of inline in a 300-line spawned async block.

use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc;

use rio_proto::types::{
    BuildResult as ProtoBuildResult, BuildResultStatus, CompletionReport, ExecutorMessage,
    ResourceUsage, executor_message,
};

use crate::executor::{ExecutionResult, ExecutorError};

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
}

/// Map a successful `ExecutionResult` to its `CompletionReport`. Resource
/// fields flow from the executor (cgroup `memory.peak` + polled `cpu.stat`).
/// `r.fixture_resources` (test-fixtures only) overrides the cgroup-poll
/// snapshot so scripted builds can inject `cpu_limit_cores`/`cpu_seconds`.
pub(super) fn ok_completion(r: ExecutionResult, stamp: CompletionStamp) -> CompletionReport {
    CompletionReport {
        drv_path: r.drv_path,
        result: Some(r.result),
        assignment_token: r.assignment_token,
        peak_memory_bytes: r.peak_memory_bytes,
        peak_cpu_cores: r.peak_cpu_cores,
        node_name: stamp.node_name,
        hw_class: stamp.hw_class,
        final_resources: r.fixture_resources.or(stamp.final_resources),
    }
}

/// Map an `ExecutorError` to a `CompletionReport`.
///
/// `was_cancelled` is read from the build's cancel flag BEFORE deciding
/// the status — `try_cancel_build` sets it BEFORE writing `cgroup.kill`;
/// the kill → SIGKILL → stdout-EOF → `Err` path has some latency, so by
/// the time the result is observed the flag is set. Three buckets:
///
/// - `was_cancelled` → `Cancelled`. Expected outcome of `CancelBuild` /
///   `DrainExecutor(force)`. Not an error — info-logged. Scheduler's
///   completion handler treats `Cancelled` as a no-op (already
///   transitioned the derivation when it sent the `CancelSignal`).
/// - `e.is_permanent()` → `InputRejected`. Deterministic per-derivation
///   (`WrongKind`, `.drv` parse failure). Another pod will fail
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
    stamp: CompletionStamp,
) -> CompletionReport {
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
    CompletionReport {
        drv_path,
        result: Some(ProtoBuildResult {
            status: status.into(),
            error_msg: if was_cancelled {
                "cancelled by scheduler".into()
            } else {
                e.to_string()
            },
            ..Default::default()
        }),
        assignment_token,
        // Executor error → cgroup never populated.
        // All resource fields = 0 = no-signal.
        peak_memory_bytes: 0,
        peak_cpu_cores: 0.0,
        node_name: stamp.node_name,
        hw_class: stamp.hw_class,
        // Carry the snapshot even on executor error: cpu_seconds_total
        // and peak_disk_bytes from a build that OOMed are exactly what
        // the SLA model needs to bump resource_floor next time.
        final_resources: stamp.final_resources,
    }
}

/// Build-task-panicked `CompletionReport`. Sent by the panic-catcher so
/// the scheduler doesn't leave the derivation stuck in `Running`.
pub(super) fn panic_completion(
    drv_path: String,
    assignment_token: String,
    stamp: CompletionStamp,
) -> CompletionReport {
    CompletionReport {
        drv_path,
        result: Some(ProtoBuildResult {
            status: BuildResultStatus::InfrastructureFailure.into(),
            error_msg: "worker build task panicked".into(),
            ..Default::default()
        }),
        assignment_token,
        // Panic = cgroup file descriptor likely dropped mid-read, or we
        // never got past spawn. 0 = no-signal.
        peak_memory_bytes: 0,
        peak_cpu_cores: 0.0,
        node_name: stamp.node_name,
        hw_class: stamp.hw_class,
        final_resources: stamp.final_resources,
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
/// permanent sink. Every terminal build outcome (success, error,
/// cancel, panic) MUST go through here so:
///
/// - `rio_builder_builds_total{outcome}` increments exactly once per
///   build (observability.md:202 SLI). bug_174: the panic-catcher
///   previously open-coded the send and skipped the counter, so a
///   worker that panicked on 1/100 builds reported the same success
///   rate as a healthy one.
/// - `completion_pending` is set BEFORE the message enters `sink_rx`.
///   The drain machinery (`reconnect_drain_gate`, `wait_build_flushed`)
///   keys off this — NOT `slot.is_busy()` — to decide whether the
///   reconnect loop may exit. bug_472: `_slot_guard` drops AFTER this
///   send, so "slot idle" alone meant "completion queued in sink_rx"
///   when `relay_target=None`, and `run_teardown` dropped it on the
///   floor. `relay_loop` clears the flag only after a successful
///   `grpc_tx.send()` into a confirmed-open stream.
pub(super) async fn send_completion(
    stream_tx: &mpsc::Sender<ExecutorMessage>,
    completion_pending: &AtomicBool,
    completion: CompletionReport,
) {
    completion_pending.store(true, Ordering::Release);
    metrics::counter!("rio_builder_builds_total", "outcome" => outcome_label(&completion))
        .increment(1);
    let msg = ExecutorMessage {
        msg: Some(executor_message::Msg::Completion(completion)),
    };
    if let Err(e) = stream_tx.send(msg).await {
        tracing::error!(error = %e, "failed to send completion report");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::metrics::CountingRecorder;

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
        let pending = AtomicBool::new(false);
        let completion = panic_completion(
            "/nix/store/aaa-x.drv".into(),
            "tok".into(),
            CompletionStamp {
                node_name: None,
                hw_class: None,
                final_resources: None,
            },
        );

        send_completion(&tx, &pending, completion).await;

        assert_eq!(
            rec.get("rio_builder_builds_total{outcome=infra_failure}"),
            1,
            "panic-path completion must increment the SLI counter \
             (saw keys: {:?})",
            rec.all_keys()
        );
        assert!(
            pending.load(Ordering::Acquire),
            "completion_pending set before sink send (drain-gate hook)"
        );
        // Message actually landed in the sink.
        let msg = rx.recv().await.unwrap();
        assert!(matches!(
            msg.msg,
            Some(executor_message::Msg::Completion(_))
        ));
    }
}
