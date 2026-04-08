//! Worker runtime: heartbeat construction and build-task spawning.
//!
//! Extracted from lib.rs — this is the glue between main.rs's event loop
//! and the executor/FUSE/upload subsystems. `build_heartbeat_request`
//! assembles capability/resource data; `spawn_build_task` wraps
//! `executor::execute_build` with ACK + CompletionReport + panic-catcher.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio::sync::{Notify, Semaphore, mpsc};

use rio_proto::types::{
    CompletionReport, ExecutorMessage, HeartbeatRequest, PrefetchComplete, PrefetchHint,
    ProgressUpdate, ResourceUsage, WorkAssignment, WorkAssignmentAck, executor_message,
};

use tracing::{Instrument, instrument};

use crate::{executor, fuse, log_stream};

use crate::cgroup::ResourceSnapshotHandle;

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

/// Single-build occupancy. P0537: one build per pod, no concurrency
/// knob. Replaces the old `Semaphore::new(1)` + `RwLock<HashSet<String>>`
/// pair — both "is a build running?" and "which drv_path?" live here.
///
/// `try_claim` is non-blocking by design: with one build per pod, the
/// scheduler shouldn't dispatch while busy (heartbeat reports
/// `running_builds`). An assignment arriving while busy is a scheduler
/// bug; the old `acquire_owned().await` would have queued it locally,
/// silently defeating capacity reporting.
#[derive(Default)]
pub struct BuildSlot {
    /// `Some(drv_path)` while a build is in-flight. `Mutex` not
    /// `RwLock`: with one build, contention is impossible (claim/release
    /// are serial; heartbeat reads every 10s).
    running: std::sync::Mutex<Option<String>>,
    /// Notified on release. `notify_waiters` (not `_one`): the drain
    /// watcher and the build-done watcher may both be parked at once
    /// (SIGTERM during the build).
    idle: Notify,
    /// Cancel target for the running build: (cgroup path for
    /// `cgroup.kill`, cancelled flag). Populated by `spawn_build_task`
    /// PREDICTIVELY (before the cgroup is created); read by
    /// [`try_cancel_build`]. The flag is the same `Arc` threaded into
    /// `ExecutorEnv.cancelled` and the spawned task's Err-classifier.
    ///
    /// `Mutex` like `running`: the Cancel handler reads once;
    /// `spawn_build_task` writes once. Cleared by `BuildSlotGuard::drop`
    /// (same lifetime as `running`).
    cancel: std::sync::Mutex<Option<(PathBuf, Arc<AtomicBool>)>>,
}

impl BuildSlot {
    /// Claim the slot for `drv_path`. Returns `None` if already busy
    /// (caller logs and rejects the assignment — see struct doc).
    pub fn try_claim(self: &Arc<Self>, drv_path: &str) -> Option<BuildSlotGuard> {
        let mut slot = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return None;
        }
        *slot = Some(drv_path.to_string());
        Some(BuildSlotGuard(Arc::clone(self)))
    }

    /// Current in-flight drv_path, for heartbeat `running_builds`.
    pub fn running(&self) -> Option<String> {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn is_busy(&self) -> bool {
        self.running().is_some()
    }

    /// Record the cancel target for the running build. See field doc.
    pub fn set_cancel_target(&self, cgroup_path: PathBuf, cancelled: Arc<AtomicBool>) {
        *self.cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some((cgroup_path, cancelled));
    }

    /// Park until the slot is idle. Missed-notification-safe: the
    /// `notified()` future is registered BEFORE checking `is_busy()`
    /// (Notify's documented `enable()` pattern), so a release between
    /// the check and the await still wakes us.
    pub async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            // Register interest before the load — see tokio::sync::Notify
            // docs §"Avoiding missed notifications".
            notified.as_mut().enable();
            if !self.is_busy() {
                return;
            }
            notified.await;
        }
    }
}

/// RAII release: `Drop` clears the slot and wakes any `wait_idle()`
/// callers. Held inside [`spawn_build_task`]'s spawned future (same
/// lifetime the old `OwnedSemaphorePermit` had).
pub struct BuildSlotGuard(Arc<BuildSlot>);

impl Drop for BuildSlotGuard {
    fn drop(&mut self) {
        *self.0.running.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.0.cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.0.idle.notify_waiters();
    }
}

