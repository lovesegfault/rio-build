//! Worker runtime: the `'reconnect` event loop and build-task spawning.
//!
//! Glue between main.rs's bootstrap and the executor/FUSE/upload
//! subsystems. [`setup`] wires up cgroups, gRPC clients, FUSE mount,
//! relay, heartbeat; [`run`] drives the reconnect loop until the single
//! build completes or drain finishes.
//!
//! Submodules (each a clean extraction with no cross-cutting state):
//! - `slot`: single-build occupancy + cancel target
//! - `heartbeat`: heartbeat construction + spawn loop
//! - `prefetch`: PrefetchHint handling + warm-gate ACK
//! - `setup`: cold-start wiring (identity, cgroup, connect, FUSE)
//! - `drain`: SIGTERM drain gate + exit-time DrainExecutor RPC

mod drain;
mod heartbeat;
mod prefetch;
mod result;
mod setup;
mod slot;

pub use heartbeat::build_heartbeat_request;
pub use prefetch::handle_prefetch_hint;
pub use setup::setup;
pub use slot::{BuildSlot, BuildSlotGuard, try_cancel_build};

use drain::{reconnect_drain_gate, run_drain};
use prefetch::PrefetchDeps;
use result::{err_completion, ok_completion, outcome_label, panic_completion};
use setup::{BalanceGuards, WorkerClient};

use crate::cgroup::ResourceSnapshotHandle;

// Test-only re-exports: the `mod tests` block below predates the
// submodule split and pulls everything via `super::*`.
#[cfg(test)]
use {
    prefetch::PREFETCH_WARM_SIZE_CAP_BYTES, rio_proto::types::PrefetchHint,
    setup::validate_host_arch, tokio::sync::Semaphore,
};

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{Notify, mpsc, watch};
use tracing::{Instrument, info, instrument};

use rio_proto::types::{
    ExecutorMessage, ExecutorRegister, ProgressUpdate, ResourceUsage, WorkAssignment,
    WorkAssignmentAck, executor_message, scheduler_message,
};

use crate::{executor, log_stream};

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

/// Shared context for spawning build tasks.
///
/// Constructed once before the event loop to reduce per-assignment clone
/// boilerplate. `spawn_build_task` clones only what each spawned task needs.
#[derive(Clone)]
pub struct BuildSpawnContext {
    /// `StoreService` over the balanced channel. `.store` goes to
    /// `execute_build` (drv fetch, upload, query); the full bundle is
    /// held by `NixStoreFs` (set at FUSE mount) so the JIT `lookup`
    /// callback can reach it.
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
    /// Silence timeout default (from `Config.max_silent_time`).
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
    /// Advertised target systems (resolved `RIO_SYSTEMS`). Threaded to
    /// `setup_nix_conf` so the per-build daemon's `extra-platforms`
    /// matches what the heartbeat told the scheduler — a drv routed for
    /// `i686-linux` is then accepted by the x86_64 daemon.
    pub systems: Arc<[String]>,
    /// Handle to the FUSE local cache. Threaded into `ExecutorEnv` so
    /// the executor can `register_inputs` (JIT allowlist) and
    /// `prefetch_manifests` (I-110c) before daemon spawn.
    pub fuse_cache: Arc<crate::fuse::cache::Cache>,
    /// Base per-fetch gRPC timeout for the FUSE cache's `GetPath`.
    /// JIT lookup scales it per path via `jit_fetch_timeout(this,
    /// nar_size)` (I-178). Same value passed to
    /// [`handle_prefetch_hint`].
    pub fuse_fetch_timeout: Duration,
    /// k8s `spec.nodeName` (from `Config.node_name`, downward API).
    /// Attached to every `CompletionReport` for ADR-023's hw_class
    /// join. `None` outside k8s (empty config string).
    pub node_name: Option<String>,
    /// Shared handle to the cgroup-poll [`ResourceUsage`] snapshot
    /// (same `Arc` as the heartbeat loop). Read once at completion
    /// time to populate `CompletionReport.final_resources` so the
    /// scheduler's `build_samples` writer has the ADR-023 telemetry
    /// without depending on best-effort `ProgressUpdate` delivery.
    pub resources: ResourceSnapshotHandle,
}

impl BuildSpawnContext {
    /// Per-worker fields stamped onto every [`CompletionReport`]
    /// (success, error, and panic paths). Read once at completion
    /// time so the snapshot reflects the build's final cgroup state.
    fn completion_stamp(&self) -> result::CompletionStamp {
        result::CompletionStamp {
            node_name: self.node_name.clone(),
            final_resources: Some(*self.resources.read().unwrap_or_else(|e| e.into_inner())),
        }
    }
}

