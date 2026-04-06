//! Worker runtime: heartbeat construction and build-task spawning.
//!
//! Extracted from lib.rs — this is the glue between main.rs's event loop
//! and the executor/FUSE/upload subsystems. `build_heartbeat_request`
//! snapshots the FUSE cache bloom filter; `spawn_build_task` wraps
//! `executor::execute_build` with ACK + CompletionReport + panic-catcher.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

use tokio::sync::{RwLock, Semaphore, mpsc};
use tonic::transport::Channel;

use rio_proto::StoreServiceClient;
use rio_proto::types::{
    CompletionReport, HeartbeatRequest, PrefetchHint, ProgressUpdate, ResourceUsage,
    WorkAssignment, WorkAssignmentAck, WorkerMessage, worker_message,
};

use tracing::{Instrument, instrument};

use crate::{executor, fuse, log_stream};

use crate::cgroup::ResourceSnapshotHandle;

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

/// Generation fence: should this assignment be rejected as stale?
///
/// Separate from the event-loop handler for testability — main.rs's
/// event loop has no mock SchedulerMessage stream. Strictly-less (`<`):
/// generation is constant during a leader's tenure, so `assignment_gen
/// == latest_observed` is the steady state.
// r[impl sched.lease.generation-fence]
pub fn is_stale_assignment(assignment_gen: u64, latest_observed: u64) -> bool {
    assignment_gen < latest_observed
}

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
// 8 args: all distinct worker-identity/state fields. A struct would
// just move the same 8 lines to the call site.
#[allow(clippy::too_many_arguments)]
pub async fn build_heartbeat_request(
    worker_id: &str,
    systems: &[String],
    features: &[String],
    max_builds: u32,
    size_class: &str,
    running: &RwLock<HashSet<String>>,
    bloom: Option<&BloomHandle>,
    resources: &ResourceSnapshotHandle,
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

    // Snapshot is Copy; the read lock is held for one struct load.
    // First heartbeat (before first 10s poll) sends zeros — same
    // as the old ResourceUsage::default(), converges after one tick.
    // Override running_builds/available_build_slots here: the cgroup
    // sampler doesn't know max_builds. These are redundant with the
    // top-level HeartbeatRequest fields but filling them keeps the
    // ResourceUsage message self-contained for ListWorkers consumers.
    let running_count = current.len() as u32;
    let resources = {
        let snap = *resources.read().unwrap_or_else(|e| e.into_inner());
        let mut ru = snap.to_proto();
        ru.running_builds = running_count;
        ru.available_build_slots = max_builds.saturating_sub(running_count);
        ru
    };

    HeartbeatRequest {
        worker_id: worker_id.to_string(),
        running_builds: current,
        resources: Some(resources),
        local_paths,
        systems: systems.to_vec(),
        supported_features: features.to_vec(),
        max_builds,
        // Empty string = unclassified (scheduler maps to None). We
        // pass through verbatim — the worker doesn't interpret it,
        // just declares what the operator configured.
        size_class: size_class.to_string(),
        // TODO(P0210): replace with circuit.is_open() once the FUSE
        // circuit breaker lands. Explicit `false` (not ..Default::default())
        // so P0210 has a clean one-line rebase target.
        store_degraded: false,
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
    /// Silence timeout default (from `Config.max_silent_time_secs`).
    /// Used when WorkAssignment's BuildOptions.max_silent_time is 0.
    /// 0 = disabled.
    pub max_silent_time: u64,
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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Cgroup doesn't exist — cancel arrived before execute_build
            // reached BuildCgroup::create (overlay setup + daemon spawn
            // window). The kill DID NOT HAPPEN. Undo the flag so an
            // unrelated executor Err later isn't misclassified as
            // Cancelled (operator would see "cancelled", never
            // investigate the real fault).
            //
            // The cancel itself is lost — see worker.md:361, scheduler's
            // backstop timeout is the safety net. We could stash a
            // "deferred cancel" and have execute_build check it post-
            // cgroup-create, but the window is narrow and the backstop
            // already covers it.
            //
            // r[impl worker.cancel.flag-clear-enoent]
            cancelled.store(false, std::sync::atomic::Ordering::Release);
            tracing::debug!(
                drv_path,
                cgroup = %cgroup_path.display(),
                "cancel: cgroup not yet created (early-arrival race); flag cleared"
            );
            false
        }
        Err(e) => {
            // EACCES (delegation broken?) / EINVAL (kernel < 5.14?).
            // We don't know if the kill landed. Leave the flag set —
            // if the build IS still running and later errs, we'll
            // misclassify as Cancelled, but that's less bad than the
            // reverse (kill DID land, we clear flag, build errs from
            // the kill, we report InfrastructureFailure → reassign).
            tracing::warn!(drv_path, error = %e, "cgroup.kill failed (non-ENOENT); flag left set");
            true
        }
    }
}