/// Build a heartbeat request, populating `running_builds` from the shared
/// tracker.
///
/// Extracted for testability — the heartbeat loop in main.rs calls this.
///
/// `systems` + `features`: slice refs to the worker's static config.
/// Both are `.to_vec()`'d into the proto — a heartbeat every 10s
/// means ~100 allocs/min for typically 1-3 elements; not worth the
/// lifetime-threading to avoid.
#[allow(clippy::too_many_arguments)]
pub async fn build_heartbeat_request(
    executor_id: &str,
    executor_kind: rio_proto::types::ExecutorKind,
    systems: &[String],
    features: &[String],
    size_class: &str,
    slot: &BuildSlot,
    resources: &ResourceSnapshotHandle,
    store_degraded: bool,
    draining: bool,
) -> HeartbeatRequest {
    let current: Vec<String> = slot.running().into_iter().collect();

    // Snapshot is Copy; the read lock is held for one struct load.
    // First heartbeat (before first 10s poll) sends zeros — same
    // as the old ResourceUsage::default(), converges after one tick.
    // Override running_builds here: the cgroup sampler doesn't know
    // the running set. Redundant with the top-level HeartbeatRequest
    // field but filling it keeps the ResourceUsage message self-
    // contained for ListExecutors consumers.
    let running_count = current.len() as u32;
    let resources = {
        let snap = *resources.read().unwrap_or_else(|e| e.into_inner());
        let mut ru = snap.to_proto();
        ru.running_builds = running_count;
        ru
    };

    HeartbeatRequest {
        executor_id: executor_id.to_string(),
        running_builds: current,
        resources: Some(resources),
        systems: systems.to_vec(),
        supported_features: features.to_vec(),
        // Empty string = unclassified (scheduler maps to None). We
        // pass through verbatim — the worker doesn't interpret it,
        // just declares what the operator configured.
        size_class: size_class.to_string(),
        // r[impl builder.heartbeat.store-degraded]
        // Reflects CircuitBreaker::is_open() — main.rs reads the
        // breaker each heartbeat tick and passes it here. Scheduler
        // treats this like `draining`: has_capacity() returns false
        // while the FUSE fetch circuit is open (store unreachable
        // or degraded). Half-open counts as NOT degraded (the probe
        // is in flight, let it decide).
        store_degraded,
        // I-063: the worker is the authority on its own drain state.
        // main.rs flips this on first SIGTERM and keeps the stream
        // alive for completion reports; scheduler trusts this over
        // DrainExecutor RPC or reconnect inference.
        draining,
        // Builder or Fetcher — from `RIO_EXECUTOR_KIND` config.
        // Scheduler routes FODs to fetchers only per
        // spec sched.dispatch.fod-to-fetcher.
        kind: executor_kind as i32,
    }
}

/// Shared context for spawning build tasks.
///
/// Constructed once before the event loop to reduce per-assignment clone
/// boilerplate. `spawn_build_task` clones only what each spawned task needs.
#[derive(Clone)]
pub struct BuildSpawnContext {
    /// `StoreService` + `ChunkService` over the same balanced channel.
    /// `.store` goes to `execute_build` (drv fetch, upload, query);
    /// the full bundle is held by `NixStoreFs` (set at FUSE mount) so
    /// the chunk-fanout transport (dataplane2) is reachable from the
    /// JIT `lookup` callback.
    pub store_clients: crate::fuse::StoreClients,
    pub executor_id: String,
    pub fuse_mount_point: PathBuf,
    pub overlay_base_dir: PathBuf,
    pub stream_tx: mpsc::Sender<ExecutorMessage>,
    /// Single-build occupancy. Heartbeat reads `slot.running()`;
    /// main.rs's assignment handler `try_claim`s before calling
    /// [`spawn_build_task`].
    pub slot: Arc<BuildSlot>,
    /// Per-build log rate/size limits. `Copy`, so cloning into each spawned
    /// task is cheap. Worker-wide (set once at startup from config), not
    /// per-assignment — the limits are a worker policy, not a build option.
    pub log_limits: log_stream::LogLimits,
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
    /// Builder or Fetcher (from `Config.executor_kind`). Threaded into
    /// each spawned task's `ExecutorEnv` for the wrong-kind gate.
    pub executor_kind: rio_proto::types::ExecutorKind,
    /// Handle to the FUSE local cache. Threaded into `ExecutorEnv` so
    /// the executor can `register_inputs` (JIT allowlist) and
    /// `prefetch_manifests` (I-110c) before daemon spawn.
    pub fuse_cache: Arc<crate::fuse::cache::Cache>,
    /// Base per-fetch gRPC timeout for the FUSE cache's `GetPath`.
    /// JIT lookup scales it per path via `jit_fetch_timeout(this,
    /// nar_size)` (I-178). Same value passed to
    /// [`handle_prefetch_hint`].
    pub fuse_fetch_timeout: Duration,
}

