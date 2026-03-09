//! Worker runtime: heartbeat construction and build-task spawning.
//!
//! Extracted from lib.rs — this is the glue between main.rs's event loop
//! and the executor/FUSE/upload subsystems. `build_heartbeat_request`
//! snapshots the FUSE cache bloom filter; `spawn_build_task` wraps
//! `executor::execute_build` with ACK + CompletionReport + panic-catcher.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};

use tokio::sync::{RwLock, Semaphore, mpsc};
use tonic::transport::Channel;

use rio_proto::StoreServiceClient;
use rio_proto::types::{
    CompletionReport, HeartbeatRequest, PrefetchHint, ResourceUsage, WorkAssignment,
    WorkAssignmentAck, WorkerMessage, worker_message,
};

use crate::{executor, fuse, log_stream};

/// Handle to the FUSE cache's bloom filter. Extracted via
/// `Cache::bloom_handle()` before the Cache is moved into the FUSE
/// mount — lets the heartbeat loop read the same filter that `insert()`
/// writes to.
pub type BloomHandle = Arc<std::sync::RwLock<rio_common::bloom::BloomFilter>>;

/// Per-build cancel registration: cgroup path (for cgroup.kill) +
/// cancelled flag (for spawn_build_task to distinguish Cancelled
/// from InfrastructureFailure when execute_build returns Err).
///
/// Type alias appeases `clippy::type_complexity` for the nested
/// `Arc<RwLock<HashMap<String, (PathBuf, Arc<AtomicBool>)>>>` on
/// `BuildSpawnContext.cancel_registry`. The inner types ARE the
/// right shape — extracting a struct for `(PathBuf, Arc<AtomicBool>)`
/// would be more indirection for two fields read once each.
pub type CancelRegistry = std::sync::RwLock<HashMap<String, (PathBuf, Arc<AtomicBool>)>>;

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
///
/// `systems` + `features`: slice refs to the worker's static config.
/// Both are `.to_vec()`'d into the proto — a heartbeat every 10s
/// means ~100 allocs/min for typically 1-3 elements; not worth the
/// lifetime-threading to avoid.
pub async fn build_heartbeat_request(
    worker_id: &str,
    systems: &[String],
    features: &[String],
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
        systems: systems.to_vec(),
        supported_features: features.to_vec(),
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
    /// Parent cgroup (`cgroup::delegated_root()` — PARENT of the
    /// worker's own cgroup), validated at startup. Each build creates
    /// a sub-cgroup under here as a SIBLING of the worker. Set ONCE
    /// in main.rs after `enable_subtree_controllers` succeeds — if
    /// that fails, main.rs bails with `?` and we never get here. So
    /// this is always a valid, delegated cgroup2 path.
    pub cgroup_parent: PathBuf,
    /// FOD proxy URL. Passed to ExecutorEnv → daemon spawn.
    pub fod_proxy_url: Option<String>,
    /// drv_path → (cgroup path, cancel flag). Populated by
    /// execute_build after the BuildCgroup is created; removed by
    /// the scopeguard at the end of spawn_build_task (same lifetime
    /// as running_builds). The Cancel handler in main.rs looks up
    /// by drv_path, writes the cgroup.kill pseudo-file, AND sets
    /// the flag so spawn_build_task knows to report Cancelled (not
    /// InfrastructureFailure) when execute_build returns Err.
    ///
    /// The AtomicBool is the handshake: cgroup.kill SIGKILLs the
    /// daemon → run_daemon_build sees stdout EOF → returns Err →
    /// execute_build returns Err → WITHOUT the flag, spawn_build_
    /// task would report InfrastructureFailure. With it: Cancelled.
    ///
    /// std::sync::RwLock not tokio::sync::RwLock: writes are rare
    /// (once per build start/end, once per cancel), reads are
    /// cancel-only. std::sync is simpler and the critical sections
    /// are short (HashMap insert/remove/get — no await inside).
    pub cancel_registry: Arc<CancelRegistry>,
}