impl BuildSpawnContext {
    /// Project the per-worker config fields into an [`executor::ExecutorEnv`].
    ///
    /// `BuildSpawnContext` is the runtime-layer bundle (Arc-shared
    /// stream/slot handles + config); `ExecutorEnv` is the executor-layer
    /// view (config + per-build cancel flag, no stream/slot). Keeping the
    /// field copy in one place means a new config field only needs wiring
    /// here, not at every `execute_build` call site.
    pub fn executor_env(&self, cancelled: Arc<AtomicBool>) -> executor::ExecutorEnv {
        executor::ExecutorEnv {
            fuse_mount_point: self.fuse_mount_point.clone(),
            overlay_base_dir: self.overlay_base_dir.clone(),
            executor_id: self.executor_id.clone(),
            log_limits: self.log_limits,
            daemon_timeout: self.daemon_timeout,
            max_silent_time: self.max_silent_time,
            cgroup_parent: self.cgroup_parent.clone(),
            executor_kind: self.executor_kind,
            systems: Arc::clone(&self.systems),
            fuse_cache: Some(Arc::clone(&self.fuse_cache)),
            fuse_fetch_timeout: self.fuse_fetch_timeout,
            cancelled,
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

    // Record the cgroup path on the slot. We know it deterministically:
    // cgroup_parent/sanitize_build_id(drv_path). execute_build creates
    // it AFTER spawning the daemon (needs PID); we record PREDICTIVELY
    // here so a Cancel arriving early still finds it. If Cancel arrives
    // BEFORE the cgroup exists, cgroup.kill → ENOENT → try_cancel_build
    // leaves the flag SET; execute_build polls it during prefetch+warm
    // and aborts pre-daemon-spawn (I-166).
    //
    // The cancelled flag itself was created at try_claim time (lives in
    // the slot AND the guard); read below in the Err arm to distinguish
    // "cancelled" (user intent, Cancelled status) from "executor failed"
    // (infra issue, InfrastructureFailure status).
    let build_id = executor::sanitize_build_id(&drv_path);
    let build_cgroup_path = ctx.cgroup_parent.join(&build_id);
    let cancelled = guard.cancelled();
    ctx.slot.set_cgroup_path(build_cgroup_path.clone());

    // Clone for the panic handler before moving ctx into the task.
    let panic_tx = ctx.stream_tx.clone();
    let panic_drv_path = drv_path.clone();
    let panic_token = assignment_token.clone();
    let panic_node_name = ctx.node_name.clone();
    let panic_resources = Arc::clone(&ctx.resources);

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
        // Same Arc as the slot's cancel flag. execute_build polls it
        // during the pre-cgroup phase (I-166).
        let build_env = ctx.executor_env(Arc::clone(&cancelled));

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
        // r[impl builder.retry.daemon-transient]
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
        // (cgroup memory.peak + polled cpu.stat). On Err, the cancel flag
        // is read BEFORE deciding the status — Acquire pairs with
        // try_cancel_build's Release (not strictly needed, no other state
        // to synchronize, but cheap and documents the pairing).
        let stamp = ctx.completion_stamp();
        let completion = match result {
            Ok(exec_result) => ok_completion(exec_result, stamp),
            Err(e) => err_completion(
                &e,
                drv_path,
                assignment_token,
                cancelled.load(std::sync::atomic::Ordering::Acquire),
                stamp,
            ),
        };

        metrics::counter!("rio_builder_builds_total", "outcome" => outcome_label(&completion))
            .increment(1);

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
            let msg = ExecutorMessage {
                msg: Some(executor_message::Msg::Completion(panic_completion(
                    panic_drv_path.clone(),
                    panic_token,
                    result::CompletionStamp {
                        node_name: panic_node_name,
                        final_resources: Some(
                            *panic_resources.read().unwrap_or_else(|e| e.into_inner()),
                        ),
                    },
                ))),
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
    /// Probe-loop guards for both balanced channels. Held for process
    /// lifetime (dropping a `BalancedChannel` stops its probe loop).
    _balance_guard: BalanceGuards,
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
    // same balanced channel) reports running_build to the new
    // leader within one tick; reconcile at T+45s sees the worker
    // connected + running_build populated → no reassignment.
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
                match reconnect_backoff(&rt).await {
                    std::ops::ControlFlow::Continue(()) => continue 'reconnect,
                    std::ops::ControlFlow::Break(()) => break 'reconnect,
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
                // r[impl builder.idle-exit]
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
                match reconnect_backoff(&rt).await {
                    std::ops::ControlFlow::Continue(()) => continue 'reconnect,
                    std::ops::ControlFlow::Break(()) => break 'reconnect,
                }
            }
        }
    }

    run_teardown(rt).await;
    Ok(())
}

/// 1s reconnect backoff with shutdown/drain interruption. Used by both
/// the stream-open-failed and stream-ended paths in [`run`].
///
/// `Continue` → retry the connect at the top of `'reconnect` (also
/// taken on first SIGTERM, so the drain transition runs there before
/// resuming reconnection). `Break` → drain finished, exit `'reconnect`.
async fn reconnect_backoff(rt: &BuilderRuntime) -> std::ops::ControlFlow<()> {
    tokio::select! {
        biased;
        // First SIGTERM: skip the sleep, transition at the top of
        // 'reconnect, then resume reconnecting.
        _ = rt.shutdown.cancelled(),
            if !rt.draining.load(Ordering::Relaxed)
            => std::ops::ControlFlow::Continue(()),
        _ = rt.drain_done.notified() => std::ops::ControlFlow::Break(()),
        _ = tokio::time::sleep(Duration::from_secs(1)) => std::ops::ControlFlow::Continue(()),
    }
}

/// Exit teardown after `'reconnect` breaks: heartbeat abort,
/// `DrainExecutor`, FUSE abort. By now `in_flight=0` (drain_done fired,
/// or the single build returned its permit).
async fn run_teardown(rt: BuilderRuntime) {
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
    // I-142: 5s hard timeout. run_drain's connect + RPC have no
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
    // shows running_build set.
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
// r[impl builder.relay.reconnect]
pub(super) async fn relay_loop(
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

#[cfg(test)]
mod tests {
    use super::*;
    use rio_proto::types::ExecutorRegister;
    use rstest::rstest;

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
    /// build then stalled forever (`running_build` never freed).
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
    // r[verify builder.idle-exit]
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

        let slot = Arc::new(BuildSlot::default());
        let g = slot.try_claim("/nix/store/test.drv").unwrap();
        let cancelled = g.cancelled();
        slot.set_cgroup_path(cgroup_path.clone());

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
        slot.set_cgroup_path(PathBuf::from("/nope"));
        assert!(
            !try_cancel_build(&slot, "/nix/store/absent.drv"),
            "drv mismatch → false (stale CancelSignal guard)"
        );
    }

    /// Corr#3 regression: cancel arrives between `try_claim` and
    /// `set_cgroup_path`. Previously the cancel target lived under a
    /// separate mutex and was `None` here → `try_cancel_build` returned
    /// `false` and the cancel was lost. Now the flag is created at claim
    /// time, so the cancel lands.
    #[test]
    fn cancel_build_before_cgroup_path_recorded() {
        let slot = Arc::new(BuildSlot::default());
        let g = slot.try_claim("/nix/store/test.drv").unwrap();
        let cancelled = g.cancelled();
        // No set_cgroup_path call — spawn_build_task hasn't reached it yet.

        let got = try_cancel_build(&slot, "/nix/store/test.drv");
        assert!(
            got,
            "claimed slot → cancel must land even without cgroup path"
        );
        assert!(
            cancelled.load(std::sync::atomic::Ordering::Acquire),
            "flag set so execute_build's pre-cgroup poll aborts"
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
        // Path that definitely doesn't exist. tmpdir/nonexistent so
        // the test doesn't depend on /sys/fs/cgroup being mounted (CI
        // sandbox may not have cgroup v2).
        let tmp = tempfile::tempdir().unwrap();
        let fake_cgroup = tmp.path().join("not-created-yet");

        let slot = Arc::new(BuildSlot::default());
        let g = slot.try_claim("/nix/store/test.drv").unwrap();
        let cancelled = g.cancelled();
        slot.set_cgroup_path(fake_cgroup);

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

    /// `build_heartbeat_request` is field-for-field passthrough. Each
    /// case varies one input; the body asserts the full proto so a
    /// hardcoded literal in any position fails every row.
    ///
    /// Covers: running_build/busy from slot, store_degraded from
    /// CircuitBreaker::is_open() (P0211 has_capacity gate),
    /// supported_features from CRD config (regression: hardcoded-empty
    /// silently ignored requiredSystemFeatures).
    #[rstest]
    #[case::running(Some("/nix/store/foo.drv"), vec![], false)]
    #[case::idle(None, vec![], false)]
    #[case::degraded(None, vec![], true)]
    #[case::features(None, vec!["kvm".into(), "big-parallel".into()], false)]
    #[tokio::test]
    async fn heartbeat_field_passthrough(
        #[case] claim: Option<&'static str>,
        #[case] features: Vec<String>,
        #[case] store_degraded: bool,
    ) {
        let slot = Arc::new(BuildSlot::default());
        let _guard = claim.map(|d| slot.try_claim(d).unwrap());
        let req = build_heartbeat_request(
            "worker-1",
            rio_proto::types::ExecutorKind::Builder,
            &["x86_64-linux".into()],
            &features,
            "",
            &slot,
            &ResourceSnapshotHandle::default(),
            store_degraded,
            false,
        )
        .await;
        assert_eq!(req.executor_id, "worker-1");
        assert_eq!(req.systems, vec!["x86_64-linux"]);
        assert_eq!(req.running_build.as_deref(), claim);
        assert_eq!(req.store_degraded, store_degraded);
        assert_eq!(req.supported_features, features);
        assert_eq!(req.resources.unwrap().busy, claim.is_some());
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
        let cache = Arc::new(crate::fuse::cache::Cache::new(dir.path().to_path_buf()).unwrap());
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
        let cache = Arc::new(crate::fuse::cache::Cache::new(dir.path().to_path_buf()).unwrap());
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
        let cache = Arc::new(crate::fuse::cache::Cache::new(dir.path().to_path_buf()).unwrap());
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
