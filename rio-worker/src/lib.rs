//! Build executor with FUSE store for rio-build.
//!
//! Receives build assignments from the scheduler, runs builds using
//! nix-daemon within an overlayfs+FUSE environment, and uploads
//! results to the store.
//!
//! # Architecture
//!
//! ```text
//! rio-worker binary
//! +-- gRPC clients
//! |   +-- WorkerService.BuildExecution (bidi stream to scheduler)
//! |   +-- WorkerService.Heartbeat (periodic to scheduler)
//! |   +-- StoreService (fetch inputs, upload outputs)
//! +-- FUSE daemon (fuse/)
//! |   +-- Mount /nix/store via fuser 0.17
//! |   +-- lookup/getattr -> StoreService.QueryPathInfo
//! |   +-- read/readdir -> SSD cache or StoreService.GetPath
//! |   +-- LRU cache on local SSD (cache.rs)
//! +-- Build executor (executor.rs)
//! |   +-- Overlay management (overlay.rs)
//! |   +-- Synthetic DB generation (synth_db.rs)
//! |   +-- Log streaming (log_stream.rs)
//! |   +-- Output upload (upload.rs)
//! +-- Heartbeat loop (10s interval)
//! ```

pub mod executor;
pub mod fuse;
pub mod log_stream;
pub mod overlay;
pub mod synth_db;
pub mod upload;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use tokio::sync::{RwLock, mpsc};
use tonic::transport::Channel;

use rio_proto::store::store_service_client::StoreServiceClient;
use rio_proto::types::{
    CompletionReport, HeartbeatRequest, ResourceUsage, WorkAssignment, WorkAssignmentAck,
    WorkerMessage, worker_message,
};

/// Default threshold for leaked overlay mounts before the worker refuses
/// new builds. Overridable via `RIO_WORKER_MAX_LEAKED_MOUNTS`.
///
/// A leaked mount means `umount2` failed in `OverlayMount::Drop` — typically
/// the mount is stuck busy (open file handles, zombie nix-daemon). After N
/// leaks the worker is likely in a degraded state; refusing new builds and
/// reporting `InfrastructureFailure` lets the scheduler reassign to a
/// healthy worker, and the worker can be restarted by its supervisor.
const DEFAULT_MAX_LEAKED_MOUNTS: usize = 3;

/// Read the leaked-mount threshold from `RIO_WORKER_MAX_LEAKED_MOUNTS`,
/// falling back to [`DEFAULT_MAX_LEAKED_MOUNTS`].
pub fn max_leaked_mounts() -> usize {
    static THRESHOLD: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *THRESHOLD.get_or_init(|| match std::env::var("RIO_WORKER_MAX_LEAKED_MOUNTS") {
        Ok(val) => match val.parse::<usize>() {
            Ok(n) => {
                tracing::debug!(threshold = n, "using configured max leaked mounts");
                n
            }
            Err(e) => {
                tracing::warn!(
                    value = %val,
                    error = %e,
                    "invalid RIO_WORKER_MAX_LEAKED_MOUNTS, using default {DEFAULT_MAX_LEAKED_MOUNTS}"
                );
                DEFAULT_MAX_LEAKED_MOUNTS
            }
        },
        Err(_) => DEFAULT_MAX_LEAKED_MOUNTS,
    })
}

/// Build a heartbeat request, populating `running_builds` from the shared tracker.
///
/// Extracted for testability — the heartbeat loop in main.rs calls this.
pub async fn build_heartbeat_request(
    worker_id: &str,
    system: &str,
    max_builds: u32,
    running: &RwLock<HashSet<String>>,
) -> HeartbeatRequest {
    let current: Vec<String> = running.read().await.iter().cloned().collect();
    HeartbeatRequest {
        worker_id: worker_id.to_string(),
        running_builds: current,
        resources: Some(ResourceUsage::default()),
        local_paths: None,
        system: system.to_string(),
        supported_features: Vec::new(),
        max_builds,
    }
}

/// Shared context for spawning build tasks.
///
/// Constructed once before the event loop to reduce per-assignment clone
/// boilerplate. `spawn_build_task` clones only what each spawned task needs.
#[derive(Clone)]
pub struct BuildSpawnContext {
    pub store_client: StoreServiceClient<Channel>,
    pub worker_id: String,
    pub fuse_mount_point: PathBuf,
    pub overlay_base_dir: PathBuf,
    pub stream_tx: mpsc::Sender<WorkerMessage>,
    pub running_builds: Arc<RwLock<HashSet<String>>>,
    /// Worker-lifetime count of overlay mounts whose teardown failed.
    /// `execute_build` checks this at entry; `OverlayMount::Drop` increments.
    pub leaked_mounts: Arc<AtomicUsize>,
    /// Per-build log rate/size limits. `Copy`, so cloning into each spawned
    /// task is cheap. Worker-wide (set once at startup from config), not
    /// per-assignment — the limits are a worker policy, not a build option.
    pub log_limits: log_stream::LogLimits,
}