/// Attempt to cancel a build by drv_path. Looks up the cgroup
/// in the registry, writes cgroup.kill, sets the cancel flag.
///
/// Returns `true` if the build was found and kill was attempted
/// (kill may still fail if the cgroup was already removed — we
/// log and consider it "cancelled anyway"). `false` if not found
/// (build already finished, or never started).
///
/// Called from main.rs's `Msg::Cancel` handler. Fire-and-forget:
/// the scheduler doesn't wait for confirmation (it's already
/// transitioned the derivation to Cancelled on its side — this
/// is just cleanup).
pub fn try_cancel_build(registry: &CancelRegistry, drv_path: &str) -> bool {
    // Read lock: we only need to look up. The cgroup.kill write
    // doesn't mutate our data structure. The AtomicBool store
    // doesn't need a write lock either.
    let guard = registry.read().unwrap_or_else(|e| e.into_inner());
    let Some((cgroup_path, cancelled)) = guard.get(drv_path) else {
        // Not found: build finished between the scheduler sending
        // CancelSignal and us receiving it, OR it never started
        // (assignment dropped due to permit-acquire failure). Either
        // way: nothing to cancel.
        tracing::debug!(
            drv_path,
            "cancel: build not in registry (finished or never started)"
        );
        return false;
    };

    // Set flag BEFORE kill: if there's a race where execute_build
    // is reading the flag right now, we want "cancelled=true" to
    // be visible by the time it sees the Err from run_daemon_build.
    // The kill → stdout EOF → Err path has some latency (kernel
    // delivers SIGKILL, process dies, pipe closes, tokio wakes);
    // setting the flag first gives us a wider window.
    cancelled.store(true, std::sync::atomic::Ordering::Release);

    match crate::cgroup::kill_cgroup(cgroup_path) {
        Ok(()) => {
            tracing::info!(drv_path, cgroup = %cgroup_path.display(), "build cancelled via cgroup.kill");
            true
        }
        Err(e) => {
            // cgroup gone (race with Drop) or kernel too old for
            // cgroup.kill. Log but still return true — the flag is
            // set, so if the build IS still running it'll report
            // Cancelled; if it finished, no harm.
            tracing::warn!(drv_path, error = %e, "cgroup.kill failed (build may have finished)");
            true
        }
    }
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

    // Register in the cancel registry. We know the cgroup path
    // deterministically: cgroup_parent/sanitize_build_id(drv_path).
    // execute_build creates this AFTER spawning the daemon (needs
    // PID); we register PREDICTIVELY here so a Cancel arriving
    // early still finds the entry. If Cancel arrives BEFORE the
    // cgroup exists, cgroup.kill → ENOENT → try_cancel_build logs
    // warn, build proceeds (tiny race, scheduler's backstop timeout
    // catches it).
    //
    // The cancelled flag: set by try_cancel_build BEFORE killing.
    // Read below in the Err arm to distinguish "cancelled" (user
    // intent, Cancelled status) from "executor failed" (infra issue,
    // InfrastructureFailure status).
    let build_id = executor::sanitize_build_id(&drv_path);
    let cgroup_path = ctx.cgroup_parent.join(&build_id);
    let cancelled = Arc::new(AtomicBool::new(false));
    ctx.cancel_registry
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(drv_path.clone(), (cgroup_path, cancelled.clone()));

    // Clone state needed by spawned tasks ('static lifetime).
    let mut build_store_client = ctx.store_client.clone();
    let build_tx = ctx.stream_tx.clone();
    let build_running = ctx.running_builds.clone();
    let build_leaked_mounts = ctx.leaked_mounts.clone();
    let build_drv_path = drv_path.clone();
    let build_cancel_registry = ctx.cancel_registry.clone();
    let build_cancelled = cancelled;
    let build_env = executor::ExecutorEnv {
        fuse_mount_point: ctx.fuse_mount_point.clone(),
        overlay_base_dir: ctx.overlay_base_dir.clone(),
        worker_id: ctx.worker_id.clone(),
        log_limits: ctx.log_limits,
        max_leaked_mounts: ctx.max_leaked_mounts,
        daemon_timeout: ctx.daemon_timeout,
        cgroup_parent: ctx.cgroup_parent.clone(),
        fod_proxy_url: ctx.fod_proxy_url.clone(),
    };

    // Clone for the panic handler before moving into the task.
    let panic_tx = ctx.stream_tx.clone();
    let panic_drv_path = drv_path.clone();
    let panic_token = assignment_token.clone();

    let handle = rio_common::task::spawn_monitored("build-executor", async move {
        let _permit = permit; // Hold permit until build completes

        // Remove from running_builds + cancel_registry on task exit
        // (success, failure, panic, or cancellation). Same lifetime
        // for both — they track "this build is in-flight."
        let cleanup_drv_path = build_drv_path.clone();
        let _running_guard = scopeguard::guard((), move |()| {
            let rb = build_running.clone();
            let reg = build_cancel_registry.clone();
            let drv = cleanup_drv_path;
            // Cancel registry: synchronous (std::sync::RwLock), no
            // await needed. Remove here directly; the running_builds
            // remove still needs a spawned task (tokio RwLock).
            reg.write().unwrap_or_else(|e| e.into_inner()).remove(&drv);
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

        // Send CompletionReport. Resource fields flow from the executor
        // (cgroup memory.peak + polled cpu.stat).
        let completion = match result {
            Ok(exec_result) => CompletionReport {
                drv_path: exec_result.drv_path,
                result: Some(exec_result.result),
                assignment_token: exec_result.assignment_token,
                peak_memory_bytes: exec_result.peak_memory_bytes,
                output_size_bytes: exec_result.output_size_bytes,
                peak_cpu_cores: exec_result.peak_cpu_cores,
            },
            Err(e) => {
                // Check the cancel flag BEFORE deciding the status.
                // try_cancel_build sets this BEFORE writing cgroup.kill;
                // the kill → SIGKILL → stdout-EOF → Err path has some
                // latency, so by the time we're here the flag is set.
                // Acquire pairs with try_cancel_build's Release — not
                // strictly needed (no other state to synchronize) but
                // cheap and documents the pairing.
                let was_cancelled = build_cancelled.load(std::sync::atomic::Ordering::Acquire);
                let (status, log_level) = if was_cancelled {
                    // Expected outcome of CancelBuild / DrainWorker(force).
                    // Not an error — info, not error. Scheduler's
                    // completion handler treats Cancelled as a no-op
                    // (already transitioned the derivation when it sent
                    // the CancelSignal).
                    (rio_proto::types::BuildResultStatus::Cancelled, false)
                } else {
                    // Genuine executor failure (overlay mount fail,
                    // daemon spawn fail, etc). Error-log.
                    (
                        rio_proto::types::BuildResultStatus::InfrastructureFailure,
                        true,
                    )
                };
                if log_level {
                    tracing::error!(
                        drv_path = %drv_path,
                        error = %e,
                        "build execution failed"
                    );
                } else {
                    tracing::info!(
                        drv_path = %drv_path,
                        "build cancelled (cgroup.kill)"
                    );
                }
                CompletionReport {
                    drv_path,
                    result: Some(rio_proto::types::BuildResult {
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
                }
            }
        };

        // Record outcome for SLI dashboards. Ok(exec) doesn't mean success —
        // check the proto status. Err(ExecutorError) is infra failure OR
        // cancelled; the "cancelled" bucket is a distinct label so SLIs
        // don't count user-initiated cancels as failures.
        let outcome = match &completion.result {
            Some(r) if r.status == rio_proto::types::BuildResultStatus::Built as i32 => "success",
            Some(r) if r.status == rio_proto::types::BuildResultStatus::Cancelled as i32 => {
                "cancelled"
            }
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
                // Panic = cgroup file descriptor likely dropped mid-
                // read, or we never got past spawn. 0 = no-signal.
                peak_memory_bytes: 0,
                output_size_bytes: 0,
                peak_cpu_cores: 0.0,
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

/// Handle a PrefetchHint from the scheduler: spawn one fire-and-forget
/// task per path to warm the FUSE cache.
///
/// Called from main.rs's event loop. Does NOT block the caller: each path is spawned
/// as an independent tokio task that acquires a permit from `sem`
/// before entering the blocking pool.
///
/// No JoinHandle tracking: prefetch is fire-and-forget. If the worker
/// SIGTERMs mid-prefetch, the tasks abort with the runtime — the
/// partial fetch is in a .tmp-XXXX sibling dir (see fetch_extract_insert)
/// which cache init cleans up on next start.
pub fn handle_prefetch_hint(
    prefetch: PrefetchHint,
    cache: Arc<fuse::cache::Cache>,
    store_client: StoreServiceClient<Channel>,
    rt: tokio::runtime::Handle,
    sem: Arc<Semaphore>,
) {
    // Spawn one task per path. Don't await — the
    // whole point is to NOT block the stream loop
    // on prefetch. The semaphore bounds concurrent
    // in-flight; excess queue in tokio's task
    // scheduler (cheap — no blocking-pool thread
    // is held until the permit is acquired).
    for store_path in prefetch.store_paths {
        // Scheduler sends full paths; we need
        // basename. Malformed (no /nix/store/
        // prefix) → skip with debug log. Don't
        // fail the loop — one bad path in a
        // batch shouldn't poison the rest.
        let Some(basename) = store_path.strip_prefix("/nix/store/") else {
            tracing::debug!(
                path = %store_path,
                "prefetch: malformed path (no /nix/store/ prefix), skipping"
            );
            metrics::counter!("rio_worker_prefetch_total", "result" => "malformed").increment(1);
            continue;
        };
        let basename = basename.to_string();

        // Clone handles into the task. All cheap:
        // Arc clone, tonic Channel is Arc-internal,
        // tokio Handle is a lightweight token.
        let cache = Arc::clone(&cache);
        let client = store_client.clone();
        let rt = rt.clone();
        let sem = Arc::clone(&sem);

        tokio::spawn(async move {
            // Permit BEFORE spawn_blocking: if the
            // semaphore is saturated, this task
            // waits here (cheap async wait) not
            // in the blocking pool. Tasks queue
            // in tokio's scheduler; blocking
            // threads only taken when a permit
            // is available.
            //
            // On Err(Closed): semaphore closed →
            // worker shutting down. Drop the
            // prefetch silently — it was a hint.
            let Ok(_permit) = sem.acquire_owned().await else {
                return;
            };

            // spawn_blocking: Cache methods use
            // block_on internally (nested-runtime
            // panic from async). The permit moves
            // into the blocking closure and drops
            // when it returns — next queued task
            // wakes.
            let result = tokio::task::spawn_blocking(move || {
                use crate::fuse::fetch::{PrefetchSkip, prefetch_path_blocking};
                let _permit = _permit; // hold through blocking work
                match prefetch_path_blocking(&cache, &client, &rt, &basename) {
                    Ok(None) => "fetched",
                    Ok(Some(PrefetchSkip::AlreadyCached)) => "already_cached",
                    Ok(Some(PrefetchSkip::AlreadyInFlight)) => "already_in_flight",
                    Err(_) => "error",
                }
            })
            .await;

            // JoinError (panic in blocking) →
            // record as "panic". Don't re-panic
            // — we're fire-and-forget.
            let label = result.unwrap_or("panic");
            metrics::counter!("rio_worker_prefetch_total", "result" => label).increment(1);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse;

    // ---- try_cancel_build ----

    /// Entry in registry + cgroup.kill file exists → kill written,
    /// flag set, returns true.
    #[test]
    fn cancel_build_found_in_registry() {
        // Use a tmpdir as a fake cgroup. cgroup.kill is a write-
        // once pseudo-file in a real cgroup2fs; in tmpfs it's just
        // a regular file that gets the "1" written. Good enough
        // for testing the plumbing (real cgroup behavior is
        // VM-tested in vm-phase3b).
        let tmpdir = tempfile::tempdir().unwrap();
        let cgroup_path = tmpdir.path().to_path_buf();
        let cancelled = Arc::new(AtomicBool::new(false));

        let registry = std::sync::RwLock::new(HashMap::from([(
            "/nix/store/test.drv".to_string(),
            (cgroup_path.clone(), cancelled.clone()),
        )]));

        let found = try_cancel_build(&registry, "/nix/store/test.drv");
        assert!(found, "entry in registry → true");
        assert!(
            cancelled.load(std::sync::atomic::Ordering::Acquire),
            "flag set — spawn_build_task reads this to report Cancelled"
        );
        assert_eq!(
            std::fs::read_to_string(cgroup_path.join("cgroup.kill")).unwrap(),
            "1",
            "cgroup.kill written with '1' (kernel trigger in real cgroup2fs)"
        );
    }

    /// Not in registry → returns false, nothing written.
    #[test]
    fn cancel_build_not_found() {
        let registry = std::sync::RwLock::new(HashMap::new());
        let found = try_cancel_build(&registry, "/nix/store/absent.drv");
        assert!(
            !found,
            "not in registry → false (build finished or never started)"
        );
    }

    /// Entry exists but cgroup path doesn't → still sets flag,
    /// returns true. Kill-write fails (ENOENT), logged as warn.
    /// This is the "Cancel arrived before cgroup created" race.
    #[test]
    fn cancel_build_cgroup_missing_still_sets_flag() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let registry = std::sync::RwLock::new(HashMap::from([(
            "/nix/store/test.drv".to_string(),
            (PathBuf::from("/nonexistent/cgroup/path"), cancelled.clone()),
        )]));

        let found = try_cancel_build(&registry, "/nix/store/test.drv");
        assert!(found, "entry found → true even if kill fails");
        assert!(
            cancelled.load(std::sync::atomic::Ordering::Acquire),
            "flag set FIRST — if the build proceeds and later Errs, \
             spawn_build_task still sees cancelled=true and reports correctly"
        );
    }

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

        let req = build_heartbeat_request(
            "worker-1",
            &["x86_64-linux".into()],
            &[],
            2,
            "",
            &running,
            None,
        )
        .await;
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
        assert_eq!(req.systems, vec!["x86_64-linux"]);
        assert_eq!(req.max_builds, 2);
        // No cache → no bloom filter.
        assert!(req.local_paths.is_none());
    }

    #[tokio::test]
    async fn test_heartbeat_empty_running_builds() {
        let running = Arc::new(RwLock::new(HashSet::new()));
        let req = build_heartbeat_request(
            "worker-1",
            &["x86_64-linux".into()],
            &[],
            1,
            "",
            &running,
            None,
        )
        .await;
        assert!(req.running_builds.is_empty());
    }

    /// size_class passes through verbatim (scheduler interprets "" as None).
    #[tokio::test]
    async fn test_heartbeat_includes_size_class() {
        let running = Arc::new(RwLock::new(HashSet::new()));
        let req = build_heartbeat_request(
            "worker-1",
            &["x86_64-linux".into()],
            &[],
            1,
            "large",
            &running,
            None,
        )
        .await;
        assert_eq!(req.size_class, "large");

        let req = build_heartbeat_request(
            "worker-1",
            &["x86_64-linux".into()],
            &[],
            1,
            "",
            &running,
            None,
        )
        .await;
        assert_eq!(req.size_class, "", "empty = unclassified");
    }

    /// Regression: supported_features must reflect the config — if
    /// hardcoded empty, the CRD's features field is silently ignored
    /// and any derivation with requiredSystemFeatures never dispatches.
    #[tokio::test]
    async fn test_heartbeat_includes_systems_and_features() {
        let running = Arc::new(RwLock::new(HashSet::new()));
        let req = build_heartbeat_request(
            "worker-1",
            &["x86_64-linux".into(), "aarch64-linux".into()],
            &["kvm".into(), "big-parallel".into()],
            1,
            "",
            &running,
            None,
        )
        .await;
        assert_eq!(req.systems, vec!["x86_64-linux", "aarch64-linux"]);
        assert_eq!(req.supported_features, vec!["kvm", "big-parallel"]);
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
        let req = build_heartbeat_request(
            "worker-1",
            &["x86_64-linux".into()],
            &[],
            1,
            "",
            &running,
            Some(&bloom),
        )
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
