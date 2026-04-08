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

    // Read lock held for one struct clone. First heartbeat (before
    // first 10s poll) sends zeros — same as ResourceUsage::default(),
    // converges after one tick. Override running_builds here: the
    // cgroup sampler doesn't know the running set. Redundant with the
    // top-level HeartbeatRequest field but filling it keeps the
    // ResourceUsage message self-contained for ListExecutors consumers.
    let running_count = current.len() as u32;
    let resources = {
        let mut ru = *resources.read().unwrap_or_else(|e| e.into_inner());
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
    let build_cgroup_path = ctx.cgroup_parent.join(&build_id);
    let cancelled = Arc::new(AtomicBool::new(false));
    ctx.slot
        .set_cancel_target(build_cgroup_path.clone(), Arc::clone(&cancelled));

    // Clone for the panic handler before moving ctx into the task.
    let panic_tx = ctx.stream_tx.clone();
    let panic_drv_path = drv_path.clone();
    let panic_token = assignment_token.clone();

    // The spawned task needs 'static; clone the whole context once and
    // move it in. ExecutorEnv is built INSIDE the task from the owned
    // ctx fields (no per-field clone boilerplate).
    let ctx = ctx.clone();

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

        let mut store_client = ctx.store_clients.store.clone();
        let build_env = executor::ExecutorEnv {
            fuse_mount_point: ctx.fuse_mount_point,
            overlay_base_dir: ctx.overlay_base_dir,
            executor_id: ctx.executor_id,
            log_limits: ctx.log_limits,
            daemon_timeout: ctx.daemon_timeout,
            max_silent_time: ctx.max_silent_time,
            cgroup_parent: ctx.cgroup_parent,
            executor_kind: ctx.executor_kind,
            fuse_cache: Some(ctx.fuse_cache),
            fuse_fetch_timeout: ctx.fuse_fetch_timeout,
            // Same Arc as the slot's cancel target. execute_build polls
            // it during the pre-cgroup phase (I-166).
            cancelled: Arc::clone(&cancelled),
        };

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
                executor::execute_build(&assignment, &build_env, &mut store_client, &ctx.stream_tx),
                &build_cgroup_path,
                &drv_path,
                &ctx.stream_tx,
            )
            .await;

            match &r {
                Err(e)
                    if e.is_daemon_transient()
                        && attempt < executor::DAEMON_RETRY_MAX
                        && !cancelled.load(std::sync::atomic::Ordering::Acquire) =>
                {
                    let delay = executor::DAEMON_RETRY_BACKOFF.duration(attempt);
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
                let was_cancelled = cancelled.load(std::sync::atomic::Ordering::Acquire);
                let (status, log_level) = if was_cancelled {
                    // Expected outcome of CancelBuild / DrainExecutor(force).
                    // Not an error — info, not error. Scheduler's
                    // completion handler treats Cancelled as a no-op
                    // (already transitioned the derivation when it sent
                    // the CancelSignal).
                    (rio_proto::types::BuildResultStatus::Cancelled, false)
                } else if e.is_permanent() {
                    // Deterministic per-derivation (WrongKind, .drv
                    // parse failure). Another pod will fail identically;
                    // surface as InputRejected so the scheduler stops
                    // burning ephemeral cold-starts before the poison
                    // threshold trips.
                    (rio_proto::types::BuildResultStatus::InputRejected, true)
                } else {
                    // Node- or network-local executor failure (overlay
                    // mount, daemon crash, gRPC, IO). Another pod might
                    // succeed → InfrastructureFailure → reassign.
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
        metrics::counter!("rio_builder_builds_total", "outcome" => outcome).increment(1);

        let msg = ExecutorMessage {
            msg: Some(executor_message::Msg::Completion(completion)),
        };
        if let Err(e) = ctx.stream_tx.send(msg).await {
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
#[allow(clippy::too_many_arguments)]
#[instrument(skip_all, fields(count = prefetch.store_paths.len()))]
pub fn handle_prefetch_hint(
    prefetch: PrefetchHint,
    cache: Arc<fuse::cache::Cache>,
    clients: crate::fuse::StoreClients,
    rt: tokio::runtime::Handle,
    sem: Arc<Semaphore>,
    fetch_timeout: std::time::Duration,
    test_ack_delay: std::time::Duration,
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
        // Test hook: `Config.test_prefetch_delay` injects a delay AFTER
        // all fetches complete but BEFORE the ACK. The VM warm-gate
        // scenario uses it to prove the scheduler waits for the ACK
        // (assert assigned_at - registered_at >= delay).
        if !test_ack_delay.is_zero() {
            tokio::time::sleep(test_ack_delay).await;
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

// ─── main-loop runtime (extracted from main.rs) ──────────────────────

use crate::config::{Config, detect_system};
use rio_proto::types::{ExecutorRegister, scheduler_message};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::watch;
use tracing::info;

/// Heartbeat interval. Shared source of truth with the scheduler's timeout
/// check (`rio_common::limits::HEARTBEAT_TIMEOUT_SECS` derives from this).
const HEARTBEAT_INTERVAL: Duration =
    Duration::from_secs(rio_common::limits::HEARTBEAT_INTERVAL_SECS);

/// Fully-wired worker runtime. Built by [`setup`]; consumed by [`run`].
/// Holds every long-lived handle the reconnect loop needs so `main()`
/// reduces to bootstrap → setup → run → teardown.
pub struct BuilderRuntime {
    scheduler_addr: String,
    scheduler_client: WorkerClient,
    shutdown: rio_common::signal::Token,
    /// FUSE mount session. Dropped explicitly in [`run`]'s teardown
    /// (NOT here) so the abort-then-sleep ordering in `r[builder.shutdown.fuse-abort]`
    /// stays adjacent to the comment that explains it.
    fuse_session: crate::fuse::FuseMount,
    relay_target_tx: watch::Sender<Option<mpsc::Sender<ExecutorMessage>>>,
    slot: Arc<BuildSlot>,
    draining: Arc<AtomicBool>,
    drain_done: Arc<Notify>,
    build_done: Arc<Notify>,
    latest_generation: Arc<AtomicU64>,
    heartbeat_handle: tokio::task::JoinHandle<()>,
    build_ctx: BuildSpawnContext,
    /// Prefetch handler dependencies. Bundled so [`run`] can call
    /// [`handle_prefetch_hint`] without 7 loose fields.
    prefetch: PrefetchDeps,
    idle_timeout: Duration,
    test_prefetch_delay: Duration,
    /// Probe-loop guards for both balanced channels. Held for process
    /// lifetime (dropping a `BalancedChannel` stops its probe loop).
    _balance_guard: BalanceGuards,
}

struct PrefetchDeps {
    cache: Arc<crate::fuse::cache::Cache>,
    clients: StoreClients,
    runtime: tokio::runtime::Handle,
    sem: Arc<Semaphore>,
    fetch_timeout: Duration,
}

/// Wire up cgroups, health server, gRPC clients, FUSE mount, relay,
/// heartbeat, and the build context. Everything `main()` did before the
/// `'reconnect` loop.
///
/// Returns `None` if shutdown fired during cold-start connect — caller
/// exits cleanly (nothing to drain, never connected).
pub async fn setup(
    mut cfg: Config,
    shutdown: rio_common::signal::Token,
) -> anyhow::Result<Option<BuilderRuntime>> {
    let (executor_id, systems, features) = resolve_executor_identity(
        std::mem::take(&mut cfg.executor_id),
        std::mem::take(&mut cfg.systems),
        std::mem::take(&mut cfg.features),
    )?;
    validate_host_arch(cfg.executor_kind, &systems, &detect_system())?;
    info!(%executor_id, "executor identity resolved");

    // cgroup setup BEFORE the health server: if cgroup fails, we don't
    // want liveness passing while startup is hung on `?` propagation.
    // Pod goes straight to CrashLoopBackOff with a clear log line.
    let (cgroup_parent, resource_snapshot) = init_cgroup(&cfg.overlay_base_dir, shutdown.clone())?;

    // Readiness flag + HTTP health server. Spawned BEFORE gRPC connect
    // so liveness passes as soon as the process is up (connect may take
    // seconds if scheduler DNS is slow to resolve). Readiness stays
    // false until the first heartbeat comes back accepted — that's the
    // right gate: a worker that can't heartbeat is not useful capacity.
    let ready = Arc::new(AtomicBool::new(false));
    crate::health::spawn_health_server(cfg.health_addr, Arc::clone(&ready), shutdown.clone());

    let Some((store_clients, scheduler_client, _balance_guard)) =
        connect_upstreams(&cfg, &shutdown).await
    else {
        // Shutdown fired during cold-start connect. Clean exit —
        // nothing to drain (never connected), no FUSE mounted yet.
        return Ok(None);
    };
    info!(
        %executor_id,
        scheduler_addr = %cfg.scheduler.addr,
        store_addr = %cfg.store.addr,
        systems = ?systems,
        features = ?features,
        "connected to gRPC services"
    );

    // Set up FUSE cache and mount. Arc so we can clone for the
    // prefetch handler before moving into mount_fuse_background.
    let cache = Arc::new(crate::fuse::cache::Cache::new(cfg.fuse_cache_dir).await?);
    // Clone for prefetch. Cache methods use runtime.block_on
    // internally (sync, designed for FUSE callbacks on dedicated
    // threads). The prefetch handler will call them via
    // spawn_blocking — async → nested-runtime panic.
    let prefetch_cache = Arc::clone(&cache);
    let runtime = tokio::runtime::Handle::current();
    // FUSE fetch timeout (60s default) — NOT GRPC_STREAM_TIMEOUT (300s).
    // FUSE is the build-critical path; a stalled fetch blocks a fuser
    // thread. See config.rs fuse_fetch_timeout for the full rationale.
    let fuse_fetch_timeout = cfg.fuse_fetch_timeout;
    // Process-global FUSE fetch transport — read by FUSE callbacks
    // that have no Config handle. Set once, before mount.
    crate::fuse::fetch::FetchTransport::init(cfg.fetch_transport);

    // ─── Startup rootfs writes (readOnlyRootFilesystem audit) ─────
    //
    // FetcherPool forces readOnlyRootFilesystem:true (ADR-019
    // §Sandbox hardening — reconcilers/fetcherpool/mod.rs:212).
    // Every write below MUST land on an emptyDir mount from
    // reconcilers/common/sts.rs, or the pod CrashLoops with EROFS.
    //
    //   path                        | covering mount (sts.rs)
    //   ──────────────────────────────────────────────────────────
    //   cfg.fuse_mount_point        | `fuse-store` emptyDir
    //     (/var/rio/fuse-store)     |   (readOnlyRoot only)
    //   cfg.overlay_base_dir        | `overlays` emptyDir
    //     (/var/rio/overlays)       |   (always)
    //   /nix/var/{nix,log}/**       | `nix-var` emptyDir
    //                               |   (readOnlyRoot only)
    //   /tmp (tempfile crate)       | `tmp` emptyDir, 64Mi tmpfs
    //                               |   (readOnlyRoot only)
    //   cfg.fuse_cache_dir          | `fuse-cache` emptyDir
    //     (/var/rio/cache —         |   (always)
    //      Cache::new above)        |
    //   /sys/fs/cgroup/**           | cgroupfs, not rootfs —
    //     (cgroup.rs)               |   remounted rw at cgroup.rs
    //                               |   ns-root-remount
    //
    // Adding a new startup write? Extend BOTH this table AND the
    // `if p.read_only_root_fs` blocks in common/sts.rs (Volume +
    // VolumeMount pair). vm-fetcher-split-k3s catches misses.
    std::fs::create_dir_all(&cfg.fuse_mount_point)?;
    std::fs::create_dir_all(&cfg.overlay_base_dir)?;
    // nix's `LocalStore` (chroot-store via `--store local?root=X`)
    // refuses to open if any ancestor of X is world-writable. The k8s
    // emptyDir at overlay_base_dir is 0777; clamp it and its parent.
    {
        use anyhow::Context as _;
        use std::os::unix::fs::PermissionsExt;
        let mode_755 = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&cfg.overlay_base_dir, mode_755.clone()).with_context(|| {
            format!(
                "chmod 0755 {} (nix LocalStore refuses world-writable ancestor)",
                cfg.overlay_base_dir.display()
            )
        })?;
        if let Some(parent) = cfg.overlay_base_dir.parent()
            && let Err(e) = std::fs::set_permissions(parent, mode_755)
        {
            tracing::warn!(path = %parent.display(), error = %e, "chmod parent of overlay_base_dir");
        }
    }

    let (fuse_session, fuse_circuit) = crate::fuse::mount_fuse_background(
        &cfg.fuse_mount_point,
        cache,
        store_clients.clone(),
        runtime.clone(),
        cfg.fuse_passthrough,
        cfg.fuse_threads,
        fuse_fetch_timeout,
    )?;

    info!(
        mount_point = %cfg.fuse_mount_point.display(),
        "FUSE store mounted"
    );

    // ---- BuildExecution stream with reconnect ----
    //
    // Architecture: a PERMANENT sink channel (sink_tx, sink_rx)
    // lives for process lifetime. BuildSpawnContext holds sink_tx
    // — running builds send CompletionReport/LogBatch here.
    // sink_rx is drained by a relay task that pumps into whatever
    // gRPC outbound channel is currently live (via watch::channel).
    //
    // Why: stderr_loop.rs breaks the build with MiscFailure if
    // its log send fails (channel closed). If we handed build
    // tasks the gRPC channel directly, stream death on scheduler
    // failover would kill every running build. With the permanent
    // sink, the build tasks' channel NEVER closes — the relay
    // just buffers (up to mpsc capacity) during the ~1s gap
    // between old-stream-dead and new-stream-open.
    //
    // The relay recovers the one message lost on transition
    // (mpsc::error::SendError<T> holds the unsent message) and
    // blocks on watch.changed() until the reconnect loop swaps
    // in a fresh gRPC channel.
    let (sink_tx, sink_rx) = mpsc::channel::<ExecutorMessage>(256);

    // Relay target: Some(grpc_tx) while connected, None during
    // the reconnect gap. Starts None — the reconnect loop sets
    // it before opening the first stream.
    let (relay_target_tx, relay_target_rx) =
        watch::channel::<Option<mpsc::Sender<ExecutorMessage>>>(None);

    rio_common::task::spawn_monitored("stream-relay", relay_loop(sink_rx, relay_target_rx));

    // P0537: one build per pod. The slot tracks both occupancy and
    // the running drv_path (heartbeat reads it). `try_claim` is
    // non-blocking — see BuildSlot doc for why.
    let slot = Arc::new(BuildSlot::default());

    // I-063: drain state. Set true on first SIGTERM. Heartbeat reports
    // it (worker is authority); the assignment handler rejects while
    // set; the reconnect loop KEEPS the stream alive (completions for
    // in-flight builds reach whichever scheduler is leader) until
    // `drain_done` fires (slot idle via wait_idle()).
    let draining = Arc::new(AtomicBool::new(false));
    let drain_done = Arc::new(Notify::new());

    // Build-complete signal. Notified after the one build is spawned
    // AND its permit returns (CompletionReport sent — spawn_build_task's
    // scopeguard drops the permit after build_tx.send(completion).await).
    // The select loop awaits this; one notification = exit.
    //
    // Notify not oneshot: Notify is cheaper (no channel allocation)
    // and `notified()` is cancel-safe for select!.
    let build_done = Arc::new(Notify::new());

    // Latest generation observed in an accepted HeartbeatResponse.
    // Starts at 0 — scheduler generation is always ≥1 (lease/mod.rs
    // non-K8s path starts at 1; k8s Lease increments from 1 on first
    // acquire), so 0 never rejects a real assignment. Relaxed ordering:
    // this is a fence against a DIFFERENT process's stale writes, not a
    // within-process happens-before. The value itself is the signal.
    let latest_generation = Arc::new(AtomicU64::new(0));
    let heartbeat_handle = spawn_heartbeat(HeartbeatCtx {
        executor_id: executor_id.clone(),
        executor_kind: cfg.executor_kind,
        // move (not clone): owned Vecs, used only by the heartbeat
        // loop. setup() has no further use for them.
        systems,
        features,
        size_class: cfg.size_class.clone(),
        slot: Arc::clone(&slot),
        ready: Arc::clone(&ready),
        resources: Arc::clone(&resource_snapshot),
        // FUSE circuit breaker: polled each tick. move into the task:
        // setup() has no other use for the handle.
        circuit: fuse_circuit,
        draining: Arc::clone(&draining),
        generation: Arc::clone(&latest_generation),
        client: scheduler_client.clone(),
    });

    // Shared context for spawning build tasks (clones done once per assignment
    // inside spawn_build_task, not here).
    let build_ctx = BuildSpawnContext {
        store_clients: store_clients.clone(),
        executor_id,
        fuse_mount_point: cfg.fuse_mount_point,
        overlay_base_dir: cfg.overlay_base_dir,
        // The permanent sink, NOT a per-connection gRPC channel.
        // Build tasks' sends never fail on scheduler failover.
        stream_tx: sink_tx,
        slot: Arc::clone(&slot),
        log_limits: crate::log_stream::LogLimits {
            rate_lines_per_sec: cfg.log_rate_limit,
            total_bytes: cfg.log_size_limit,
        },
        daemon_timeout: cfg.daemon_timeout,
        max_silent_time: cfg.max_silent_time_secs,
        cgroup_parent,
        executor_kind: cfg.executor_kind,
        // I-110c: same Arc as prefetch_cache / the FUSE mount —
        // executor primes manifest hints + JIT allowlist, FUSE threads
        // consume them.
        fuse_cache: Arc::clone(&prefetch_cache),
        // Base per-path fetch timeout; JIT lookup scales it with
        // nar_size (I-178). Same value the PrefetchHint handler uses.
        fuse_fetch_timeout,
    };

    Ok(Some(BuilderRuntime {
        scheduler_addr: cfg.scheduler.addr,
        scheduler_client,
        shutdown,
        fuse_session,
        relay_target_tx,
        slot,
        draining,
        drain_done,
        build_done,
        latest_generation,
        heartbeat_handle,
        build_ctx,
        prefetch: PrefetchDeps {
            cache: prefetch_cache,
            clients: store_clients,
            runtime,
            // Prefetch concurrency limit. 8 is conservative: each holds a
            // tokio blocking-pool thread (default pool is 512, so no
            // starvation concern) AND pins an in-flight gRPC stream to the
            // store (which is what we're bounding — don't DDoS the store with
            // 100 parallel NARs when the scheduler sends a big hint list).
            sem: Arc::new(Semaphore::new(8)),
            fetch_timeout: fuse_fetch_timeout,
        },
        idle_timeout: cfg.idle_timeout,
        test_prefetch_delay: cfg.test_prefetch_delay,
        _balance_guard,
    }))
}

/// Drive the `'reconnect` loop until the single build completes or drain
/// finishes, then run exit teardown (heartbeat abort, `DrainExecutor`,
/// FUSE abort).
///
/// `select!` is biased toward shutdown: poll it FIRST each iteration.
/// Without `biased;`, `select!` picks a ready branch pseudorandomly —
/// under heavy assignment traffic, the token could starve behind stream
/// messages. K8s sends SIGTERM then starts the grace period clock; we
/// want to react immediately, not after the next gap in assignments.
pub async fn run(mut rt: BuilderRuntime) -> anyhow::Result<()> {
    // Spawn the build-done watcher task exactly ONCE (on the first
    // assignment). AtomicBool swap(true) returns the previous value —
    // only the first caller sees false. Lives outside the reconnect
    // loop: a scheduler failover mid-build must NOT spawn a second
    // watcher (the build task keeps running and returns its permit
    // regardless of stream state).
    let done_watcher_spawned = AtomicBool::new(false);
    // I-116: idle timeout. `last_activity` is bumped on every received
    // stream message; the select arm below fires when `last_activity +
    // idle_timeout` passes with the slot still idle. Lives outside
    // 'reconnect: a scheduler restart (stream churn with no Assignment)
    // is still "idle" from the Job's perspective.
    let mut last_activity = tokio::time::Instant::now();

    // Process incoming scheduler messages + shutdown signal for graceful drain.
    //
    // Wrapped in a reconnect loop: on stream close/error, open a
    // fresh BuildExecution via the balanced channel (p2c routes to
    // the new leader). Running builds continue — their completions
    // land in the permanent sink, the relay buffers until the new
    // gRPC channel is swapped in. Heartbeat (separate unary RPC,
    // same balanced channel) reports running_builds to the new
    // leader within one tick; reconcile at T+45s sees the worker
    // connected + running_builds populated → no reassignment.
    // See rio-scheduler/src/actor/recovery.rs handle_reconcile_assignments.
    'reconnect: loop {
        // I-063 drain transition + I-195 idle fast-path. Hoisted to the
        // top of 'reconnect (not inside the inner select) so it fires
        // regardless of which select arm was active when SIGTERM
        // arrived: all three `shutdown.cancelled()` arms below
        // `continue 'reconnect` to reach this.
        if reconnect_drain_gate(&rt.shutdown, &rt.draining, &rt.slot, &rt.drain_done).is_break() {
            break 'reconnect;
        }

        // Fresh per-connection outbound channel. The relay pumps
        // the permanent sink into this; when the gRPC bidi dies,
        // grpc_rx (wrapped in ReceiverStream → build_execution)
        // is dropped → grpc_tx.send() fails in the relay → relay
        // buffers and waits for the next swap.
        let (grpc_tx, grpc_rx) = mpsc::channel::<ExecutorMessage>(256);

        // ExecutorRegister MUST be the first message. Send it on
        // grpc_tx directly (not via the sink — we want it to go
        // out on THIS connection, not be buffered by a relay that
        // might still be pointing at the old channel).
        grpc_tx
            .send(ExecutorMessage {
                msg: Some(executor_message::Msg::Register(ExecutorRegister {
                    executor_id: rt.build_ctx.executor_id.clone(),
                })),
            })
            .await?;

        // Swap in the new gRPC target. Relay resumes pumping.
        // send_replace because we don't care about the old value
        // (it's a dead channel or None).
        rt.relay_target_tx.send_replace(Some(grpc_tx));

        let mut build_stream = match rt
            .scheduler_client
            .build_execution(tokio_stream::wrappers::ReceiverStream::new(grpc_rx))
            .await
        {
            Ok(s) => s.into_inner(),
            Err(e) => {
                // Leader still settling, or balance channel hasn't
                // caught up. Back off and retry. The balanced
                // channel's probe loop rediscovers within ~3s.
                tracing::warn!(error = %e, "BuildExecution open failed; retrying in 1s");
                rt.relay_target_tx.send_replace(None);
                tokio::select! {
                    biased;
                    // First SIGTERM: skip the sleep, transition at the
                    // top of 'reconnect, then resume reconnecting.
                    _ = rt.shutdown.cancelled(),
                        if !rt.draining.load(Ordering::Relaxed)
                        => continue 'reconnect,
                    _ = rt.drain_done.notified() => break 'reconnect,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => continue 'reconnect,
                }
            }
        };
        info!("BuildExecution stream open");

        let stream_end = loop {
            if rt.heartbeat_handle.is_finished() {
                // bail! not exit(1): unwind the stack so fuse_session
                // (above) drops → Mount::drop → fusermount -u.
                // exit(1) would leak the mount → next start EBUSY.
                // Skip run_drain: heartbeat is the scheduler probe;
                // if it's dead, DrainExecutor won't land anyway.
                anyhow::bail!("heartbeat loop terminated unexpectedly");
            }

            tokio::select! {
                biased;

                // r[impl builder.shutdown.sigint]
                // First SIGTERM: continue to the top of 'reconnect for
                // the drain transition (set flag, spawn watcher). The
                // CURRENT stream stays open via the next iteration's
                // fresh-channel swap; in-flight completions buffered in
                // the permanent sink flush through. Guard becomes false
                // after the swap above, so this arm goes inactive —
                // `shutdown.cancelled()` would otherwise fire every
                // iteration once the token is set.
                _ = rt.shutdown.cancelled(),
                    if !rt.draining.load(Ordering::Relaxed) => {
                    continue 'reconnect;
                }

                // Drain complete: in_flight=0, all completions reported.
                // Guard: don't poll Notify before the watcher exists.
                _ = rt.drain_done.notified(),
                    if rt.draining.load(Ordering::Relaxed) => {
                    break 'reconnect;
                }

                // Single-shot exit. The watcher task spawned after
                // spawn_build_task fires this once the build's permit
                // returns.
                //
                // biased; ordering: this comes AFTER shutdown (SIGTERM
                // always wins) but BEFORE the stream arm. If a Cancel
                // arrives at the same instant the build completes, we
                // prefer to exit (the build is done; Cancel is moot).
                _ = rt.build_done.notified() => {
                    info!("build complete; exiting");
                    break StreamEnd::BuildComplete;
                }

                // I-116: idle exit. Controller spawns N Jobs for
                // queue-depth N; if the queue drains first, surplus
                // Jobs never get an Assignment. Exit cleanly instead
                // of idling to activeDeadlineSeconds.
                //
                // `sleep_until(last_activity + timeout)`: the deadline
                // shifts each time the stream arm bumps `last_activity`
                // (loop iterates → arm recreated). Guard `!is_busy()`:
                // once a build starts this arm goes inert — the
                // `build_done` arm above owns the post-build exit.
                // biased; ordering: AFTER build_done (a completed
                // build's exit wins over an idle-timeout that happens
                // to coincide).
                _ = tokio::time::sleep_until(last_activity + rt.idle_timeout),
                    if !rt.slot.is_busy() => {
                    info!(
                        idle_secs = rt.idle_timeout.as_secs(),
                        "idle timeout (no assignment); exiting"
                    );
                    break StreamEnd::BuildComplete;
                }

                msg_result = tokio_stream::StreamExt::next(&mut build_stream) => {
                    let Some(msg_result) = msg_result else {
                        break StreamEnd::Closed;
                    };
                    let msg = match msg_result {
                        Ok(m) => m,
                        Err(e) => {
                            tracing::warn!(error = %e, "build execution stream error");
                            break StreamEnd::Error;
                        }
                    };
                    // I-116: any message (Assignment, Cancel, Prefetch)
                    // counts as activity — resets the idle deadline.
                    last_activity = tokio::time::Instant::now();

                    match msg.msg {
                        Some(scheduler_message::Msg::Assignment(assignment)) => {
                            handle_assignment(assignment, &rt, &done_watcher_spawned).await;
                        }
                        Some(scheduler_message::Msg::Cancel(cancel)) => {
                            info!(
                                drv_path = %cancel.drv_path,
                                reason = %cancel.reason,
                                "received cancel signal"
                            );
                            try_cancel_build(&rt.slot, &cancel.drv_path);
                        }
                        Some(scheduler_message::Msg::Prefetch(prefetch)) => {
                            tracing::debug!(
                                paths = prefetch.store_paths.len(),
                                "received prefetch hint"
                            );
                            handle_prefetch_hint(
                                prefetch,
                                Arc::clone(&rt.prefetch.cache),
                                rt.prefetch.clients.clone(),
                                rt.prefetch.runtime.clone(),
                                Arc::clone(&rt.prefetch.sem),
                                rt.prefetch.fetch_timeout,
                                rt.test_prefetch_delay,
                                // Warm-gate ACK goes through the
                                // permanent sink (same as completions
                                // and log batches) — survives stream
                                // reconnect. A worker that warms then
                                // briefly loses its stream still
                                // delivers PrefetchComplete to the new
                                // leader once the relay reconnects.
                                rt.build_ctx.stream_tx.clone(),
                            );
                        }
                        None => {
                            tracing::warn!("received empty scheduler message");
                        }
                    }
                }
            }
        };

        match stream_end {
            StreamEnd::BuildComplete => break 'reconnect,
            StreamEnd::Closed | StreamEnd::Error => {
                // Swap relay target to None — relay buffers until
                // we open the next stream. Running builds' send()s
                // to the permanent sink succeed; the relay just
                // holds them. 256-cap sink → up to 256 messages
                // buffered before build tasks block on send. At
                // typical log rates (100ms batch flush), that's
                // ~25s of buffer — far more than the ~1s gap.
                tracing::warn!(
                    running = ?rt.build_ctx.slot.running(),
                    "BuildExecution stream ended; reconnecting (running build continues)"
                );
                rt.relay_target_tx.send_replace(None);
                tokio::select! {
                    biased;
                    _ = rt.shutdown.cancelled(),
                        if !rt.draining.load(Ordering::Relaxed)
                        => continue 'reconnect,
                    _ = rt.drain_done.notified() => break 'reconnect,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => continue 'reconnect,
                }
            }
        }
    }

    // r[impl builder.ephemeral.exit-aborts-heartbeat]
    // I-142: stop the heartbeat task FIRST, before run_drain. While
    // it's alive the scheduler sees `heartbeat-alive but stream_tx
    // closed` and keeps the executor in its map (undispatchable
    // zombie). run_drain below has no inherent timeout — if the
    // scheduler is overloaded, the heartbeat would tick indefinitely.
    // Abort is fire-and-forget: the task holds only Arcs (no Drop
    // ordering hazards), and any in-flight HeartbeatRequest is
    // harmless (scheduler tolerates a final heartbeat after Drain).
    //
    // Manual verification (no main-loop test harness): start a
    // builder against a scheduler with admin RPCs blocked
    // (iptables DROP), trigger a build → completion. Pre-fix: process
    // logs "drain complete, exiting" never appears; heartbeats keep
    // landing every 10s. Post-fix: heartbeats stop within one
    // interval; process exits at the 5s timeout below.
    rt.heartbeat_handle.abort();

    // Exit deregister. By now in_flight=0 (drain_done fired, or the
    // single build returned its permit). DrainExecutor
    // here is the explicit "I'm leaving" — heartbeat already told
    // the scheduler `draining=true` during the wait. Best-effort:
    // 50% chance of standby (I-046), but the stream-close that
    // follows (process exit drops the bidi) triggers
    // ExecutorDisconnected anyway.
    //
    // I-142: 5s hard timeout. run_drain's connect_admin + RPC have no
    // built-in timeout; an overloaded scheduler stalls this and the
    // process never reaches drop(fuse_session) below. Best-effort
    // means best-effort — log and move on.
    if tokio::time::timeout(
        Duration::from_secs(5),
        run_drain(&rt.scheduler_addr, &rt.build_ctx.executor_id),
    )
    .await
    .is_err()
    {
        tracing::debug!("DrainExecutor timed out (5s); proceeding to exit");
    }

    // r[impl builder.shutdown.fuse-abort]
    // I-165: abort the FUSE connection FIRST. The builder both serves
    // this mount (fuser threads) and consumes it (spawn_blocking
    // symlink_metadata from the warm loop). If warm-stat threads are
    // parked in the kernel's FUSE request queue when the runtime tears
    // down, they're uninterruptible — exit_group() can't reap them and
    // the process hangs (observed: main zombie + 4× D-state stat
    // threads). The fusectl abort makes the kernel return ECONNABORTED
    // to all pending requests, so the D-state threads wake BEFORE the
    // session drops and the runtime exits. Then:
    //   - drop the inner Mount → fusermount -u (lazy MNT_DETACH; with
    //     no pending requests this completes immediately)
    //   - detached fuser-bg thread sees ENODEV on /dev/fuse read
    //     → Session::run() returns → Filesystem::destroy() runs
    //     (flushes passthrough-failure stats, profraw)
    //
    // The race: main thread can reach libc exit() before the detached
    // FUSE thread processes DESTROY → destroy() never runs → profraw
    // lost for that code. The short sleep gives the FUSE thread time
    // to process DESTROY in the common case. It's best-effort — if the
    // mount is busy (fusermount fails EBUSY) or the FUSE thread is
    // stuck on a slow request, destroy() won't run. That's fine:
    // kernel unmounts on process death anyway (the fd closes); a
    // missed flush only loses profraw for this one build.
    //
    // Why not umount_and_join()? It takes self by value — if it
    // blocks (busy mount → join never returns), there's no clean way
    // to fall back to the Drop path without fuse_session ownership
    // gymnastics. The Drop path is already correct for shutdown
    // (mount cleaned up, process exits); it's only the profraw
    // flush we're optimizing for, and a sleep is sufficient.
    drop(rt.fuse_session);
    std::thread::sleep(std::time::Duration::from_millis(200));

    Ok(())
}

/// Handle a `WorkAssignment` arriving on the stream: gate (generation
/// fence, draining, slot busy), then spawn the build task and the
/// build-done watcher.
async fn handle_assignment(
    assignment: WorkAssignment,
    rt: &BuilderRuntime,
    done_watcher_spawned: &AtomicBool,
) {
    // r[impl sched.lease.generation-fence]
    // Reject assignments from a deposed leader.
    // Strictly-less (`<`): equal is the steady
    // state (generation constant per leader
    // tenure). The deposed leader's BuildExecution
    // stream stays open until its process exits;
    // this is the ONLY worker-side defense against
    // split-brain double-dispatch. No ACK sent on
    // reject — the deposed leader's actor state is
    // going away; not ACKing leaves the derivation
    // Assigned there (harmless), the NEW leader
    // re-dispatches from PG.
    let latest = rt.latest_generation.load(Ordering::Relaxed);
    if is_stale_assignment(assignment.generation, latest) {
        info!(
            drv_path = %assignment.drv_path,
            assignment_gen = assignment.generation,
            latest_gen = latest,
            "rejecting stale-generation assignment (deposed leader)"
        );
        metrics::counter!("rio_builder_stale_assignments_rejected_total").increment(1);
        return;
    }
    // I-063: belt-and-suspenders. Heartbeat
    // carries `draining` so the scheduler
    // shouldn't dispatch here, but the next
    // heartbeat is up to 10s away. No ACK
    // sent — same rationale as the stale-
    // generation reject above.
    if rt.draining.load(Ordering::Relaxed) {
        info!(
            drv_path = %assignment.drv_path,
            "rejecting assignment while draining"
        );
        return;
    }
    info!(
        drv_path = %assignment.drv_path,
        generation = assignment.generation,
        "received work assignment"
    );

    // Claim BEFORE ACKing: don't tell the
    // scheduler we accepted work we can't
    // start. P0537: one build per pod —
    // try_claim is non-blocking. If busy,
    // the scheduler dispatched while heartbeat
    // shows running_builds nonempty.
    //
    // Known harmless trigger (observed in KVM
    // test, ~300ms double-dispatch of SAME
    // drv): worker stream reconnects mid-build
    // → handle_worker_connected creates a
    // fresh executor entry with running=None
    // → became_idle inline dispatch re-picks
    // the drv before the first post-reconnect
    // heartbeat adopts it back. The TOCTOU
    // keep-logic (executor.rs handle_heartbeat)
    // covers the stale-heartbeat race; phantom
    // confirmation needs 2 heartbeats ≥10s, so
    // neither explains a 300ms window. No ACK
    // sent — same rationale as the
    // stale-generation reject above.
    let Some(guard) = rt.slot.try_claim(&assignment.drv_path) else {
        tracing::warn!(
            drv_path = %assignment.drv_path,
            running = ?rt.slot.running(),
            "rejecting assignment: slot busy \
             (scheduler dispatched while heartbeat shows running)"
        );
        return;
    };

    spawn_build_task(assignment, guard, &rt.build_ctx).await;

    // After spawning the ONE build, wait for
    // its permit to return (build complete +
    // CompletionReport sent — spawn_build_task's
    // scopeguard drops the permit after the
    // send), then exit. The select arm on
    // `build_done.notified()` breaks the inner
    // loop → outer loop breaks → run_drain
    // (no-op here: slot already idle,
    // DrainExecutor deregisters us) → FUSE
    // drop → exit 0 → pod terminates → Job
    // complete.
    //
    // Why not break immediately here: the build
    // is still RUNNING (spawn_build_task
    // returned, but the spawned task is live).
    // We need to wait for it to finish AND for
    // the CompletionReport to land in the
    // scheduler. The slot going idle is the
    // synchronization point — same mechanism
    // run_drain uses.
    //
    // Why spawn a watcher task (not inline
    // wait_idle here): inlining would block
    // the select loop, which means Cancel
    // messages wouldn't be processed while the
    // build runs. The watcher runs concurrently;
    // select still processes Cancel.
    //
    // swap(true) gates to ONCE: belt-and-
    // suspenders against a scheduler double-
    // dispatch bug — the first assignment sees
    // false and spawns; any subsequent
    // assignment sees true and skips.
    if !done_watcher_spawned.swap(true, Ordering::Relaxed) {
        let watch_slot = Arc::clone(&rt.slot);
        let done = Arc::clone(&rt.build_done);
        tokio::spawn(async move {
            watch_slot.wait_idle().await;
            done.notify_one();
        });
    }
}
/// Top-of-`'reconnect` drain handling. Called each iteration BEFORE
/// the fresh `ExecutorRegister` send.
///
/// I-063 drain transition: on the FIRST call after SIGTERM (`swap` is
/// the test-and-set), set `draining=true` and spawn the idle-watcher
/// → `drain_done` notifier. The reconnect loop then KEEPS RUNNING —
/// completions for an in-flight build reach the current leader (even
/// across scheduler restart) via the same relay machinery as steady-
/// state. Exit when `drain_done` fires (in_flight=0).
///
/// I-195 idle fast-path: returns `Break` when `draining` AND the slot
/// is idle. The reconnect-under-drain machinery exists so an in-flight
/// build's CompletionReport reaches the leader; an idle slot has
/// nothing to report. Re-registering would bump the scheduler's
/// `workers_active`, and the heartbeat task (aborted only AFTER the
/// loop exits) keeps `last_heartbeat` fresh until process exit. Under
/// coverage instrumentation the profraw atexit write delays exit by
/// ~80s (GHA 24018216226) — `cov_factor` in `lifecycle.nix` band-
/// aided that; this is the structural fix. Covers both the first
/// SIGTERM iteration with an already-idle slot AND any later
/// iteration where the slot has since gone idle (e.g. build completed
/// during a stream-retry sleep).
// r[impl builder.shutdown.idle-no-reregister]
fn reconnect_drain_gate(
    shutdown: &rio_common::signal::Token,
    draining: &std::sync::atomic::AtomicBool,
    slot: &Arc<BuildSlot>,
    drain_done: &Arc<tokio::sync::Notify>,
) -> std::ops::ControlFlow<()> {
    use std::sync::atomic::Ordering::Relaxed;
    if shutdown.is_cancelled() && !draining.swap(true, Relaxed) {
        info!(
            in_flight = u8::from(slot.is_busy()),
            "shutdown signal received, draining \
             (stream stays connected for completion reports)"
        );
        // Watcher: same wait_idle synchronization the build_done path
        // uses. Spawned (not awaited): the reconnect loop keeps the
        // stream alive; the select arms pick up the notification.
        // Spawned even when idle (the Break below skips the select):
        // wait_idle returns immediately, notify_one stores one permit
        // that nothing consumes — harmless, and keeps the busy/idle
        // paths uniform for the watcher's lifecycle.
        let watch_slot = Arc::clone(slot);
        let done = Arc::clone(drain_done);
        tokio::spawn(async move {
            watch_slot.wait_idle().await;
            done.notify_one();
        });
    }
    if draining.load(Relaxed) && !slot.is_busy() {
        info!("draining with idle slot; exiting reconnect loop without re-register");
        return std::ops::ControlFlow::Break(());
    }
    std::ops::ControlFlow::Continue(())
}

/// Exit-time deregister. The wait-for-in-flight is already done by
/// the drain watcher (or the build's permit return) before we get
/// here — this just sends DrainExecutor as an explicit goodbye.
///
/// K8s preStop sequence is now:
///   1. SIGTERM → `draining` flag set, watcher spawned
///   2. Heartbeat reports `draining=true` (worker is authority)
///   3. Reconnect loop KEEPS the stream alive — completions reach
///      whichever scheduler is leader, even across scheduler restart
///   4. In-flight=0 → drain_done fires → loop exits → here
///   5. DrainExecutor (best-effort, redundant with heartbeat)
///   6. Exit 0
///
/// terminationGracePeriodSeconds=7200 (2h). If we exceed that,
/// SIGKILL — builds lost. 2h is enough for ~any single build.
async fn run_drain(scheduler_addr: &str, executor_id: &str) {
    // I-091: scheduler_addr is the k8s Service — kube-proxy picks a
    // replica per TCP connection, so ~50% land on the standby, which
    // rejects with Unavailable("not leader"). One retry on a FRESH
    // channel (tonic reuses the HTTP/2 conn, so retrying on the same
    // client would hit the same pod) gives kube-proxy another roll.
    // Still best-effort: two standby picks in a row falls through to
    // the warn path; heartbeat already reported draining either way.
    for attempt in 0..2 {
        match rio_proto::client::connect_admin(scheduler_addr).await {
            Ok(mut admin) => {
                match admin
                    .drain_executor(rio_proto::types::DrainExecutorRequest {
                        executor_id: executor_id.to_string(),
                        force: false,
                    })
                    .await
                {
                    Ok(resp) => {
                        let r = resp.into_inner();
                        info!(
                            accepted = r.accepted,
                            running = r.running_builds,
                            "drain acknowledged by scheduler"
                        );
                        break;
                    }
                    Err(e) if e.code() == tonic::Code::Unavailable && attempt == 0 => {
                        tracing::debug!(error = %e, "DrainExecutor hit standby; reconnecting once");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "DrainExecutor RPC failed; heartbeat already reported draining");
                        break;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "admin connect failed; heartbeat already reported draining");
                break;
            }
        }
    }
    info!("drain complete, exiting");
}

/// Why the inner select loop exited. BuildComplete breaks the outer
/// reconnect loop; Closed/Error trigger a reconnect. Shutdown no
/// longer flows through here — the SIGTERM arm `continue 'reconnect`s
/// directly so the drain transition runs, then `drain_done` (in_flight
/// =0) `break 'reconnect`s directly.
enum StreamEnd {
    Closed,
    Error,
    /// One build completed (or idle timeout fired). Exit the process so
    /// the pod terminates → Job completes → ttlSecondsAfterFinished
    /// reaps it.
    BuildComplete,
}

/// Pump the permanent sink channel into the current gRPC outbound
/// channel. The target is a `watch` so the reconnect loop can swap
/// it: `Some(tx)` = connected, `None` = reconnecting (relay blocks
/// on `changed()`, sink buffers in its mpsc backlog).
///
/// Exits only when the permanent sink closes (all `sink_tx` clones
/// dropped — process shutdown).
async fn relay_loop(
    mut sink_rx: mpsc::Receiver<ExecutorMessage>,
    mut target: watch::Receiver<Option<mpsc::Sender<ExecutorMessage>>>,
) {
    // One-message buffer for the transition case: we recv'd from
    // the sink, tried to send to gRPC, gRPC channel is dead.
    // `mpsc::error::SendError<T>` holds the unsent message —
    // extract it, wait for the next target swap, retry.
    let mut buffered: Option<ExecutorMessage> = None;

    loop {
        // Wait for a live target. borrow_and_update() so the next
        // changed() fires only on an actual swap, not immediately.
        let Some(grpc_tx) = target.borrow_and_update().clone() else {
            // No target yet (startup) or mid-reconnect. Block
            // until the reconnect loop swaps one in. changed()
            // errs only if the Sender dropped — main() owns it
            // for process lifetime, so this is shutdown.
            if target.changed().await.is_err() {
                return;
            }
            continue;
        };

        // Flush the buffered message first (if any).
        if let Some(msg) = buffered.take()
            && let Err(e) = grpc_tx.send(msg).await
        {
            // Still dead (reconnect raced us). Re-buffer
            // and wait again.
            buffered = Some(e.0);
            if target.changed().await.is_err() {
                return;
            }
            continue;
        }

        // Pump until this gRPC target dies OR the reconnect loop
        // swaps the watch. `target.changed()` is the load-bearing
        // exit: `grpc_tx.send()` may keep succeeding into a zombie
        // channel (tonic's ReceiverStream can outlive the network
        // stream during graceful close), so send() alone won't
        // detect a stale target. Observed on EKS: completions
        // silently lost after scheduler failover — relay pumped
        // into the dead stream for ~20min until pod restart.
        //
        // biased; with changed() first: a pending target swap wins
        // over a pending sink message. The message stays in sink_rx
        // and goes to the NEW target on the next outer iteration.
        loop {
            tokio::select! {
                biased;
                r = target.changed() => {
                    if r.is_err() {
                        return; // watch sender dropped — shutdown
                    }
                    break; // reconnect loop swapped; re-read target
                }
                msg = sink_rx.recv() => {
                    let Some(msg) = msg else {
                        // Permanent sink closed — all sink_tx clones
                        // dropped. BuildSpawnContext holds one for
                        // process lifetime, so this is shutdown.
                        return;
                    };
                    if let Err(e) = grpc_tx.send(msg).await {
                        // gRPC channel died. Buffer this one message
                        // and go back to the top to re-read target.
                        buffered = Some(e.0);
                        break;
                    }
                }
            }
        }
    }
}

// ── bootstrap helpers (extracted from main) ──────────────────────────

/// Resolve executor_id / systems / features from config + environment.
/// Consumes the config's owned fields (caller passes via `mem::take` —
/// main() has no further use for them).
///
/// Errors if executor_id is empty AND gethostname() fails — two workers
/// with the same ID would steal each other's builds via heartbeat
/// merging, so we fail hard rather than silently colliding on "unknown".
fn resolve_executor_identity(
    executor_id: String,
    systems: Vec<String>,
    features: Vec<String>,
) -> anyhow::Result<(String, Vec<String>, Vec<String>)> {
    let executor_id = if executor_id.is_empty() {
        nix::unistd::gethostname()
            .ok()
            .and_then(|h| h.into_string().ok())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "cannot determine executor_id: gethostname() failed and \
                     executor_id not set (--worker-id, RIO_WORKER_ID, or worker.toml)"
                )
            })?
    } else {
        executor_id
    };

    // systems: auto-detect single element when not configured.
    // A worker with zero systems is useless (scheduler's can_build
    // always false) — auto-detect is a sensible default, not a
    // silent fallback for misconfiguration.
    let mut systems = if systems.is_empty() {
        vec![detect_system()]
    } else {
        systems
    };
    // r[impl sched.dispatch.fod-builtin-any-arch]
    // Every nix-daemon supports builtin:fetchurl — it's handled
    // internally, no real process forked. Bootstrap derivations
    // (busybox, bootstrap-tools) have system="builtin"; without
    // this, a cold store permanently stalls at the DAG leaves.
    // With per-arch FetcherPools, this is what makes a `builtin`
    // FOD eligible on either arch's fetchers (hard_filter matches
    // on the union; best_executor scores across both).
    if !systems.iter().any(|s| s == "builtin") {
        systems.push("builtin".to_string());
    }
    // features: no auto-detect. Empty is valid (worker supports no
    // special features). Operator sets these explicitly in the CRD
    // — auto-detecting "kvm" by checking /dev/kvm exists would be
    // surprising (worker on a kvm-capable host but operator wants
    // to reserve it for other work).

    Ok((executor_id, systems, features))
}

/// I-098: refuse to start when the host arch isn't in `RIO_SYSTEMS`.
/// A BuilderPool with `systems=[x86_64-linux]` whose pod lands on an
/// arm64 node would otherwise register as x86_64, accept x86_64 drvs,
/// and have nix-daemon refuse them at build time. CrashLoopBackOff is
/// the right shape — visible in `kubectl get pods`, doesn't poison drvs.
///
/// Fetchers skip this: FODs are `builtin` (arch-agnostic) so a fetcher
/// on the "wrong" arch is fine — and intentional (cheaper Gravitons).
/// `host` is a parameter (not `detect_system()` inline) for testability.
fn validate_host_arch(
    kind: rio_proto::types::ExecutorKind,
    systems: &[String],
    host: &str,
) -> anyhow::Result<()> {
    use rio_proto::types::ExecutorKind;
    if kind == ExecutorKind::Fetcher {
        return Ok(());
    }
    let mut non_builtin = systems.iter().filter(|s| s.as_str() != "builtin");
    if non_builtin.clone().next().is_none() {
        return Ok(());
    }
    if non_builtin.any(|s| s == host) {
        return Ok(());
    }
    anyhow::bail!(
        "host system {host:?} not in RIO_SYSTEMS={systems:?} — pod likely \
         scheduled onto wrong-arch node. Fix the pool's nodeSelector or \
         systems list."
    )
}

/// cgroup v2 setup + background utilization reporter spawn.
///
/// HARD REQUIREMENT — `?` on both delegated_root and
/// enable_subtree_controllers. Fail startup loudly rather than silently
/// fall back to broken metrics (the phase2c VmHWM bug measured ~10MB
/// for every build; poisoning build_history like that takes ~10 EMA
/// cycles to wash out).
///
/// `delegated_root()` returns the PARENT of /proc/self/cgroup — NOT
/// own_cgroup(). cgroup v2's no-internal-processes rule means per-build
/// cgroups must be SIBLINGS of where the worker process is, not
/// children. systemd DelegateSubgroup=builds puts the worker in
/// .../service/builds/; delegated_root() returns .../service/ (empty,
/// writable via Delegate=yes); per-build cgroups go there as siblings
/// of builds/.
///
/// `enable_subtree_controllers` writes +memory +cpu (fails on EACCES =
/// Delegate=yes not configured).
fn init_cgroup(
    overlay_base_dir: &std::path::Path,
    shutdown: rio_common::signal::Token,
) -> anyhow::Result<(std::path::PathBuf, crate::cgroup::ResourceSnapshotHandle)> {
    let cgroup_parent =
        crate::cgroup::delegated_root().map_err(|e| anyhow::anyhow!("cgroup v2 required: {e}"))?;
    crate::cgroup::enable_subtree_controllers(&cgroup_parent)
        .map_err(|e| anyhow::anyhow!("cgroup delegation required: {e}"))?;
    info!(cgroup = %cgroup_parent.display(), "cgroup v2 subtree ready");

    // Background utilization reporter: polls parent cgroup cpu.stat +
    // memory.current/max every 10s → Prometheus gauges AND the shared
    // snapshot the heartbeat loop reads for ResourceUsage. Single
    // sampling site means Prometheus and ListExecutors always agree.
    // Shutdown token lets the 10s sleep break immediately on SIGTERM
    // so main() can return and profraw flush.
    let resource_snapshot: crate::cgroup::ResourceSnapshotHandle = Default::default();
    rio_common::task::spawn_monitored(
        "cgroup-utilization-reporter",
        crate::cgroup::utilization_reporter_loop_with_shutdown(
            cgroup_parent.clone(),
            overlay_base_dir.to_path_buf(),
            std::sync::Arc::clone(&resource_snapshot),
            shutdown,
        ),
    );

    Ok((cgroup_parent, resource_snapshot))
}

use crate::fuse::StoreClients;
type WorkerClient = rio_proto::ExecutorServiceClient<tonic::transport::Channel>;
/// Probe-loop guards for both balanced channels. Dropping a
/// `BalancedChannel` stops its probe loop, so these must outlive the
/// clients. Either can be `None` (single-channel fallback).
type BalanceGuards = (
    Option<rio_proto::client::balance::BalancedChannel>,
    Option<rio_proto::client::balance::BalancedChannel>,
);

/// Retry-until-connected store + scheduler clients via
/// [`connect_with_retry`](rio_proto::client::connect_with_retry)
/// (shutdown-aware, exponential backoff).
///
/// Cold-start race: store/scheduler Services may have no endpoints
/// yet. /healthz stays 200 (process IS alive, restart won't help),
/// /readyz stays 503 (ready flag won't flip until first heartbeat
/// accepted, far past this loop).
///
/// Returns `None` if shutdown fires during retry — caller exits
/// main() cleanly (nothing to drain, never connected).
///
/// Scheduler has two modes:
/// - Balanced (K8s, multi-replica): DNS-resolve headless Service,
///   health-probe pod IPs, route to leader. Heartbeat routes through
///   the same balanced channel — leadership flip detected within one
///   probe tick (~3s).
/// - Single (non-K8s): plain connect. VM tests use this.
async fn connect_upstreams(
    cfg: &crate::config::Config,
    shutdown: &rio_common::signal::Token,
) -> Option<(StoreClients, WorkerClient, BalanceGuards)> {
    rio_proto::client::connect_with_retry(
        shutdown,
        || async {
            // dataplane2: build StoreService + ChunkService over the SAME
            // channel so per-chunk GetChunk RPCs p2c-fan across the same
            // SERVING replicas as GetPath. `connect_raw` returns the bare
            // Channel; StoreClients wraps it in BOTH typed clients.
            let (ch, store_guard) =
                rio_proto::client::connect_raw::<rio_proto::StoreServiceClient<_>>(&cfg.store)
                    .await?;
            let store = StoreClients::from_channel(ch);
            let (sched, sched_guard) = rio_proto::client::connect(&cfg.scheduler).await?;
            anyhow::Ok((store, sched, (store_guard, sched_guard)))
        },
        None,
    )
    .await
    .ok()
}

/// Inputs to the heartbeat loop. Grouped so main() doesn't grow 12
/// `let heartbeat_* = ...` prelude lines before the spawn.
struct HeartbeatCtx {
    executor_id: String,
    executor_kind: rio_proto::types::ExecutorKind,
    systems: Vec<String>,
    features: Vec<String>,
    size_class: String,
    slot: Arc<BuildSlot>,
    ready: Arc<std::sync::atomic::AtomicBool>,
    resources: crate::cgroup::ResourceSnapshotHandle,
    circuit: Arc<crate::fuse::circuit::CircuitBreaker>,
    draining: Arc<std::sync::atomic::AtomicBool>,
    generation: Arc<std::sync::atomic::AtomicU64>,
    client: WorkerClient,
}

/// Spawn the heartbeat loop. A panicking heartbeat loop leaves the
/// worker silently alive but unreachable from the scheduler's
/// perspective — the scheduler times it out and re-dispatches its
/// builds to another worker, leading to duplicate builds. Wrap in
/// spawn_monitored so panics are logged; main() checks
/// `handle.is_finished()` each select-loop iteration.
fn spawn_heartbeat(ctx: HeartbeatCtx) -> tokio::task::JoinHandle<()> {
    let HeartbeatCtx {
        executor_id,
        executor_kind,
        systems,
        features,
        size_class,
        slot,
        ready,
        resources,
        circuit,
        draining,
        generation,
        mut client,
    } = ctx;
    rio_common::task::spawn_monitored("heartbeat-loop", async move {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        loop {
            interval.tick().await;

            let request = build_heartbeat_request(
                &executor_id,
                executor_kind,
                &systems,
                &features,
                &size_class,
                &slot,
                &resources,
                circuit.is_open(),
                draining.load(std::sync::atomic::Ordering::Relaxed),
            )
            .await;

            match client.heartbeat(request).await {
                Ok(response) => {
                    let resp = response.into_inner();
                    if resp.accepted {
                        // READY. Set unconditionally — it's idempotent
                        // (already-true → true is a no-op at the atomic
                        // level) and cheaper than a load-then-store.
                        ready.store(true, std::sync::atomic::Ordering::Relaxed);
                        // r[impl sched.lease.generation-fence]
                        // fetch_max, not store: during the 15s Lease TTL
                        // split-brain window (r[sched.lease.k8s-lease]),
                        // both old and new leader answer with accepted=true.
                        // If responses interleave new-then-old, `store`
                        // would REGRESS the fence and let through exactly
                        // the stale assignment we're blocking. fetch_max
                        // is monotone regardless of response ordering.
                        generation.fetch_max(resp.generation, std::sync::atomic::Ordering::Relaxed);
                    } else {
                        tracing::warn!("heartbeat rejected by scheduler");
                        // NOT READY: scheduler is reachable but rejecting
                        // us. Could mean the scheduler doesn't recognize
                        // our executor_id (stale registration, scheduler
                        // restarted and lost in-memory state). The
                        // BuildExecution stream reconnect logic handles
                        // the actual recovery; readiness flag just
                        // reflects the current state.
                        ready.store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "heartbeat failed");
                    // NOT READY: gRPC error. Scheduler unreachable or
                    // overloaded. Don't flip liveness (restarting won't
                    // fix the network) but do flip readiness. The next
                    // successful heartbeat flips back — this tracks
                    // the scheduler's availability from our perspective.
                    ready.store(false, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_proto::types::ExecutorRegister;

    #[test]
    fn validate_host_arch_gates_builders_only() {
        use rio_proto::types::ExecutorKind::{Builder, Fetcher};
        let s = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };

        // builder: host must be in systems (excluding builtin)
        assert!(
            validate_host_arch(Builder, &s(&["x86_64-linux", "builtin"]), "x86_64-linux").is_ok()
        );
        assert!(
            validate_host_arch(Builder, &s(&["x86_64-linux", "builtin"]), "aarch64-linux").is_err(),
            "I-098: arm64 host with x86_64-only RIO_SYSTEMS must refuse"
        );
        assert!(
            validate_host_arch(
                Builder,
                &s(&["x86_64-linux", "aarch64-linux"]),
                "aarch64-linux"
            )
            .is_ok(),
            "multi-arch pool accepts either"
        );
        // builtin-only → no constraint (auto-detect path adds host first
        // anyway, but defensive)
        assert!(validate_host_arch(Builder, &s(&["builtin"]), "aarch64-linux").is_ok());

        // fetcher: never validates (FODs are arch-agnostic)
        assert!(
            validate_host_arch(Fetcher, &s(&["x86_64-linux", "builtin"]), "aarch64-linux").is_ok(),
            "fetcher on wrong arch is intentional (cheap Gravitons)"
        );
    }

    // -----------------------------------------------------------------------

    fn msg(id: &str) -> ExecutorMessage {
        ExecutorMessage {
            msg: Some(executor_message::Msg::Register(ExecutorRegister {
                executor_id: id.into(),
            })),
        }
    }

    /// Relay pumps sink → gRPC. When gRPC dies, relay buffers ONE
    /// message (from SendError) and blocks until a new target is
    /// swapped in. Messages in the sink's mpsc backlog are held
    /// (build tasks block on sink.send at cap, but don't see Err).
    #[tokio::test]
    async fn relay_survives_target_swap() {
        let (sink_tx, sink_rx) = mpsc::channel(8);
        let (target_tx, target_rx) = watch::channel(None);
        let relay = tokio::spawn(relay_loop(sink_rx, target_rx));

        // Connect target #1.
        let (grpc1_tx, mut grpc1_rx) = mpsc::channel(8);
        target_tx.send_replace(Some(grpc1_tx));

        // Send via sink → relay pumps → arrives at grpc1.
        sink_tx.send(msg("a")).await.unwrap();
        let r = tokio::time::timeout(Duration::from_secs(1), grpc1_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            r.msg,
            Some(executor_message::Msg::Register(ExecutorRegister { executor_id })) if executor_id == "a"
        ));

        // Kill target #1 (drop rx → tx.send() fails in relay).
        // Then send "b" — relay recv's it, send fails, buffers it.
        drop(grpc1_rx);
        sink_tx.send(msg("b")).await.unwrap();
        // Also queue "c" in the sink backlog (relay is blocked on
        // target.changed(), hasn't recv'd yet).
        sink_tx.send(msg("c")).await.unwrap();

        // Brief yield so relay has a chance to recv "b", hit the
        // dead channel, and buffer.
        tokio::task::yield_now().await;

        // Swap in target #2. Relay flushes "b" (buffered) then
        // resumes pumping "c" from the sink.
        let (grpc2_tx, mut grpc2_rx) = mpsc::channel(8);
        target_tx.send_replace(Some(grpc2_tx));

        let r = tokio::time::timeout(Duration::from_secs(1), grpc2_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            r.msg,
            Some(executor_message::Msg::Register(ExecutorRegister { executor_id })) if executor_id == "b"
        ));
        let r = tokio::time::timeout(Duration::from_secs(1), grpc2_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            r.msg,
            Some(executor_message::Msg::Register(ExecutorRegister { executor_id })) if executor_id == "c"
        ));

        // Cleanup: drop sink → relay exits.
        drop(sink_tx);
        tokio::time::timeout(Duration::from_secs(1), relay)
            .await
            .unwrap()
            .unwrap();
    }

    /// Relay swaps to new target on `watch.changed()` even when the
    /// OLD target's receiver is still alive. Regression for I-032:
    /// tonic's ReceiverStream can outlive the network stream during
    /// graceful close, so `grpc_tx.send()` keeps succeeding into a
    /// zombie. Before the fix, relay only broke the pump loop on
    /// SendError — completions pumped into the dead stream after
    /// scheduler failover. Observed on EKS: 4 fetchers each did ONE
    /// build then stalled forever (`running_builds` never freed).
    ///
    /// Key difference from `relay_survives_target_swap`: grpc1_rx
    /// is NOT dropped before the swap. The relay must notice via
    /// `target.changed()`, not via send-failure.
    #[tokio::test]
    async fn relay_swaps_on_watch_change_with_live_old_target() {
        let (sink_tx, sink_rx) = mpsc::channel(8);
        let (target_tx, target_rx) = watch::channel(None);
        let relay = tokio::spawn(relay_loop(sink_rx, target_rx));

        let (grpc1_tx, mut grpc1_rx) = mpsc::channel(8);
        target_tx.send_replace(Some(grpc1_tx));
        sink_tx.send(msg("a")).await.unwrap();
        let r = tokio::time::timeout(Duration::from_secs(1), grpc1_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            r.msg,
            Some(executor_message::Msg::Register(ExecutorRegister { executor_id })) if executor_id == "a"
        ));

        // Swap to target #2. CRITICAL: grpc1_rx is still in scope
        // and alive. Pre-fix: relay's pump loop doesn't watch the
        // target — grpc1_tx.send() succeeds, message lands in
        // grpc1_rx, grpc2 never sees it.
        let (grpc2_tx, mut grpc2_rx) = mpsc::channel(8);
        target_tx.send_replace(Some(grpc2_tx));

        // Give relay a chance to observe target.changed() and break
        // the pump loop. yield_now is too tight (single scheduler
        // quantum); 10ms is generous without slowing the suite.
        tokio::time::sleep(Duration::from_millis(10)).await;

        sink_tx.send(msg("b")).await.unwrap();
        let r = tokio::time::timeout(Duration::from_secs(1), grpc2_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                r.msg,
                Some(executor_message::Msg::Register(ExecutorRegister { executor_id })) if executor_id == "b"
            ),
            "message must route to new target after watch swap"
        );

        // grpc1 must have no message. Disconnected is expected: once
        // relay re-reads the watch, its clone of grpc1_tx drops (the
        // outer let binding goes out of scope on the next iteration);
        // send_replace already dropped the original. So grpc1 is fully
        // closed — which is exactly right: the stale target is dead.
        // Empty would also be fine (relay hasn't re-read yet) but is
        // a less complete state. Ok(_) is the bug — message misrouted.
        assert!(
            grpc1_rx.try_recv().is_err(),
            "stale target must not receive messages after watch swap"
        );
        drop(sink_tx);
        tokio::time::timeout(Duration::from_secs(1), relay)
            .await
            .unwrap()
            .unwrap();
    }

    /// The drain-wait synchronization: `BuildSlot::wait_idle` returns
    /// exactly when the in-flight build task drops its `BuildSlotGuard`.
    /// Missed-notification-safe: the watcher may be spawned before OR
    /// after the guard is taken.
    ///
    /// Not testing SIGTERM-to-self: signal delivery under cargo test
    /// is nondeterministic, and nextest's per-process model means a
    /// stray SIGTERM kills the test binary. vm-phase3a does real
    /// SIGTERM via `k3s kubectl delete pod`.
    #[tokio::test]
    async fn drain_wait_slot_synchronization() {
        let slot = Arc::new(BuildSlot::default());

        // Idle slot: wait_idle returns immediately.
        tokio::time::timeout(Duration::from_secs(1), slot.wait_idle())
            .await
            .expect("idle slot → wait_idle returns immediately");

        let guard = slot.try_claim("/nix/store/aaa-x.drv").unwrap();
        assert!(slot.try_claim("/nix/store/bbb-y.drv").is_none(), "busy");
        assert_eq!(slot.running().as_deref(), Some("/nix/store/aaa-x.drv"));

        // Watcher spawned WHILE busy parks until guard drops.
        let watch_slot = Arc::clone(&slot);
        let drain = tokio::spawn(async move { watch_slot.wait_idle().await });
        tokio::task::yield_now().await;
        assert!(!drain.is_finished(), "guard held → wait_idle parked");

        drop(guard);
        tokio::time::timeout(Duration::from_secs(2), drain)
            .await
            .expect("wait_idle wakes when guard drops")
            .expect("watcher didn't panic");
        assert!(slot.running().is_none());
    }

    /// I-195: SIGTERM with an idle slot must NOT re-register. The
    /// reconnect-under-drain machinery exists for in-flight
    /// CompletionReports; an idle slot has nothing to drain. `Break`
    /// here means the call site `break 'reconnect`s BEFORE the
    /// `ExecutorRegister` send → scheduler never sees a spurious
    /// `workers_active` bump that lingers through profraw atexit.
    ///
    /// `#[tokio::test]` for the watcher `tokio::spawn` inside the
    /// gate; the gate function itself is sync.
    // r[verify builder.shutdown.idle-no-reregister]
    #[tokio::test]
    async fn drain_gate_idle_slot_breaks_without_reregister() {
        use std::ops::ControlFlow;
        use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

        let shutdown = rio_common::signal::Token::new();
        let draining = AtomicBool::new(false);
        let slot = Arc::new(BuildSlot::default());
        let drain_done = Arc::new(tokio::sync::Notify::new());

        // Steady state: no SIGTERM → Continue (proceed to Register).
        assert_eq!(
            reconnect_drain_gate(&shutdown, &draining, &slot, &drain_done),
            ControlFlow::Continue(()),
            "no SIGTERM → reconnect loop opens stream as normal"
        );
        assert!(!draining.load(Relaxed));

        // SIGTERM, idle slot → Break on the FIRST gate call. This is
        // the I-195 fix: pre-fix, the loop would Continue here, send
        // ExecutorRegister, THEN break via drain_done.
        shutdown.cancel();
        assert_eq!(
            reconnect_drain_gate(&shutdown, &draining, &slot, &drain_done),
            ControlFlow::Break(()),
            "SIGTERM + idle slot → break before ExecutorRegister"
        );
        assert!(draining.load(Relaxed), "drain transition still fires");

        // SIGTERM, BUSY slot → Continue (in-flight build needs the
        // stream for its CompletionReport).
        let shutdown2 = rio_common::signal::Token::new();
        let draining2 = AtomicBool::new(false);
        let slot2 = Arc::new(BuildSlot::default());
        let guard = slot2.try_claim("/nix/store/aaa-x.drv").unwrap();
        shutdown2.cancel();
        assert_eq!(
            reconnect_drain_gate(&shutdown2, &draining2, &slot2, &drain_done),
            ControlFlow::Continue(()),
            "SIGTERM + busy slot → keep stream open for completion"
        );
        assert!(draining2.load(Relaxed));

        // Subsequent iteration: build completed during a retry sleep,
        // slot now idle, draining already true → Break (don't
        // re-register just to immediately drain_done).
        drop(guard);
        assert_eq!(
            reconnect_drain_gate(&shutdown2, &draining2, &slot2, &drain_done),
            ControlFlow::Break(()),
            "draining + slot went idle on later iteration → break"
        );
    }

    /// Missed-notification race: guard drops BETWEEN the watcher's
    /// `is_busy()` check and its `notified().await`. Exercises the
    /// `enable()` ordering in `BuildSlot::wait_idle`.
    #[tokio::test(start_paused = true)]
    async fn slot_wait_idle_no_missed_notification() {
        let slot = Arc::new(BuildSlot::default());
        let guard = slot.try_claim("/nix/store/ccc-z.drv").unwrap();

        let watch_slot = Arc::clone(&slot);
        let drain = tokio::spawn(async move { watch_slot.wait_idle().await });
        // Drop the guard BEFORE the watcher gets scheduled. With a
        // naive `if busy { notified().await }`, notify_waiters() would
        // fire into the void here and the watcher would park forever.
        drop(guard);

        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .expect("enable() before is_busy() avoids the missed-notification race")
            .expect("watcher didn't panic");
    }

    // -----------------------------------------------------------------------
    // I-116: idle timeout
    // -----------------------------------------------------------------------

    /// I-116 scenario: scheduler never dispatches → idle-timeout arm
    /// fires. Mirrors the select-arm guard + `sleep_until` shape in
    /// main()'s event loop (the loop itself is not extractable —
    /// FUSE/gRPC entanglement — so this reproduces the two arms that
    /// matter under paused time).
    #[tokio::test(start_paused = true)]
    async fn idle_timeout_fires_with_no_assignment() {
        let slot = Arc::new(BuildSlot::default());
        let idle_timeout = Duration::from_secs(120);
        let last_activity = tokio::time::Instant::now();

        // Stream that never yields — a scheduler that never dispatches.
        let stream = std::future::pending::<()>();
        tokio::pin!(stream);

        let fired = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(last_activity + idle_timeout),
                if !slot.is_busy() => true,
            _ = &mut stream => false,
        };
        assert!(fired, "idle slot + silent stream → timeout fires");
        assert!(
            tokio::time::Instant::now() >= last_activity + idle_timeout,
            "auto-advance reached the deadline"
        );
    }

    /// I-116 scenario: assignment received → slot busy → guard is
    /// false → idle-timeout arm inert for the entire build, even past
    /// the 120s deadline. The post-build exit is `build_done`'s
    /// job (covered by `drain_wait_slot_synchronization`).
    #[tokio::test(start_paused = true)]
    async fn idle_timeout_inert_while_building() {
        let slot = Arc::new(BuildSlot::default());
        let idle_timeout = Duration::from_secs(120);
        let last_activity = tokio::time::Instant::now();

        // Assignment received: try_claim succeeded (main()'s assignment
        // handler does this before the next select iteration).
        let _guard = slot.try_claim("/nix/store/aaa-building.drv").unwrap();

        // Build runs well past idle_timeout.
        let build = tokio::time::sleep(Duration::from_secs(300));
        tokio::pin!(build);

        let fired = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(last_activity + idle_timeout),
                if !slot.is_busy() => true,
            _ = &mut build => false,
        };
        assert!(
            !fired,
            "busy slot → guard false → idle-timeout arm disabled; \
             300s build runs to completion past the 120s deadline"
        );
    }

    /// I-116 scenario: a message arrives at t=100s (resets
    /// `last_activity`) → the original t=120s deadline does NOT fire;
    /// the new deadline is t=220s. Proves the "idle, not total
    /// lifetime" semantics — `sleep_until` is recreated each loop
    /// iteration with the bumped `last_activity`.
    #[tokio::test(start_paused = true)]
    async fn idle_timeout_resets_on_message() {
        let slot = Arc::new(BuildSlot::default());
        let idle_timeout = Duration::from_secs(120);
        let start = tokio::time::Instant::now();
        let mut last_activity = start;

        // One message at t=100s (a Prefetch hint, say), then silence.
        // Once delivered, the "stream" goes pending forever — same as
        // a scheduler that goes quiet after one message.
        let (msg_tx, mut msg_rx) = mpsc::channel::<()>(1);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(100)).await;
            msg_tx.send(()).await.unwrap();
            // hold tx open: recv() stays pending (not None) after this
            std::future::pending::<()>().await;
        });

        let fired_at = loop {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(last_activity + idle_timeout),
                    if !slot.is_busy() => {
                    break tokio::time::Instant::now();
                }
                Some(()) = msg_rx.recv() => {
                    last_activity = tokio::time::Instant::now();
                }
            }
        };
        assert!(
            fired_at >= start + Duration::from_secs(220),
            "message at t=100s resets deadline → fires at t=220s, not t=120s \
             (fired at +{:?})",
            fired_at - start
        );
    }

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
            crate::fuse::cache::Cache::new(dir.path().to_path_buf())
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
            Duration::ZERO,
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
            crate::fuse::cache::Cache::new(dir.path().to_path_buf())
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
            Duration::ZERO,
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
            crate::fuse::cache::Cache::new(dir.path().to_path_buf())
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
            Duration::ZERO,
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
