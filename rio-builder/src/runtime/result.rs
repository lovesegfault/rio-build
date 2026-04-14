//! `ExecutorError` / `ExecutionResult` → `CompletionReport` mapping.
//!
//! Separated from `spawn_build_task` so the cancelled / permanent /
//! infrastructure status decision and the SLI outcome label live next to
//! each other instead of inline in a 300-line spawned async block.

use rio_proto::types::{BuildResult as ProtoBuildResult, BuildResultStatus, CompletionReport};

use crate::executor::{ExecutionResult, ExecutorError};

/// Map a successful `ExecutionResult` to its `CompletionReport`. Resource
/// fields flow from the executor (cgroup `memory.peak` + polled `cpu.stat`).
pub(super) fn ok_completion(r: ExecutionResult, node_name: Option<String>) -> CompletionReport {
    CompletionReport {
        drv_path: r.drv_path,
        result: Some(r.result),
        assignment_token: r.assignment_token,
        peak_memory_bytes: r.peak_memory_bytes,
        output_size_bytes: r.output_size_bytes,
        peak_cpu_cores: r.peak_cpu_cores,
        node_name,
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
    node_name: Option<String>,
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
        output_size_bytes: 0,
        peak_cpu_cores: 0.0,
        node_name,
    }
}

/// Build-task-panicked `CompletionReport`. Sent by the panic-catcher so
/// the scheduler doesn't leave the derivation stuck in `Running`.
pub(super) fn panic_completion(
    drv_path: String,
    assignment_token: String,
    node_name: Option<String>,
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
        output_size_bytes: 0,
        peak_cpu_cores: 0.0,
        node_name,
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