/// Handle a WorkAssignment: ACK the scheduler, spawn the build task, set up
/// a panic-catcher.
///
/// Returns after spawning — does NOT block on build completion. The build runs
/// in its own tokio task holding `permit`; it reports completion via
/// `ctx.stream_tx` and drops the permit on exit (success, failure, or panic).
pub async fn spawn_build_task(
    assignment: WorkAssignment,
    permit: tokio::sync::OwnedSemaphorePermit,
    ctx: &BuildSpawnContext,
) {
    let drv_path = assignment.drv_path.clone();
    let assignment_token = assignment.assignment_token.clone();

    // Send ACK
    let ack = WorkerMessage {
        msg: Some(worker_message::Msg::Ack(WorkAssignmentAck {
            drv_path: drv_path.clone(),
            assignment_token: assignment_token.clone(),
        })),
    };
    if let Err(e) = ctx.stream_tx.send(ack).await {
        tracing::error!(error = %e, "failed to send ACK");
        return; // Permit drops, no build spawned.
    }

    // Track as running (for heartbeat).
    ctx.running_builds.write().await.insert(drv_path.clone());

    // Clone state needed by spawned tasks ('static lifetime).
    let mut build_store_client = ctx.store_client.clone();
    let build_tx = ctx.stream_tx.clone();
    let build_running = ctx.running_builds.clone();
    let build_leaked_mounts = ctx.leaked_mounts.clone();
    let build_drv_path = drv_path.clone();
    let build_env = executor::ExecutorEnv {
        fuse_mount_point: ctx.fuse_mount_point.clone(),
        overlay_base_dir: ctx.overlay_base_dir.clone(),
        worker_id: ctx.worker_id.clone(),
        log_limits: ctx.log_limits,
    };

    // Clone for the panic handler before moving into the task.
    let panic_tx = ctx.stream_tx.clone();
    let panic_drv_path = drv_path.clone();
    let panic_token = assignment_token.clone();

    let handle = rio_common::task::spawn_monitored("build-executor", async move {
        let _permit = permit; // Hold permit until build completes

        // Remove from running_builds on task exit (success, failure, or panic)
        let _running_guard = scopeguard::guard((), move |()| {
            let rb = build_running.clone();
            let drv = build_drv_path;
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    rb.write().await.remove(&drv);
                });
            }
        });

        let result = executor::execute_build(
            &assignment,
            &build_env,
            &mut build_store_client,
            &build_tx,
            &build_leaked_mounts,
        )
        .await;

        // Send CompletionReport.
        // peak_memory_bytes / output_size_bytes: 0 here is the explicit
        // "no signal" value. F2 wires VmHWM sampling in executor/daemon.rs;
        // until then, the scheduler sees 0 and doesn't update the EMA
        // (treats it as missing data, not as "build used zero memory").
        let completion = match result {
            Ok(exec_result) => CompletionReport {
                drv_path: exec_result.drv_path,
                result: Some(exec_result.result),
                assignment_token: exec_result.assignment_token,
                peak_memory_bytes: 0,
                output_size_bytes: 0,
            },
            Err(e) => {
                tracing::error!(
                    drv_path = %drv_path,
                    error = %e,
                    "build execution failed"
                );
                CompletionReport {
                    drv_path,
                    result: Some(rio_proto::types::BuildResult {
                        status: rio_proto::types::BuildResultStatus::InfrastructureFailure.into(),
                        error_msg: e.to_string(),
                        ..Default::default()
                    }),
                    assignment_token,
                    peak_memory_bytes: 0,
                    output_size_bytes: 0,
                }
            }
        };

        // Record outcome for SLI dashboards. Ok(exec) doesn't mean success —
        // check the proto status. Err(ExecutorError) is always infra failure.
        let outcome = match &completion.result {
            Some(r) if r.status == rio_proto::types::BuildResultStatus::Built as i32 => "success",
            _ => "failure",
        };
        metrics::counter!("rio_worker_builds_total", "outcome" => outcome).increment(1);

        let msg = WorkerMessage {
            msg: Some(worker_message::Msg::Completion(completion)),
        };
        if let Err(e) = build_tx.send(msg).await {
            tracing::error!(error = %e, "failed to send completion report");
        }
    });

    // If the build task panics, send InfrastructureFailure so the scheduler
    // doesn't leave the derivation stuck in Running.
    rio_common::task::spawn_monitored("build-panic-catcher", async move {
        if let Err(e) = handle.await
            && e.is_panic()
        {
            tracing::error!(
                drv_path = %panic_drv_path,
                "build task panicked; sending InfrastructureFailure to scheduler"
            );
            let completion = CompletionReport {
                drv_path: panic_drv_path.clone(),
                result: Some(rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::InfrastructureFailure.into(),
                    error_msg: "worker build task panicked".into(),
                    ..Default::default()
                }),
                assignment_token: panic_token,
                // Panic = no VmHWM to read. 0 = "no signal".
                peak_memory_bytes: 0,
                output_size_bytes: 0,
            };
            let msg = WorkerMessage {
                msg: Some(worker_message::Msg::Completion(completion)),
            };
            if let Err(e) = panic_tx.send(msg).await {
                tracing::error!(
                    drv_path = %panic_drv_path,
                    error = %e,
                    "failed to send panic-completion report; derivation may be stuck in Running"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_heartbeat_reports_running_builds() {
        let running = Arc::new(RwLock::new(HashSet::new()));
        running
            .write()
            .await
            .insert("/nix/store/foo.drv".to_string());
        running
            .write()
            .await
            .insert("/nix/store/bar.drv".to_string());

        let req = build_heartbeat_request("worker-1", "x86_64-linux", 2, &running).await;
        assert_eq!(req.running_builds.len(), 2);
        assert!(
            req.running_builds
                .contains(&"/nix/store/foo.drv".to_string())
        );
        assert!(
            req.running_builds
                .contains(&"/nix/store/bar.drv".to_string())
        );
        assert_eq!(req.worker_id, "worker-1");
        assert_eq!(req.system, "x86_64-linux");
        assert_eq!(req.max_builds, 2);
    }

    #[tokio::test]
    async fn test_heartbeat_empty_running_builds() {
        let running = Arc::new(RwLock::new(HashSet::new()));
        let req = build_heartbeat_request("worker-1", "x86_64-linux", 1, &running).await;
        assert!(req.running_builds.is_empty());
    }
}