/// Proactive-ema resource tick interval. 10s matches HEARTBEAT_INTERVAL
/// — frequent enough that a build trending toward OOM is noticed within
/// one tick, coarse enough that the WorkerMessage stream isn't flooded
/// (a 30-min build = 180 messages, dwarfed by log batches).
const RESOURCE_TICK: Duration = Duration::from_secs(10);

/// Wrap a build future with a 10s resource tick: samples the per-build
/// cgroup's `memory.peak` and emits `ProgressUpdate{resources}` to the
/// scheduler. Proactive ema — scheduler updates `ema_peak_memory_bytes`
/// BEFORE completion so the NEXT submit of that drv is right-sized
/// immediately, instead of after a full OOM→retry cycle.
///
/// `r[sched.preempt.never-running]` stands: this NEVER kills. The
/// scheduler's response to these samples is to update an ema, not
/// to cancel. A build trending toward OOM still OOMs — but the
/// retry gets a bigger worker on the first attempt.
///
/// `memory.peak` is kernel-tracked cumulative-max: monotone, exact,
/// zero-polling-overhead (kernel updates on every alloc). Reading it
/// mid-build gives "peak so far" — exactly what the ema update wants.
/// The cgroup may not exist yet (executor creates it after spawning
/// the daemon); ENOENT → skip the tick, try again next interval.
///
/// Generic over the future's output so tests can pass a simple
/// `sleep(35s)` instead of mocking all of `execute_build`.
async fn run_with_resource_tick<F, T>(
    build: F,
    cgroup_path: &Path,
    drv_path: &str,
    tx: &mpsc::Sender<WorkerMessage>,
) -> T
where
    F: Future<Output = T>,
{
    // pin! not Box::pin: the future is stack-local, no heap needed.
    // The select! arm re-borrows &mut on each iteration.
    let mut build = std::pin::pin!(build);

    // interval_at(now + period) so the first tick is at t=10s, not
    // t=0. A t=0 tick would sample before the cgroup exists (executor
    // creates it post-daemon-spawn) — harmless (skip-on-ENOENT) but
    // noise. Delay-on-missed: under load, don't burst-emit stale
    // samples; one tick per period is the contract.
    let start = tokio::time::Instant::now() + RESOURCE_TICK;
    let mut tick = tokio::time::interval_at(start, RESOURCE_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let peak_file = cgroup_path.join("memory.peak");
    loop {
        tokio::select! {
            // biased: poll the build first. If the build completed in
            // the same wakeup that the tick fired, we want to break
            // and send CompletionReport (which carries the FINAL
            // memory.peak), not emit a redundant mid-build sample.
            biased;
            result = &mut build => break result,
            _ = tick.tick() => {
                // Inline read_single_u64 — it's crate-private in cgroup.rs
                // and it's one line. memory.peak format: "12345\n".
                // ENOENT (cgroup not created yet) / parse-fail → None →
                // skip. The cgroup appears mid-build; first few ticks
                // may be skipped on a slow daemon-spawn.
                let Some(peak) = std::fs::read_to_string(&peak_file)
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok())
                else {
                    continue;
                };
                // Only memory_used_bytes populated — that's the ema
                // input. cpu_fraction would need delta-polling (two
                // reads + elapsed); not worth it for proactive-ema
                // (scheduler tracks ema_peak_memory_bytes, not CPU).
                // Everything else ..Default = 0 = "not sampled."
                //
                // try_send: if the stream is backpressured (log batch
                // flood during a chatty build), drop the sample. Same
                // policy as ForwardLogBatch — this is advisory. Next
                // tick re-samples memory.peak (monotone, so no info
                // lost). send().await would block the select loop →
                // the BUILD future wouldn't be polled → build stalls.
                let _ = tx.try_send(WorkerMessage {
                    msg: Some(worker_message::Msg::Progress(ProgressUpdate {
                        drv_path: drv_path.to_string(),
                        resources: Some(ResourceUsage {
                            memory_used_bytes: peak,
                            ..Default::default()
                        }),
                        build_phase: String::new(),
                    })),
                });
            }
        }
    }
}