/// Attempt to cancel the running build. Checks the slot's running
/// drv_path matches, sets the cancel flag, writes cgroup.kill.
///
/// Returns `true` if the build was found and kill was attempted
/// (kill may still fail if the cgroup was already removed — we
/// log and consider it "cancelled anyway"). `false` if not found
/// (build already finished, or the cancel is for a different drv —
/// stale CancelSignal from a previous scheduler generation).
///
/// Called from main.rs's `Msg::Cancel` handler. Fire-and-forget:
/// the scheduler doesn't wait for confirmation (it's already
/// transitioned the derivation to Cancelled on its side — this
/// is just cleanup).
pub fn try_cancel_build(slot: &BuildSlot, drv_path: &str) -> bool {
    // drv_path match: with one build per pod the slot holds 0-or-1
    // entry; the drv_path check guards against a stale CancelSignal
    // (scheduler restarted and re-sent for a build this pod never had).
    if slot.running().as_deref() != Some(drv_path) {
        tracing::debug!(
            drv_path,
            "cancel: build not in slot (finished or never started)"
        );
        return false;
    }
    let guard = slot.cancel.lock().unwrap_or_else(|e| e.into_inner());
    let Some((cgroup_path, cancelled)) = guard.as_ref() else {
        // Slot claimed but cancel target not yet set — spawn_build_task
        // hasn't reached set_cancel_target. Same as ENOENT below: return
        // false; the scheduler will re-send.
        tracing::debug!(
            drv_path,
            "cancel: slot claimed but cancel target not yet set"
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
            // reached BuildCgroup::create. Under JIT fetch this window
            // is overlay → resolve → prepare_sandbox → register +
            // prefetch (sub-second; the I-165 47-min warm stall is
            // gone), so the cancel-poll select was removed.
            //
            // r[impl builder.cancel.pre-cgroup-deferred]
            // LEAVE THE FLAG SET. execute_build checks it before the
            // register+prefetch phase and aborts with
            // `ExecutorError::Cancelled` without spawning the daemon.
            // The misclassification risk (an unrelated Err later
            // reported as Cancelled) is real but is the lesser evil:
            // a build that the scheduler already transitioned to
            // Cancelled has no client waiting on its real outcome,
            // and an unkillable builder burns activeDeadlineSeconds
            // (1h) of compute × N pods (I-166: ×86).
            tracing::info!(
                drv_path,
                cgroup = %cgroup_path.display(),
                "cancel: cgroup not yet created; flag left set for pre-cgroup poll"
            );
            true
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
/// one tick, coarse enough that the ExecutorMessage stream isn't flooded
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
    tx: &mpsc::Sender<ExecutorMessage>,
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
                let _ = tx.try_send(ExecutorMessage {
                    msg: Some(executor_message::Msg::Progress(ProgressUpdate {
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
/// `ctx.stream_tx` and drops the slot guard on exit (success, failure,
/// or panic).
///
/// main.rs's event loop spawns a watcher AFTER calling this that
/// `slot.wait_idle()`s, then signals exit. The guard-drop on completion
/// IS the signal. The single-shot gate is in main.rs's select! loop,
/// not here.
#[instrument(skip_all, fields(drv_path = %assignment.drv_path))]
pub async fn spawn_build_task(
    assignment: WorkAssignment,
    guard: BuildSlotGuard,
    ctx: &BuildSpawnContext,
) {
    let drv_path = assignment.drv_path.clone();
    let assignment_token = assignment.assignment_token.clone();
    let traceparent = assignment.traceparent.clone();

    // Send ACK
    let ack = ExecutorMessage {
        msg: Some(executor_message::Msg::Ack(WorkAssignmentAck {
            drv_path: drv_path.clone(),
            assignment_token: assignment_token.clone(),
        })),
    };
    if let Err(e) = ctx.stream_tx.send(ack).await {
        tracing::error!(error = %e, "failed to send ACK");
        return; // Guard drops, no build spawned.
    }

    // Record the cancel target on the slot. We know the cgroup path
    // deterministically: cgroup_parent/sanitize_build_id(drv_path).
    // execute_build creates this AFTER spawning the daemon (needs
    // PID); we record PREDICTIVELY here so a Cancel arriving early
    // still finds it. If Cancel arrives BEFORE the cgroup exists,
    // cgroup.kill → ENOENT → try_cancel_build leaves the flag SET;
    // execute_build polls it during prefetch+warm and aborts
    // pre-daemon-spawn (I-166).
    //
    // The cancelled flag: set by try_cancel_build BEFORE killing.
    // Read below in the Err arm to distinguish "cancelled" (user
    // intent, Cancelled status) from "executor failed" (infra issue,
    // InfrastructureFailure status).
    let build_id = executor::sanitize_build_id(&drv_path);
    let cgroup_path = ctx.cgroup_parent.join(&build_id);
    let cancelled = Arc::new(AtomicBool::new(false));
    // Cloned for the resource tick sampler before moving into the
    // slot's cancel target. Same deterministic path execute_build creates.
    let build_cgroup_path = cgroup_path.clone();
    ctx.slot
        .set_cancel_target(cgroup_path, Arc::clone(&cancelled));

    // Clone state needed by spawned tasks ('static lifetime).
    let mut build_store_client = ctx.store_clients.store.clone();
    let build_tx = ctx.stream_tx.clone();
    let build_drv_path = drv_path.clone();
    let build_cancelled = cancelled;
    let build_env = executor::ExecutorEnv {
        fuse_mount_point: ctx.fuse_mount_point.clone(),
        overlay_base_dir: ctx.overlay_base_dir.clone(),
        executor_id: ctx.executor_id.clone(),
        log_limits: ctx.log_limits,
        daemon_timeout: ctx.daemon_timeout,
        max_silent_time: ctx.max_silent_time,
        cgroup_parent: ctx.cgroup_parent.clone(),
        executor_kind: ctx.executor_kind,
        fuse_cache: Some(Arc::clone(&ctx.fuse_cache)),
        fuse_fetch_timeout: ctx.fuse_fetch_timeout,
        // Same Arc as the registry entry and `build_cancelled` below.
        // execute_build polls it during the pre-cgroup phase (I-166).
        cancelled: Arc::clone(&build_cancelled),
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
        // Hold the slot until build completes — drop on any exit
        // (success, failure, panic, cancellation) clears slot.running
        // and slot.cancel, and wakes wait_idle().
        let _slot_guard = guard;

        // Proactive-ema wrap: 10s memory.peak samples flow to the
        // scheduler while the build runs. execute_build is the polled
        // future; run_with_resource_tick drives the select! loop.
        //
        // Daemon-transient retry: if nix-daemon crashes mid-handshake
        // (core dump, OOM-kill) the error surfaces as early-EOF on the
        // wire. Retrying locally is cheaper than a scheduler round-trip
        // (re-dispatch + re-fetch closure + re-generate synth DB) and
        // keeps a hot-loop daemon bug from flooding the scheduler with
        // InfrastructureFailure reports — without this, a crashing
        // daemon caused 800+ retries in <10min (scheduler re-dispatches
        // InfrastructureFailure immediately, no backoff). The retry
        // budget is small (DAEMON_RETRY_MAX=3, exponential backoff
        // 0.5/1/2s); after exhaustion the error propagates as
        // InfrastructureFailure and the scheduler's own retry policy
        // takes over. Cancelled builds short-circuit the loop — the
        // cancelled flag is set by try_cancel_build before cgroup.kill,
        // so checking it here avoids retrying a user-cancelled build.
        let mut attempt = 0u32;
        let result = loop {
            let r = run_with_resource_tick(
                executor::execute_build(
                    &assignment,
                    &build_env,
                    &mut build_store_client,
                    &build_tx,
                ),
                &build_cgroup_path,
                &build_drv_path,
                &build_tx,
            )
            .await;

            match &r {
                Err(e)
                    if e.is_daemon_transient()
                        && attempt < executor::DAEMON_RETRY_MAX
                        && !build_cancelled.load(std::sync::atomic::Ordering::Acquire) =>
                {
                    let delay = executor::DAEMON_RETRY_BASE_DELAY * 2u32.pow(attempt);
                    attempt += 1;
                    tracing::warn!(
                        drv_path = %drv_path,
                        attempt,
                        max = executor::DAEMON_RETRY_MAX,
                        retry_in = ?delay,
                        error = %e,
                        "daemon transient failure; retrying locally"
                    );
                    tokio::time::sleep(delay).await;
                }
                _ => break r,
            }
        };

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
                    // Expected outcome of CancelBuild / DrainExecutor(force).
                    // Not an error — info, not error. Scheduler's
                    // completion handler treats Cancelled as a no-op
                    // (already transitioned the derivation when it sent
                    // the CancelSignal).
                    (rio_proto::build_types::BuildResultStatus::Cancelled, false)
                } else if e.is_permanent() {
                    // Deterministic per-derivation (WrongKind, .drv
                    // parse failure). Another pod will fail identically;
                    // surface as InputRejected so the scheduler stops
                    // burning ephemeral cold-starts before the poison
                    // threshold trips.
                    (
                        rio_proto::build_types::BuildResultStatus::InputRejected,
                        true,
                    )
                } else {
                    // Node- or network-local executor failure (overlay
                    // mount, daemon crash, gRPC, IO). Another pod might
                    // succeed → InfrastructureFailure → reassign.
                    (
                        rio_proto::build_types::BuildResultStatus::InfrastructureFailure,
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
                    result: Some(rio_proto::build_types::BuildResult {
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
            Some(r) => match rio_proto::build_types::BuildResultStatus::try_from(r.status) {
                Ok(rio_proto::build_types::BuildResultStatus::Built) => "success",
                Ok(rio_proto::build_types::BuildResultStatus::Cancelled) => "cancelled",
                // Operationally distinct: means "raise the limit," not
                // "the build is broken." Separate label so SLI queries
                // can exclude these from failure-rate denominators.
                Ok(rio_proto::build_types::BuildResultStatus::TimedOut) => "timed_out",
                Ok(rio_proto::build_types::BuildResultStatus::LogLimitExceeded) => "log_limit",
                Ok(rio_proto::build_types::BuildResultStatus::InfrastructureFailure) => {
                    "infra_failure"
                }
                _ => "failure",
            },
            None => "failure",
        };
        metrics::counter!("rio_builder_builds_total", "outcome" => outcome).increment(1);

        let msg = ExecutorMessage {
            msg: Some(executor_message::Msg::Completion(completion)),
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
                result: Some(rio_proto::build_types::BuildResult {
                    status: rio_proto::build_types::BuildResultStatus::InfrastructureFailure.into(),
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
            let msg = ExecutorMessage {
                msg: Some(executor_message::Msg::Completion(completion)),
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

/// I-212: warm-gate size cap. PrefetchHint paths whose `nar_size`
/// exceeds this are skipped when the JIT allowlist is not yet armed
/// (i.e., during the initial warm-gate batch, before any assignment,
/// when the builder can't tell declared inputs from over-included
/// sibling outputs). The scheduler's `approx_input_closure` sends ALL outputs of
/// each input drv — a 2.9 GB `clang-debug` arrives alongside the
/// `clang-out` the build actually needs. Declared inputs the build DOES
/// need and that exceed the cap are fetched on-demand by JIT lookup
/// (which has the size-aware `jit_fetch_timeout`), so this never blocks
/// a correct build; it only stops the warm-gate from speculatively
/// pulling multi-GB paths it can't prove are needed.
///
/// 256 MiB: large enough to cover the common-set inputs the warm-gate is
/// for (glibc ~40 MB, gcc-unwrapped ~200 MB), small enough to exclude
/// debug outputs (clang-debug 2.9 GB, llvm-debug ~1.5 GB).
const PREFETCH_WARM_SIZE_CAP_BYTES: u64 = 256 * 1024 * 1024;

/// Handle a PrefetchHint from the scheduler: spawn one fire-and-forget
/// task per path to warm the FUSE cache, then send `PrefetchComplete`
/// once all paths have finished (succeeded, cached, or errored).
///
/// Called from main.rs's event loop. Does NOT block the caller: each
/// path is spawned as an independent tokio task that acquires a permit
/// from `sem` before entering the blocking pool. A joiner task awaits
/// all handles and sends the warm-gate ACK.
///
/// Warm-gate protocol (`r[sched.assign.warm-gate]`): the scheduler
/// gates dispatch on `ExecutorState.warm = true`, flipped on receipt of
/// `PrefetchComplete`. We send the ACK AFTER every path's fetch task
/// has returned — the scheduler's first assignment then arrives with a
/// warm cache. An empty hint (paths=[]) sends the ACK immediately.
///
/// No JoinHandle leak: if the worker SIGTERMs mid-prefetch, the tasks
/// abort with the runtime — the partial fetch is in a .tmp-XXXX sibling
/// dir (see fetch_extract_insert) which cache init cleans up on next
/// start. The joiner task also aborts; no ACK is sent — that's fine,
/// we're shutting down.
#[instrument(skip_all, fields(count = prefetch.store_paths.len()))]
pub fn handle_prefetch_hint(
    prefetch: PrefetchHint,
    cache: Arc<fuse::cache::Cache>,
    clients: crate::fuse::StoreClients,
    rt: tokio::runtime::Handle,
    sem: Arc<Semaphore>,
    fetch_timeout: std::time::Duration,
    stream_tx: mpsc::Sender<ExecutorMessage>,
) {
    // Collect JoinHandles for the ACK-joiner task. A typical hint
    // is ≤100 paths (scheduler caps at MAX_PREFETCH_PATHS=100) →
    // ≤100 handles. Cheap (JoinHandle is a small struct).
    let mut handles: Vec<tokio::task::JoinHandle<&'static str>> =
        Vec::with_capacity(prefetch.store_paths.len());

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
            metrics::counter!("rio_builder_prefetch_total", "result" => "malformed").increment(1);
            continue;
        };
        let basename = basename.to_string();

        // Clone handles into the task. All cheap:
        // Arc clone, tonic Channel is Arc-internal,
        // tokio Handle is a lightweight token.
        let cache = Arc::clone(&cache);
        let clients = clients.clone();
        let rt = rt.clone();
        let sem = Arc::clone(&sem);

        let handle = tokio::spawn(async move {
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
                return "shutdown";
            };

            // I-212 filter, BEFORE spawn_blocking: jit_classify is a
            // cheap RwLock read; QueryPathInfo (warm-gate arm) is a
            // single async RPC. Both belong in the async half so the
            // blocking pool only sees work that's actually going to
            // fetch.
            use crate::fuse::cache::JitClass;
            let store_path = format!("/nix/store/{basename}");
            match cache.jit_classify(&basename) {
                JitClass::NotInput => {
                    // Armed and NOT a declared input → the build can
                    // never read this path (FUSE lookup would ENOENT).
                    metrics::counter!("rio_builder_prefetch_filtered_total",
                                      "reason" => "not_input")
                    .increment(1);
                    metrics::counter!("rio_builder_prefetch_total",
                                      "result" => "not_input")
                    .increment(1);
                    return "not_input";
                }
                JitClass::NotArmed => {
                    // Warm-gate batch (before any assignment). The
                    // scheduler over-includes; we can't tell declared
                    // from sibling. Size-cap via QPI: skip paths above
                    // PREFETCH_WARM_SIZE_CAP_BYTES. On QPI error, fall
                    // through to fetch — I-211's progress-based timeout
                    // makes a large fetch correct, just slow.
                    let mut sc = clients.store.clone();
                    if let Ok(Some(info)) = rio_proto::client::query_path_info_opt(
                        &mut sc,
                        &store_path,
                        fetch_timeout,
                        &[],
                    )
                    .await
                        && info.nar_size > PREFETCH_WARM_SIZE_CAP_BYTES
                    {
                        metrics::counter!("rio_builder_prefetch_filtered_total",
                                          "reason" => "size_cap")
                        .increment(1);
                        metrics::counter!("rio_builder_prefetch_total",
                                          "result" => "size_cap")
                        .increment(1);
                        return "size_cap";
                    }
                }
                JitClass::KnownInput { .. } => {
                    // Declared input — fetch unconditionally. A huge
                    // declared input is still needed; JIT lookup would
                    // fetch it anyway, just later.
                }
            }

            // spawn_blocking: Cache methods use
            // block_on internally (nested-runtime
            // panic from async). The permit moves
            // into the blocking closure and drops
            // when it returns — next queued task
            // wakes.
            let result = tokio::task::spawn_blocking(move || {
                use crate::fuse::fetch::{PrefetchSkip, prefetch_path_blocking};
                let _permit = _permit; // hold through blocking work
                match prefetch_path_blocking(&cache, &clients, &rt, fetch_timeout, &basename) {
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
            metrics::counter!("rio_builder_prefetch_total", "result" => label).increment(1);
            label
        });
        handles.push(handle);
    }

    // r[impl sched.assign.warm-gate]
    // Joiner: wait for ALL path-fetch tasks to return, then send the
    // PrefetchComplete ACK. spawn_monitored so a panic in the joiner
    // logs with task=prefetch-complete instead of vanishing. Does NOT
    // block the caller (main.rs event loop).
    //
    // Serialization: all per-path tasks were spawned above. They run
    // concurrently (bounded by `sem`). The joiner awaits each handle
    // in order — order doesn't matter for semantics (we only care
    // about "all done"), it's just the simplest join-all. A slow path
    // delays the ACK for the whole batch, which is CORRECT: the
    // scheduler should wait until the cache is actually warm.
    rio_common::task::spawn_monitored("prefetch-complete", async move {
        let mut fetched: u32 = 0;
        let mut cached: u32 = 0;
        for handle in handles {
            // JoinError (task aborted or panicked) → count as neither
            // fetched nor cached. The label was already recorded in
            // the metric above; for the ACK we just skip it.
            if let Ok(label) = handle.await {
                match label {
                    "fetched" => fetched += 1,
                    "already_cached" | "already_in_flight" => cached += 1,
                    // "error", "malformed", "shutdown", "panic" → noise.
                    // Scheduler gates on receipt, not on counts.
                    _ => {}
                }
            }
        }

        // TODO(P0311): ordering-proof VM scenario uses this hook.
        // Test hook: RIO_TEST_PREFETCH_DELAY_MS injects an extra delay
        // AFTER all fetches complete but BEFORE the ACK. The VM warm-
        // gate scenario uses this to prove the scheduler waits for the
        // ACK (assert assigned_at - registered_at >= delay). Parse
        // failure → 0 → no delay. Only read once per hint (not hot).
        if let Ok(ms) = std::env::var("RIO_TEST_PREFETCH_DELAY_MS")
            && let Ok(ms) = ms.parse::<u64>()
            && ms > 0
        {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        }

        // send().await not try_send(): the ACK MUST land. If the
        // permanent-sink relay is backpressured (256 cap filled by
        // log batches during a chatty build), we block here until a
        // slot frees. That delays the NEXT prefetch hint's ACK by
        // one stream-roundtrip — acceptable. Dropping the ACK would
        // leave the worker cold in the scheduler's view forever.
        let ack = ExecutorMessage {
            msg: Some(executor_message::Msg::PrefetchComplete(PrefetchComplete {
                paths_fetched: fetched,
                paths_cached: cached,
            })),
        };
        if let Err(e) = stream_tx.send(ack).await {
            // Sink closed → worker is shutting down. Fine — no point
            // ACKing to a scheduler we're disconnecting from.
            tracing::debug!(error = %e,
                            "PrefetchComplete send failed (sink closed; shutting down?)");
        } else {
            tracing::debug!(fetched, cached, "sent PrefetchComplete (warm-gate ACK)");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- try_cancel_build ----

    /// Slot running + cgroup.kill file exists → kill written,
    /// flag set, returns true.
    #[test]
    fn cancel_build_found_in_slot() {
        // Use a tmpdir as a fake cgroup. cgroup.kill is a write-
        // once pseudo-file in a real cgroup2fs; in tmpfs it's just
        // a regular file that gets the "1" written. Good enough
        // for testing the plumbing (real cgroup behavior is
        // VM-tested in vm-phase3b).
        let tmpdir = tempfile::tempdir().unwrap();
        let cgroup_path = tmpdir.path().to_path_buf();
        let cancelled = Arc::new(AtomicBool::new(false));

        let slot = Arc::new(BuildSlot::default());
        let _g = slot.try_claim("/nix/store/test.drv").unwrap();
        slot.set_cancel_target(cgroup_path.clone(), Arc::clone(&cancelled));

        let found = try_cancel_build(&slot, "/nix/store/test.drv");
        assert!(found, "running drv in slot → true");
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

    /// Slot idle, or running a different drv → returns false.
    #[test]
    fn cancel_build_not_found() {
        let slot = Arc::new(BuildSlot::default());
        assert!(
            !try_cancel_build(&slot, "/nix/store/absent.drv"),
            "idle slot → false"
        );

        let _g = slot.try_claim("/nix/store/other.drv").unwrap();
        slot.set_cancel_target(PathBuf::from("/nope"), Arc::new(AtomicBool::new(false)));
        assert!(
            !try_cancel_build(&slot, "/nix/store/absent.drv"),
            "drv mismatch → false (stale CancelSignal guard)"
        );
    }

    /// Cancel arrives before cgroup exists → kill ENOENT → flag STAYS
    /// set. execute_build polls it during prefetch+warm and aborts
    /// pre-daemon-spawn.
    ///
    /// I-166: this INVERTS the previous behaviour (flag cleared on
    /// ENOENT, cancel lost, scheduler backstop catches it). I-165
    /// showed the pre-cgroup window can be 47 min (warm stalled on a
    /// saturated store), not "narrow"; the backstop is
    /// activeDeadlineSeconds=1h. The misclassification risk that
    /// motivated the old clear (unrelated Err → reported as Cancelled)
    /// is the lesser evil — a build the scheduler already cancelled
    /// has no client waiting on its real outcome.
    ///
    // r[verify builder.cancel.pre-cgroup-deferred]
    #[test]
    fn cancel_build_cgroup_missing_keeps_flag() {
        let cancelled = Arc::new(AtomicBool::new(false));
        // Path that definitely doesn't exist. tmpdir/nonexistent so
        // the test doesn't depend on /sys/fs/cgroup being mounted (CI
        // sandbox may not have cgroup v2).
        let tmp = tempfile::tempdir().unwrap();
        let fake_cgroup = tmp.path().join("not-created-yet");

        let slot = Arc::new(BuildSlot::default());
        let _g = slot.try_claim("/nix/store/test.drv").unwrap();
        slot.set_cancel_target(fake_cgroup, Arc::clone(&cancelled));

        let got = try_cancel_build(&slot, "/nix/store/test.drv");

        // Kill was a no-op (ENOENT) but the cancel INTENT is recorded.
        // true: the entry was found and the flag is set; the executor's
        // pre-cgroup poll will honour it.
        assert!(got, "ENOENT cancel should return true (flag set, deferred)");
        // Load-bearing: flag stays TRUE so execute_build's pre-cgroup
        // check / warm-phase poll aborts with ExecutorError::Cancelled
        // instead of proceeding to daemon spawn.
        assert!(
            cancelled.load(std::sync::atomic::Ordering::Acquire),
            "flag must stay set on ENOENT so the pre-cgroup poll can abort the build"
        );
    }

    #[tokio::test]
    async fn test_heartbeat_reports_running_build() {
        let slot = Arc::new(BuildSlot::default());
        let _guard = slot.try_claim("/nix/store/foo.drv").unwrap();

        let req = build_heartbeat_request(
            "worker-1",
            rio_proto::types::ExecutorKind::Builder,
            &["x86_64-linux".into()],
            &[],
            "",
            &slot,
            &ResourceSnapshotHandle::default(),
            false,
            false,
        )
        .await;
        assert_eq!(req.running_builds, vec!["/nix/store/foo.drv".to_string()]);
        assert_eq!(req.executor_id, "worker-1");
        assert_eq!(req.systems, vec!["x86_64-linux"]);
        // ResourceUsage.running_builds mirrors the top-level field.
        assert_eq!(req.resources.unwrap().running_builds, 1);
    }

    #[tokio::test]
    async fn test_heartbeat_empty_running_builds() {
        let slot = Arc::new(BuildSlot::default());
        let req = build_heartbeat_request(
            "worker-1",
            rio_proto::types::ExecutorKind::Builder,
            &["x86_64-linux".into()],
            &[],
            "",
            &slot,
            &ResourceSnapshotHandle::default(),
            false,
            false,
        )
        .await;
        assert!(req.running_builds.is_empty());
    }

    /// size_class passes through verbatim (scheduler interprets "" as None).
    #[tokio::test]
    async fn test_heartbeat_includes_size_class() {
        let slot = Arc::new(BuildSlot::default());
        let req = build_heartbeat_request(
            "worker-1",
            rio_proto::types::ExecutorKind::Builder,
            &["x86_64-linux".into()],
            &[],
            "large",
            &slot,
            &ResourceSnapshotHandle::default(),
            false,
            false,
        )
        .await;
        assert_eq!(req.size_class, "large");

        let req = build_heartbeat_request(
            "worker-1",
            rio_proto::types::ExecutorKind::Builder,
            &["x86_64-linux".into()],
            &[],
            "",
            &slot,
            &ResourceSnapshotHandle::default(),
            false,
            false,
        )
        .await;
        assert_eq!(req.size_class, "", "empty = unclassified");
    }

    /// `store_degraded` passes through to the proto field. main.rs
    /// reads `CircuitBreaker::is_open()` each heartbeat tick and
    /// passes it here — this test is the contract between the two:
    /// whatever bool main.rs observes, the scheduler sees.
    ///
    /// No mock circuit: `build_heartbeat_request` takes the bool
    /// directly (the circuit lives in main.rs's FUSE setup, which
    /// has no unit-test harness). The scheduler-side consumption
    /// is `r[verify]`'d separately (P0211, has_capacity excludes
    /// degraded workers).
    #[tokio::test]
    async fn test_heartbeat_store_degraded_passthrough() {
        let slot = Arc::new(BuildSlot::default());

        let req = build_heartbeat_request(
            "worker-1",
            rio_proto::types::ExecutorKind::Builder,
            &["x86_64-linux".into()],
            &[],
            "",
            &slot,
            &ResourceSnapshotHandle::default(),
            true,
            false,
        )
        .await;
        assert!(
            req.store_degraded,
            "circuit open → proto field set → scheduler excludes via has_capacity()"
        );

        // Control: default path is NOT degraded. Guards against an
        // accidental hardcode-true in the struct literal.
        let req = build_heartbeat_request(
            "worker-1",
            rio_proto::types::ExecutorKind::Builder,
            &["x86_64-linux".into()],
            &[],
            "",
            &slot,
            &ResourceSnapshotHandle::default(),
            false,
            false,
        )
        .await;
        assert!(!req.store_degraded);
    }

    /// Regression: supported_features must reflect the config — if
    /// hardcoded empty, the CRD's features field is silently ignored
    /// and any derivation with requiredSystemFeatures never dispatches.
    #[tokio::test]
    async fn test_heartbeat_includes_systems_and_features() {
        let slot = Arc::new(BuildSlot::default());
        let req = build_heartbeat_request(
            "worker-1",
            rio_proto::types::ExecutorKind::Builder,
            &["x86_64-linux".into(), "aarch64-linux".into()],
            &["kvm".into(), "big-parallel".into()],
            "",
            &slot,
            &ResourceSnapshotHandle::default(),
            false,
            false,
        )
        .await;
        assert_eq!(req.systems, vec!["x86_64-linux", "aarch64-linux"]);
        assert_eq!(req.supported_features, vec!["kvm", "big-parallel"]);
    }

    /// I-212: when the JIT allowlist is armed, `handle_prefetch_hint`
    /// MUST skip paths the build can never read (jit_classify=NotInput).
    /// The scheduler's per-assignment hint over-includes (sends ALL
    /// outputs of each input drv — e.g., 2.9 GB clang-debug when only
    /// clang-out is declared). Filter at the latest possible point
    /// (inside the spawned task, after the sem permit) so register_inputs
    /// has the widest window to land first.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_prefetch_hint_filters_not_input_when_armed() {
        use rio_test_support::fixtures::{make_nar, make_path_info, test_store_basename};
        use rio_test_support::grpc::spawn_mock_store;

        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(
            crate::fuse::cache::Cache::new(dir.path().to_path_buf(), 10)
                .await
                .unwrap(),
        );
        let (store, addr, _srv) = spawn_mock_store().await.unwrap();
        let ch = rio_proto::client::connect_channel(&addr.to_string())
            .await
            .unwrap();
        let clients = crate::fuse::StoreClients::from_channel(ch);

        // Seed both paths so a fetch (if attempted) would succeed.
        let known = test_store_basename("i212-known");
        let extra = test_store_basename("i212-extra");
        for b in [&known, &extra] {
            let p = format!("/nix/store/{b}");
            let (nar, hash) = make_nar(b"x");
            store.seed(make_path_info(&p, &nar, hash), nar);
        }

        // Arm JIT with ONLY `known`. `extra` → NotInput.
        cache.register_inputs([(known.clone(), 1)]);

        let (tx, mut rx) = mpsc::channel(4);
        handle_prefetch_hint(
            PrefetchHint {
                store_paths: vec![format!("/nix/store/{known}"), format!("/nix/store/{extra}")],
            },
            Arc::clone(&cache),
            clients,
            tokio::runtime::Handle::current(),
            Arc::new(Semaphore::new(4)),
            Duration::from_secs(5),
            tx,
        );

        // Joiner sends PrefetchComplete after all per-path tasks finish.
        let ack = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("ACK within 10s")
            .expect("channel open");
        assert!(
            matches!(
                ack.msg,
                Some(rio_proto::types::executor_message::Msg::PrefetchComplete(_))
            ),
            "expected PrefetchComplete ACK, got: {ack:?}"
        );

        assert!(
            dir.path().join(&known).exists(),
            "known input MUST be fetched"
        );
        assert!(
            !dir.path().join(&extra).exists(),
            "I-212: NotInput path MUST be skipped when JIT armed"
        );
    }

    /// I-212: when JIT is NOT armed (initial warm-gate hint, before any
    /// assignment), the filter MUST NOT fire — every hinted path is
    /// fetched. Skipping here would defeat the warm cache.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_prefetch_hint_unarmed_fetches_all() {
        use rio_test_support::fixtures::{make_nar, make_path_info, test_store_basename};
        use rio_test_support::grpc::spawn_mock_store;

        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(
            crate::fuse::cache::Cache::new(dir.path().to_path_buf(), 10)
                .await
                .unwrap(),
        );
        let (store, addr, _srv) = spawn_mock_store().await.unwrap();
        let ch = rio_proto::client::connect_channel(&addr.to_string())
            .await
            .unwrap();
        let clients = crate::fuse::StoreClients::from_channel(ch);

        let a = test_store_basename("i212-warm-a");
        let b = test_store_basename("i212-warm-b");
        for name in [&a, &b] {
            let p = format!("/nix/store/{name}");
            let (nar, hash) = make_nar(b"x");
            store.seed(make_path_info(&p, &nar, hash), nar);
        }
        // NO register_inputs → NotArmed.

        let (tx, mut rx) = mpsc::channel(4);
        handle_prefetch_hint(
            PrefetchHint {
                store_paths: vec![format!("/nix/store/{a}"), format!("/nix/store/{b}")],
            },
            Arc::clone(&cache),
            clients,
            tokio::runtime::Handle::current(),
            Arc::new(Semaphore::new(4)),
            Duration::from_secs(5),
            tx,
        );

        let _ack = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("ACK within 10s")
            .expect("channel open");

        assert!(
            dir.path().join(&a).exists() && dir.path().join(&b).exists(),
            "NotArmed → both paths fetched (warm-gate preserved)"
        );
    }

    /// I-212: warm-gate (NotArmed) size cap. A path whose
    /// `QueryPathInfo.nar_size` exceeds `PREFETCH_WARM_SIZE_CAP_BYTES` is
    /// skipped; a small one alongside is still fetched. The build can
    /// always fetch the large path on-demand via JIT lookup if it turns
    /// out to be a real input — this only stops the warm-gate from
    /// speculatively pulling 2.9 GB clang-debug.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_prefetch_hint_unarmed_size_cap_skips_large() {
        use rio_test_support::fixtures::{make_nar, make_path_info, test_store_basename};
        use rio_test_support::grpc::spawn_mock_store;

        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(
            crate::fuse::cache::Cache::new(dir.path().to_path_buf(), 10)
                .await
                .unwrap(),
        );
        let (store, addr, _srv) = spawn_mock_store().await.unwrap();
        let ch = rio_proto::client::connect_channel(&addr.to_string())
            .await
            .unwrap();
        let clients = crate::fuse::StoreClients::from_channel(ch);

        let small = test_store_basename("i212-cap-small");
        let large = test_store_basename("i212-cap-large");
        let (nar, hash) = make_nar(b"x");
        store.seed(
            make_path_info(&format!("/nix/store/{small}"), &nar, hash),
            nar.clone(),
        );
        // Large: PathInfo.nar_size lies (> cap) so QPI sees it as huge;
        // actual NAR is tiny (we never GetPath it).
        let mut large_info = make_path_info(&format!("/nix/store/{large}"), &nar, hash);
        large_info.nar_size = super::PREFETCH_WARM_SIZE_CAP_BYTES + 1;
        store.seed(large_info, nar);
        // NotArmed → size-cap arm.

        let (tx, mut rx) = mpsc::channel(4);
        handle_prefetch_hint(
            PrefetchHint {
                store_paths: vec![format!("/nix/store/{small}"), format!("/nix/store/{large}")],
            },
            Arc::clone(&cache),
            clients,
            tokio::runtime::Handle::current(),
            Arc::new(Semaphore::new(4)),
            Duration::from_secs(5),
            tx,
        );

        let _ack = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("ACK within 10s")
            .expect("channel open");

        assert!(
            dir.path().join(&small).exists(),
            "small path under cap MUST be fetched"
        );
        assert!(
            !dir.path().join(&large).exists(),
            "I-212: NotArmed path over size cap MUST be skipped"
        );
    }
}

#[cfg(test)]
mod resource_tick_tests {
    use super::*;

    /// Extract memory_used_bytes from Progress messages; filter the rest.
    fn progress_peaks(rx: &mut mpsc::Receiver<ExecutorMessage>) -> Vec<u64> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            if let Some(executor_message::Msg::Progress(p)) = m.msg {
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
        let Some(executor_message::Msg::Progress(p)) = msg.msg else {
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
