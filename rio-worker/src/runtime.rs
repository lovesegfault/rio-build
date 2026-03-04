//! Worker runtime: heartbeat construction and build-task spawning.
//!
//! Extracted from lib.rs — this is the glue between main.rs's event loop
//! and the executor/FUSE/upload subsystems. `build_heartbeat_request`
//! snapshots the FUSE cache bloom filter; `spawn_build_task` wraps
//! `executor::execute_build` with ACK + CompletionReport + panic-catcher.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use tokio::sync::{RwLock, mpsc};
use tonic::transport::Channel;

use rio_proto::StoreServiceClient;
use rio_proto::types::{
    CompletionReport, HeartbeatRequest, ResourceUsage, WorkAssignment, WorkAssignmentAck,
    WorkerMessage, worker_message,
};

use crate::{executor, log_stream};

/// Handle to the FUSE cache's bloom filter. Extracted via
/// `Cache::bloom_handle()` before the Cache is moved into the FUSE
/// mount — lets the heartbeat loop read the same filter that `insert()`
/// writes to.
pub type BloomHandle = Arc<std::sync::RwLock<rio_common::bloom::BloomFilter>>;

/// Build a heartbeat request, populating `running_builds` from the shared
/// tracker and `local_paths` from the FUSE cache bloom filter.
///
/// `bloom` is `Option<&BloomHandle>` because not every test has FUSE
/// mounted. `None` = no filter sent; scheduler treats that worker's
/// locality score as "unknown" (neutral, not penalized).
///
/// Takes a bloom HANDLE (not `&Cache`) because main.rs has to move the
/// Cache into `mount_fuse_background`. The handle is Arc-cloned out
/// before the move; same underlying RwLock, so Cache::insert writes
/// show up in our snapshots.
///
/// Extracted for testability — the heartbeat loop in main.rs calls this.
pub async fn build_heartbeat_request(
    worker_id: &str,
    system: &str,
    max_builds: u32,
    size_class: &str,
    running: &RwLock<HashSet<String>>,
    bloom: Option<&BloomHandle>,
) -> HeartbeatRequest {
    let current: Vec<String> = running.read().await.iter().cloned().collect();

    // Snapshot + serialize. The snapshot clone is ~60 KB (default
    // sizing); cheap for a 10s interval. Cloning out of the lock
    // (instead of holding the guard) means insert() isn't blocked
    // for the duration of the gRPC send.
    //
    // to_wire() returns a tuple because rio-common can't depend on
    // rio-proto (cycle). We unpack into the proto struct here.
    let local_paths = bloom.map(|b| {
        let snapshot = b.read().unwrap_or_else(|e| e.into_inner()).clone();
        let (data, hash_count, num_bits, version) = snapshot.to_wire();
        rio_proto::types::BloomFilter {
            data,
            hash_count,
            num_bits,
            hash_algorithm: rio_proto::types::BloomHashAlgorithm::Blake3256 as i32,
            version,
        }
    });

    HeartbeatRequest {
        worker_id: worker_id.to_string(),
        running_builds: current,
        resources: Some(ResourceUsage::default()),
        local_paths,
        system: system.to_string(),
        supported_features: Vec::new(),
        max_builds,
        // Empty string = unclassified (scheduler maps to None). We
        // pass through verbatim — the worker doesn't interpret it,
        // just declares what the operator configured.
        size_class: size_class.to_string(),
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
    /// Leaked overlay mount threshold (from `Config.max_leaked_mounts`).
    pub max_leaked_mounts: usize,
    /// nix-daemon subprocess timeout (from `Config.daemon_timeout_secs`).
    pub daemon_timeout: std::time::Duration,
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
        max_leaked_mounts: ctx.max_leaked_mounts,
        daemon_timeout: ctx.daemon_timeout,
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

        // Send CompletionReport. peak_memory_bytes (VmHWM) and
        // output_size_bytes flow from the executor.
        let completion = match result {
            Ok(exec_result) => CompletionReport {
                drv_path: exec_result.drv_path,
                result: Some(exec_result.result),
                assignment_token: exec_result.assignment_token,
                peak_memory_bytes: exec_result.peak_memory_bytes,
                output_size_bytes: exec_result.output_size_bytes,
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
    use crate::fuse;

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

        let req = build_heartbeat_request("worker-1", "x86_64-linux", 2, "", &running, None).await;
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
        // No cache → no bloom filter.
        assert!(req.local_paths.is_none());
    }

    #[tokio::test]
    async fn test_heartbeat_empty_running_builds() {
        let running = Arc::new(RwLock::new(HashSet::new()));
        let req = build_heartbeat_request("worker-1", "x86_64-linux", 1, "", &running, None).await;
        assert!(req.running_builds.is_empty());
    }

    /// size_class passes through verbatim (scheduler interprets "" as None).
    #[tokio::test]
    async fn test_heartbeat_includes_size_class() {
        let running = Arc::new(RwLock::new(HashSet::new()));
        let req =
            build_heartbeat_request("worker-1", "x86_64-linux", 1, "large", &running, None).await;
        assert_eq!(req.size_class, "large");

        let req = build_heartbeat_request("worker-1", "x86_64-linux", 1, "", &running, None).await;
        assert_eq!(req.size_class, "", "empty = unclassified");
    }

    /// With a cache, the heartbeat includes a bloom filter that
    /// positive-matches inserted paths.
    #[tokio::test]
    async fn test_heartbeat_includes_bloom_from_cache() {
        // Real Cache needs SQLite on disk — tempdir.
        let dir = tempfile::tempdir().unwrap();
        let cache = fuse::cache::Cache::new(dir.path().to_path_buf(), 1)
            .await
            .unwrap();

        // Cache::insert uses Handle::block_on (designed for FUSE's sync
        // callbacks which run on dedicated threads). Calling it directly
        // from an async test = "cannot block_on within a runtime" panic.
        // spawn_blocking moves it to a blocking-thread-pool thread where
        // block_on is legal.
        let cache = Arc::new(cache);
        {
            let c = Arc::clone(&cache);
            tokio::task::spawn_blocking(move || {
                c.insert("/nix/store/aaa-test-one", 100).unwrap();
                c.insert("/nix/store/bbb-test-two", 200).unwrap();
            })
            .await
            .unwrap();
        }

        // Extract handle (same thing main.rs does before moving Cache
        // into the FUSE mount).
        let bloom = cache.bloom_handle();

        let running = Arc::new(RwLock::new(HashSet::new()));
        let req =
            build_heartbeat_request("worker-1", "x86_64-linux", 1, "", &running, Some(&bloom))
                .await;

        let bloom_proto = req.local_paths.expect("cache present → bloom present");
        assert_eq!(
            bloom_proto.hash_algorithm,
            rio_proto::types::BloomHashAlgorithm::Blake3256 as i32
        );
        assert_eq!(bloom_proto.version, 1);

        // Deserialize and query — proves the wire roundtrip works AND
        // the scheduler would see the inserted paths.
        let filter = rio_common::bloom::BloomFilter::from_wire(
            bloom_proto.data,
            bloom_proto.hash_count,
            bloom_proto.num_bits,
            bloom_proto.hash_algorithm,
            bloom_proto.version,
        )
        .unwrap();

        assert!(filter.maybe_contains("/nix/store/aaa-test-one"));
        assert!(filter.maybe_contains("/nix/store/bbb-test-two"));
        // Absent path — probably-false (could be a false positive, but
        // at 1% FPR with 2 items inserted, that's vanishingly unlikely).
        assert!(!filter.maybe_contains("/nix/store/zzz-never-inserted"));
    }

    /// Bloom filter is populated from SQLite at Cache::new — paths that
    /// were cached in a PREVIOUS run are in the filter immediately.
    #[tokio::test]
    async fn test_bloom_rebuilt_from_sqlite_on_restart() {
        let dir = tempfile::tempdir().unwrap();

        // First "run": insert a path, drop the Cache.
        {
            let cache = Arc::new(
                fuse::cache::Cache::new(dir.path().to_path_buf(), 1)
                    .await
                    .unwrap(),
            );
            let c = Arc::clone(&cache);
            // spawn_blocking for the same block_on-in-async reason as above.
            tokio::task::spawn_blocking(move || {
                c.insert("/nix/store/persistent-path", 100).unwrap();
            })
            .await
            .unwrap();
        }

        // Second "run": fresh Cache on the SAME dir. SQLite persisted;
        // bloom should be rebuilt from it.
        let cache = fuse::cache::Cache::new(dir.path().to_path_buf(), 1)
            .await
            .unwrap();
        let snapshot = cache.bloom_snapshot();

        assert!(
            snapshot.maybe_contains("/nix/store/persistent-path"),
            "bloom should include paths from previous run's SQLite"
        );
    }
}