/// Handle a WorkAssignment: ACK the scheduler, spawn the build task, set up
/// a panic-catcher.
///
/// Returns after spawning — does NOT block on build completion. The build runs
/// in its own tokio task holding `permit`; it reports completion via
/// `ctx.stream_tx` and drops the permit on exit (success, failure, or panic).
///
/// Ephemeral mode (`RIO_EPHEMERAL` env, set by the controller's
/// `ephemeral::build_job` on Job pods — see `r[ctrl.pool.ephemeral]`):
/// main.rs's event loop spawns a watcher AFTER calling this that waits
/// for the permit to return, then signals exit. This function is
/// unchanged — the permit-drop on completion IS the signal. The
/// single-shot gate is in main.rs's select! loop, not here; putting it
/// here would mean every `spawn_build_task` caller (including tests)
/// would need to handle the ephemeral branch.
#[instrument(skip_all, fields(drv_path = %assignment.drv_path))]
pub async fn spawn_build_task(
    assignment: WorkAssignment,
    permit: tokio::sync::OwnedSemaphorePermit,
    ctx: &BuildSpawnContext,
) {
    let drv_path = assignment.drv_path.clone();
    let assignment_token = assignment.assignment_token.clone();
    let traceparent = assignment.traceparent.clone();

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
    // Cloned for the resource tick sampler before moving into the
    // cancel registry. Same deterministic path execute_build creates.
    let build_cgroup_path = cgroup_path.clone();
    ctx.cancel_registry
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(drv_path.clone(), (cgroup_path, Arc::clone(&cancelled)));

    // Clone state needed by spawned tasks ('static lifetime).
    let mut build_store_client = ctx.store_client.clone();
    let build_tx = ctx.stream_tx.clone();
    let build_running = Arc::clone(&ctx.running_builds);
    let build_leaked_mounts = Arc::clone(&ctx.leaked_mounts);
    let build_drv_path = drv_path.clone();
    let build_cancel_registry = Arc::clone(&ctx.cancel_registry);
    let build_cancelled = cancelled;
    let build_env = executor::ExecutorEnv {
        fuse_mount_point: ctx.fuse_mount_point.clone(),
        overlay_base_dir: ctx.overlay_base_dir.clone(),
        worker_id: ctx.worker_id.clone(),
        log_limits: ctx.log_limits,
        max_leaked_mounts: ctx.max_leaked_mounts,
        daemon_timeout: ctx.daemon_timeout,
        max_silent_time: ctx.max_silent_time,
        cgroup_parent: ctx.cgroup_parent.clone(),
        fod_proxy_url: ctx.fod_proxy_url.clone(),
    };

    // Clone for the panic handler before moving into the task.
    let panic_tx = ctx.stream_tx.clone();
    let panic_drv_path = drv_path.clone();
    let panic_token = assignment_token.clone();

    // r[impl sched.trace.assignment-traceparent]
    // Parent the spawned task's span by the traceparent from the assignment.
    // Closes the SSH-boundary tracing gap: scheduler injects its span's W3C
    // traceparent into the payload; we extract it here. Empty → fresh root.
    let build_span = rio_proto::interceptor::span_from_traceparent("build_executor", &traceparent);
    let executor_future = async move {
        let _permit = permit; // Hold permit until build completes

        // Remove from running_builds + cancel_registry on task exit
        // (success, failure, panic, or cancellation). Same lifetime
        // for both — they track "this build is in-flight."
        let cleanup_drv_path = build_drv_path.clone();
        let _running_guard = scopeguard::guard((), move |()| {
            let rb = Arc::clone(&build_running);
            let reg = Arc::clone(&build_cancel_registry);
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

        // Proactive-ema wrap: 10s memory.peak samples flow to the
        // scheduler while the build runs. execute_build is the polled
        // future; run_with_resource_tick drives the select! loop.
        let result = run_with_resource_tick(
            executor::execute_build(
                &assignment,
                &build_env,
                &mut build_store_client,
                &build_tx,
                &build_leaked_mounts,
            ),
            &build_cgroup_path,
            &build_drv_path,
            &build_tx,
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
            Some(r) => match rio_proto::types::BuildResultStatus::try_from(r.status) {
                Ok(rio_proto::types::BuildResultStatus::Built) => "success",
                Ok(rio_proto::types::BuildResultStatus::Cancelled) => "cancelled",
                // Operationally distinct: means "raise the limit," not
                // "the build is broken." Separate label so SLI queries
                // can exclude these from failure-rate denominators.
                Ok(rio_proto::types::BuildResultStatus::TimedOut) => "timed_out",
                Ok(rio_proto::types::BuildResultStatus::LogLimitExceeded) => "log_limit",
                Ok(rio_proto::types::BuildResultStatus::InfrastructureFailure) => "infra_failure",
                _ => "failure",
            },
            None => "failure",
        };
        metrics::counter!("rio_worker_builds_total", "outcome" => outcome).increment(1);

        let msg = WorkerMessage {
            msg: Some(worker_message::Msg::Completion(completion)),
        };
        if let Err(e) = build_tx.send(msg).await {
            tracing::error!(error = %e, "failed to send completion report");
        }
    };
    let handle =
        rio_common::task::spawn_monitored("build-executor", executor_future.instrument(build_span));

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
#[instrument(skip_all, fields(count = prefetch.store_paths.len()))]
pub fn handle_prefetch_hint(
    prefetch: PrefetchHint,
    cache: Arc<fuse::cache::Cache>,
    store_client: StoreServiceClient<Channel>,
    rt: tokio::runtime::Handle,
    sem: Arc<Semaphore>,
    fetch_timeout: std::time::Duration,
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
                match prefetch_path_blocking(&cache, &client, &rt, fetch_timeout, &basename) {
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
            (cgroup_path.clone(), Arc::clone(&cancelled)),
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

    /// Cancel arrives before cgroup exists → kill ENOENT → flag cleared.
    /// An unrelated Err later must NOT be misclassified as Cancelled.
    ///
    /// This REPLACES the old behavior test (flag stayed set on ENOENT):
    /// that was the `wkr-cancel-flag-stale` bug — a stale flag would
    /// misclassify a later overlay-teardown/daemon-spawn error as
    /// Cancelled, hiding the real infrastructure fault.
    ///
    // r[verify worker.cancel.flag-clear-enoent]
    #[test]
    fn cancel_build_cgroup_missing_clears_flag() {
        let cancelled = Arc::new(AtomicBool::new(false));
        // Path that definitely doesn't exist. tmpdir/nonexistent so
        // the test doesn't depend on /sys/fs/cgroup being mounted (CI
        // sandbox may not have cgroup v2).
        let tmp = tempfile::tempdir().unwrap();
        let fake_cgroup = tmp.path().join("not-created-yet");
        let registry = std::sync::RwLock::new(HashMap::from([(
            "/nix/store/test.drv".to_string(),
            (fake_cgroup, Arc::clone(&cancelled)),
        )]));

        let got = try_cancel_build(&registry, "/nix/store/test.drv");

        // Kill was a no-op (ENOENT) → cancel did NOT happen → false.
        assert!(!got, "ENOENT cancel should return false (nothing killed)");
        // Load-bearing: flag must be FALSE so a later Err is correctly
        // classified as InfrastructureFailure, not Cancelled.
        assert!(
            !cancelled.load(std::sync::atomic::Ordering::Acquire),
            "flag must be cleared on ENOENT; otherwise unrelated Err → misclassified as Cancelled"
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
            &ResourceSnapshotHandle::default(),
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
            &ResourceSnapshotHandle::default(),
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
            &ResourceSnapshotHandle::default(),
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
            &ResourceSnapshotHandle::default(),
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
            &ResourceSnapshotHandle::default(),
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
            &ResourceSnapshotHandle::default(),
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

#[cfg(test)]
mod resource_tick_tests {
    use super::*;

    /// Extract memory_used_bytes from Progress messages; filter the rest.
    fn progress_peaks(rx: &mut mpsc::Receiver<WorkerMessage>) -> Vec<u64> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            if let Some(worker_message::Msg::Progress(p)) = m.msg {
                out.push(p.resources.map(|r| r.memory_used_bytes).unwrap_or(0));
            }
        }
        out
    }

    /// start_paused = true: tokio's clock is frozen. Time advances
    /// only when all tasks are idle (auto-advance). A 35s sleep as
    /// the "build" + a 10s tick → ticks fire at t=10, t=20, t=30;
    /// the build completes at t=35 before t=40 fires. 3 samples.
    ///
    /// Auto-advance drives this without manual `advance()` calls:
    /// select! polls both arms, both pending → runtime auto-advances
    /// to the nearest timer (t=10 tick), tick fires, loop, repeat.
    /// The blocking fs::read_to_string is tmpfs (microseconds), not
    /// enough to confuse paused-time.
    #[tokio::test(start_paused = true)]
    async fn resource_usage_emitted_every_10s() {
        let cgroup = tempfile::tempdir().unwrap();
        // memory.peak exists from t=0 in this test (real executor
        // creates it mid-build — cgroup_missing_skips_tick covers that).
        std::fs::write(cgroup.path().join("memory.peak"), "4096\n").unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        let build = tokio::time::sleep(Duration::from_secs(35));

        run_with_resource_tick(build, cgroup.path(), "/nix/store/test.drv", &tx).await;

        let peaks = progress_peaks(&mut rx);
        assert_eq!(
            peaks.len(),
            3,
            "35s build / 10s tick → samples at t=10,20,30 (t=40 never fires)"
        );
        assert!(
            peaks.iter().all(|&p| p == 4096),
            "all samples read the 4096 fixture: {peaks:?}"
        );
    }

    /// Build shorter than one tick → zero emissions. Exercises the
    /// `biased; build-first` ordering: even if the build completes
    /// at an instant where auto-advance COULD fire the tick, the
    /// build arm wins and we break.
    #[tokio::test(start_paused = true)]
    async fn short_build_emits_nothing() {
        let cgroup = tempfile::tempdir().unwrap();
        std::fs::write(cgroup.path().join("memory.peak"), "999\n").unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        // 5s < 10s first tick.
        let build = tokio::time::sleep(Duration::from_secs(5));

        run_with_resource_tick(build, cgroup.path(), "/nix/store/fast.drv", &tx).await;

        assert!(
            progress_peaks(&mut rx).is_empty(),
            "sub-10s build → interval_at(now+10s) never fires"
        );
    }

    /// cgroup doesn't exist yet (executor creates it post-daemon-spawn).
    /// Ticks still fire on schedule; ENOENT → skip, no message. When
    /// memory.peak appears mid-build, later ticks emit.
    #[tokio::test(start_paused = true)]
    async fn cgroup_missing_skips_tick() {
        let cgroup = tempfile::tempdir().unwrap();
        // memory.peak NOT written — ENOENT on every tick.
        let (tx, mut rx) = mpsc::channel(16);
        let build = tokio::time::sleep(Duration::from_secs(25));

        run_with_resource_tick(build, cgroup.path(), "/nix/store/test.drv", &tx).await;

        assert!(
            progress_peaks(&mut rx).is_empty(),
            "ENOENT on memory.peak → skip every tick, never emit"
        );
    }

    /// The wrapped future's output is returned unchanged. Proves the
    /// select! break-arm plumbs through without eating the result.
    #[tokio::test(start_paused = true)]
    async fn build_result_passes_through() {
        let cgroup = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::channel(16);

        let build = async {
            tokio::time::sleep(Duration::from_secs(15)).await;
            42u64
        };

        let result = run_with_resource_tick(build, cgroup.path(), "/nix/store/x.drv", &tx).await;
        assert_eq!(result, 42, "select! break arm returns the build's output");
    }

    /// drv_path is plumbed into the emitted ProgressUpdate — scheduler
    /// needs this to key the ema update by derivation hash.
    #[tokio::test(start_paused = true)]
    async fn drv_path_populated() {
        let cgroup = tempfile::tempdir().unwrap();
        std::fs::write(cgroup.path().join("memory.peak"), "1\n").unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        let build = tokio::time::sleep(Duration::from_secs(12));

        run_with_resource_tick(build, cgroup.path(), "/nix/store/abc-foo.drv", &tx).await;

        let msg = rx.try_recv().expect("one tick at t=10");
        let Some(worker_message::Msg::Progress(p)) = msg.msg else {
            panic!("expected Progress, got {msg:?}");
        };
        assert_eq!(p.drv_path, "/nix/store/abc-foo.drv");
        // Precondition self-assert: resources IS Some. If the impl
        // ever regresses to Default::default() on the whole message,
        // this catches it before the scheduler-side P0266 test does.
        assert!(p.resources.is_some(), "resources must be populated");
    }
}

// r[verify sched.lease.generation-fence]
#[cfg(test)]
mod fence_tests {
    use super::is_stale_assignment;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn fence_rejects_strictly_less() {
        // Deposed leader: its BuildExecution stream stayed open past
        // the lease loss; assignment carries gen=1, we've seen gen=2.
        assert!(is_stale_assignment(1, 2));
    }

    #[test]
    fn fence_accepts_equal() {
        // Steady state: generation is constant per leader tenure.
        // Rejecting equal would reject every normal assignment.
        assert!(!is_stale_assignment(2, 2));
    }

    #[test]
    fn fence_accepts_greater() {
        // Assignment from a generation we haven't heartbeat-observed
        // yet. Heartbeat interval is 10s; an assignment CAN arrive
        // first after failover. No evidence of staleness → accept.
        // Rejecting would stall post-failover dispatch.
        assert!(!is_stale_assignment(3, 2));
    }

    #[test]
    fn fence_cold_start_accepts() {
        // latest_observed starts at 0 (before first heartbeat).
        // Scheduler generation is always ≥1 (lease/mod.rs starts at 1
        // for non-K8s; k8s lease increments from 1 on first acquire).
        // 1 < 0 is false → not rejected. Correct: no evidence yet.
        assert!(!is_stale_assignment(1, 0));
    }

    /// fetch_max monotonicity: during the 15s Lease TTL split-brain
    /// window, both old and new leader answer heartbeats with
    /// accepted=true. If responses interleave new-then-old, `store`
    /// would REGRESS the fence. `fetch_max` is monotone.
    #[test]
    fn heartbeat_gen_monotone_under_interleaving() {
        let g = AtomicU64::new(0);
        g.fetch_max(3, Ordering::Relaxed); // new leader's heartbeat lands first
        g.fetch_max(1, Ordering::Relaxed); // stale leader's heartbeat lands second
        assert_eq!(
            g.load(Ordering::Relaxed),
            3,
            "fence must not regress under out-of-order heartbeat responses"
        );
    }
}
