//! Worker runtime: the pull-mode lifecycle and build-task spawning.
//!
//! Glue between main.rs's bootstrap and the executor/FUSE/upload
//! subsystems. `setup` wires up cgroups, gRPC clients, FUSE mount and
//! the build context; [`run`] hands off to the pull loop (pull → build
//! → report → exit).
//!
//! Submodules (each a clean extraction with no cross-cutting state):
//! - `slot`: single-build occupancy + cancel target
//! - `setup`: cold-start wiring (identity, cgroup, connect, FUSE)
//! - `pull`: the pull client loop (the runtime entry)
//! - `result`: completion construction + the send chokepoint

mod idle;
pub mod pull;
mod result;
mod setup;
mod slot;

pub use setup::setup;
pub use slot::{BuildSlot, BuildSlotGuard, try_cancel_build};

use result::{
    err_completion, final_footer_result, ok_completion, panic_completion, send_completion,
};
use setup::{BalanceGuards, WorkerClient};

use crate::cgroup::ResourceSnapshotHandle;

// Test-only re-exports: the `mod tests` block below predates the
// submodule split and pulls everything via `super::*`.
#[cfg(test)]
use {setup::resolve_executor_identity, setup::validate_host_arch};

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{Instrument, info, instrument};

use rio_proto::types::WorkAssignment;

use crate::executor::BuildTaskMessage;
use crate::{executor, log_stream};

/// How long the build-completion path waits for the log uploader to
/// drain (every line acked durable by rio-store) before detaching it
/// and sending the `CompletionReport` anyway. The house style for
/// bounded teardown. Past this the upload task keeps reconnecting and
/// replaying in the background for up to its 10-minute drain deadline;
/// the build slot frees immediately. A build is never failed or delayed
/// because its log could not be persisted.
const LOG_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// `BuildSpawnContext::hw_bench` payload: the spawned resolve→bench
/// task's handle. Yields `(hw_class, factor)` — `hw_class` is empty if
/// the resolver expired (annotator dead / non-k8s); `factor` is `None`
/// if `hw_class` was empty (bench skipped) or the bench panicked. The
/// inner tuple is `(alu, membw?, ioseq?)`: per-dim presence so an
/// unmeasured dimension contributes nothing to the fleet median
/// (bug_037).
pub type HwBenchHandle = Arc<
    std::sync::Mutex<
        Option<tokio::task::JoinHandle<(String, Option<crate::hw_bench::HwBenchResult>)>>,
    >,
>;

/// Shared context for spawning build tasks.
///
/// Constructed once at setup to reduce per-assignment clone
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
    /// The process-lifetime build-task sink: the spawned build task
    /// sends its `CompletionReport` (and phase edges) here; the pull
    /// loop consumes it and forwards the report through
    /// `ReportOutcome`.
    pub stream_tx: mpsc::Sender<BuildTaskMessage>,
    /// Single-build occupancy. The pull loop `try_claim`s before
    /// calling [`spawn_build_task`].
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
    /// Resolved target systems (`RIO_SYSTEMS`). Threaded to
    /// `setup_nix_conf` so the per-build daemon's `extra-platforms`
    /// matches the resolved identity — a drv routed for `i686-linux`
    /// is then accepted by the x86_64 daemon.
    pub systems: Arc<[String]>,
    /// Handle to the FUSE local cache. Threaded into `ExecutorEnv` so
    /// the executor can `register_inputs` (JIT allowlist) and
    /// `prefetch_manifests` (I-110c) before daemon spawn.
    pub fuse_cache: Arc<crate::fuse::cache::Cache>,
    /// Base per-fetch gRPC timeout for the FUSE cache's `GetPath`.
    /// JIT lookup scales it per path via `jit_fetch_timeout(this,
    /// nar_size)` (I-178).
    pub fuse_fetch_timeout: Duration,
    /// k8s `spec.nodeName` (from `Config.node_name`, downward API).
    /// Attached to every `CompletionReport` for ADR-023's hw_class
    /// join. `None` outside k8s (empty config string).
    pub node_name: Option<String>,
    /// `RIO_HW_CLASS` (controller-stamped pod annotation, downward
    /// API). Attached to every `CompletionReport` so the scheduler can
    /// write `build_samples.hw_class` directly — the scheduler has no
    /// Node informer, so this is the only path. `None` outside k8s /
    /// before the annotator stamps. Lazily populated from `hw_bench`
    /// on first assignment (the resolve runs concurrently with FUSE
    /// mount; see `setup.rs`). `Arc<Mutex<..>>` so the struct stays
    /// `Clone` and the panic handler can read whatever the consumer
    /// wrote.
    pub hw_class: Arc<std::sync::Mutex<Option<String>>>,
    /// Shared handle to the cgroup-poll `ResourceUsage` snapshot
    /// (same `Arc` the cgroup poller publishes to). Read once at
    /// completion time to populate `CompletionReport.final_resources` — the
    /// only telemetry channel for the scheduler's `build_samples`
    /// writer (ADR-023).
    pub resources: ResourceSnapshotHandle,
    /// ADR-023 phase-10 resolve+microbench handle, spawned at init so
    /// the resolve poll (≤30s) overlaps FUSE mount. `take()`n by
    /// [`spawn_build_task`] on the FIRST assignment: the resolved
    /// `hw_class` is written to [`Self::hw_class`] and the bench
    /// `factor` (if any) is sent via `AppendHwPerfSample` carrying
    /// that assignment's token (`r[sec.boundary.grpc-hmac]` — the
    /// store derives `pod_id` from claims, not the request body).
    /// `Arc<Mutex<Option<..>>>` so the struct stays `Clone` (shared
    /// slot; second `take()` on any clone sees `None`).
    pub hw_bench: HwBenchHandle,
    /// The FUSE fetch circuit breaker (same `Arc` `NixStoreFs` holds).
    /// Read-only here: `spawn_build_task` snapshots `trip_count()` at
    /// build start and the completion stamp marks the report
    /// store-degraded when the breaker is open at completion OR
    /// tripped during the build (bug_408 — the mid-build signal a
    /// one-shot pod's fresh-closed breaker would otherwise hide).
    pub fuse_circuit: Arc<crate::fuse::circuit::CircuitBreaker>,
}

impl BuildSpawnContext {
    /// Per-worker fields stamped onto every `CompletionReport`
    /// (success, error, and panic paths).
    ///
    /// `final_resources`: the shared snapshot is the cgroup
    /// utilization reporter's 10s-cadence poll — ≤10s stale, and the
    /// reporter loop exits on
    /// shutdown WITHOUT a final read. `cpu_seconds_total` (cumulative)
    /// and `peak_disk_bytes` (running-max over CURRENT `dqb_curspace`)
    /// would systematically under-report into `build_samples`, biasing
    /// the SLA p̄ fit and disk_p90 low for short builds.
    /// `cgroup::final_sample` forces one synchronous read on top.
    ///
    /// `peak_disk_bytes` is the prjquota sample `execute_build` took
    /// BEFORE `teardown_overlay` — by the time this runs, the upper
    /// dir is gone and `dqb_curspace` ≈ 0. `None` on the Err path
    /// (no `ExecutionResult`); `final_sample` then returns `prev`'s
    /// reporter-loop value, which is the best available.
    fn completion_stamp(
        &self,
        peak_disk_bytes: Option<u64>,
        circuit_trips_at_spawn: u64,
        upload_transport: bool,
    ) -> result::CompletionStamp {
        let prev = *self.resources.read().unwrap_or_else(|e| e.into_inner());
        result::CompletionStamp {
            node_name: self.node_name.clone(),
            hw_class: self
                .hw_class
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            final_resources: Some(crate::cgroup::final_sample(
                &self.cgroup_parent,
                peak_disk_bytes,
                prev,
            )),
            // Per-lane store evidence (no Default — every lane named).
            store_evidence: result::StoreEvidenceSet {
                // bug_408: degraded = open RIGHT NOW (the build very
                // likely failed on EIO from the open breaker) OR tripped
                // during this build (open-then-auto-closed — the 30s
                // auto-close beats most build durations, so the
                // point-in-time check alone under-reports). The stamp is
                // advisory data; the assembly fns apply it only to
                // InfrastructureFailure.
                fuse_breaker: self.fuse_circuit.is_open()
                    || self.fuse_circuit.trip_count() > circuit_trips_at_spawn,
                // bug_286: upload-lane evidence, classified by the
                // caller from the execution outcome (the assembly fns
                // additionally fold ExecutionResult/Upload-error
                // evidence — this seed covers nothing today but keeps
                // the lane named at the constructor).
                upload_transport,
            },
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
            // Snapshot for the `rio:` banner header. `spawn_build_task`
            // bounded-awaits the resolve→bench task and writes this cell
            // BEFORE spawning the task that calls executor_env(), so the
            // snapshot sees the resolved value whenever it was available
            // at build start (bug_014). Still `None` when non-k8s, when
            // the annotator never stamped the downward-API volume, or
            // when the bench was still running at first-assignment time
            // — the banner then drops the `/{hw_class}` suffix.
            hw_class: self
                .hw_class
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            fuse_cache: Some(Arc::clone(&self.fuse_cache)),
            fuse_fetch_timeout: self.fuse_fetch_timeout,
            cancelled,
        }
    }
}

/// How long the first assignment waits inline for the init-time
/// resolve→bench task before falling back to a background harvest.
///
/// The wait orders the `ctx.hw_class` write before the `executor_env()`
/// snapshot that feeds the banner's `rio: builder {system}/{hw_class}`
/// line — a fire-and-forget producer racing a freshly-spawned consumer
/// has no happens-before edge, and workers are one-shot so a lost race
/// permanently drops the suffix from that pod's log (bug_014).
///
/// The latency distribution is bimodal: in steady state the
/// resolve→bench task finished during the ~30s FUSE cold-start and its
/// JoinHandle is already `Ready` — `timeout` returns `Ok` on the first
/// poll at zero cost. If it is still running it has *seconds* of bench
/// left (~5s alu ‖ ioseq, +~3s membw when `bench_needed`), so no small
/// bound would catch it. The value therefore only caps the added
/// latency on the fall-back path; it is not a "wait for the bench"
/// knob. The build must never block on the bench.
const HW_BENCH_INLINE_WAIT: Duration = Duration::from_millis(100);

/// Publish a finished resolve→bench result into the shared `hw_class`
/// cell and return the `(hw_class, factor)` pair to forward to
/// `AppendHwPerfSample` (`None` when there is nothing to send).
///
/// The cell write is **synchronous** — visible to any reader of `cell`
/// the moment this returns. `spawn_build_task` relies on that to order
/// the write before the `executor_env()` banner snapshot in the
/// completed-within-bound path. Empty `hw_class` → `None` (proto3
/// optional semantics: "unknown hw"); a panicked bench task is logged
/// and leaves the cell untouched.
fn publish_hw_class(
    joined: Result<(String, Option<crate::hw_bench::HwBenchResult>), tokio::task::JoinError>,
    cell: &std::sync::Mutex<Option<String>>,
) -> Option<(String, crate::hw_bench::HwBenchResult)> {
    let (hw_class, factor) = match joined {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "hw_bench: resolve+bench task panicked");
            return None;
        }
    };
    // Publish hw_class for the banner snapshot, completion_stamp, and
    // the panic handler. Empty → None.
    *cell.lock().unwrap_or_else(|e| e.into_inner()) =
        (!hw_class.is_empty()).then(|| hw_class.clone());
    factor.map(|factor| (hw_class, factor))
}

/// Handle a WorkAssignment: spawn the build task and set up a
/// panic-catcher.
///
/// Returns after spawning — does NOT block on build completion. The build runs
/// in its own tokio task holding `permit`; it reports completion via
/// `ctx.stream_tx` and drops the slot guard on exit (success, failure,
/// or panic).
///
/// The pull loop awaits the completion on the sink after calling this;
/// the guard-drop on completion clears the slot.
#[instrument(skip_all, fields(drv_path = %assignment.drv_path))]
pub async fn spawn_build_task(
    assignment: WorkAssignment,
    guard: BuildSlotGuard,
    ctx: &BuildSpawnContext,
) {
    let drv_path = assignment.drv_path.clone();
    let assignment_token = assignment.assignment_token.clone();
    let traceparent = assignment.traceparent.clone();
    // bug_408: window the breaker's trip counter to THIS build.
    let circuit_trips_at_spawn = ctx.fuse_circuit.trip_count();

    // ADR-023 phase-10: now that we have an assignment token, harvest
    // the resolve→bench task (spawned at init, runs concurrently with
    // FUSE mount; normally long since done by the time the first
    // assignment arrives). Populate `ctx.hw_class` for the banner
    // header and every later `CompletionReport`, and fire the
    // `AppendHwPerfSample` RPC. `take()` so this is one-shot.
    //
    // The bounded inline await orders the `ctx.hw_class` write before
    // the `executor_env()` snapshot at the top of `executor_future`
    // (spawned below) — see `HW_BENCH_INLINE_WAIT` for the race and
    // the latency budget (bug_014). If the bench is genuinely still
    // running, do NOT block the build on it: fall back to the
    // background harvest — that pod's banner drops the `/{hw_class}`
    // suffix (display-only, documented `None` cause) and
    // `completion_stamp` re-reads the cell after the build, by which
    // point the write has landed. The `AppendHwPerfSample` RPC is
    // fire-and-forget on both paths: the network call is the part the
    // build must never wait on.
    //
    // The take() is hoisted out of the `if let` scrutinee so the
    // MutexGuard dies before the `.await` — edition-2024 scrutinee
    // temporaries live through the then-block, and guards held across
    // awaits aren't Send (same convention as the panic handler below).
    let bench = ctx.hw_bench.lock().unwrap().take();
    if let Some(mut bench) = bench {
        match tokio::time::timeout(HW_BENCH_INLINE_WAIT, &mut bench).await {
            Ok(joined) => {
                // Result in hand: write the cell HERE, before
                // `executor_future` exists, so the banner snapshot is
                // sequenced after it. Only the RPC stays backgrounded.
                if let Some((hw_class, factor)) = publish_hw_class(joined, &ctx.hw_class) {
                    let mut store = ctx.store_clients.store.clone();
                    let pod_id = ctx.executor_id.clone();
                    let token = assignment_token.clone();
                    rio_common::task::spawn_monitored("hw-bench-send", async move {
                        crate::hw_bench::send(&mut store, &hw_class, &pod_id, factor, &token).await;
                    });
                }
            }
            Err(_elapsed) => {
                // Bench still running. `&mut bench` left the handle
                // un-consumed — move it into the background task and
                // publish + send from there once it finishes.
                let mut store = ctx.store_clients.store.clone();
                let hw_class_cell = Arc::clone(&ctx.hw_class);
                let pod_id = ctx.executor_id.clone();
                let token = assignment_token.clone();
                rio_common::task::spawn_monitored("hw-bench-send", async move {
                    if let Some((hw_class, factor)) = publish_hw_class(bench.await, &hw_class_cell)
                    {
                        crate::hw_bench::send(&mut store, &hw_class, &pod_id, factor, &token).await;
                    }
                });
            }
        }
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
    let cancelled = guard.cancelled();
    ctx.slot.set_cgroup_path(ctx.cgroup_parent.join(&build_id));

    // Clone for the panic handler before moving ctx into the task.
    let panic_tx = ctx.stream_tx.clone();
    let panic_drv_path = drv_path.clone();
    let panic_token = assignment_token.clone();
    let panic_node_name = ctx.node_name.clone();
    let panic_hw_class = Arc::clone(&ctx.hw_class);
    let panic_resources = Arc::clone(&ctx.resources);
    let panic_circuit = Arc::clone(&ctx.fuse_circuit);

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

        // Per-assignment log uploader: streams every BuildLogBatch this
        // assignment produces (banner header, build output, banner
        // footer — across every daemon-transient retry attempt) to
        // rio-store's `AppendLog` under the assignment's exec_id. ONE
        // uploader spans the whole retry loop: a daemon-transient retry
        // is the same execution (same exec_id, same token) with line
        // numbers continuing from the prior attempt's count, so the
        // same `AppendLog` session keeps accepting them — the store's
        // monotone-line gate sees one forward-moving stream. Only a
        // scheduler re-dispatch mints a new exec_id, and that arrives
        // as a fresh WorkAssignment → a fresh spawn_build_task → a
        // fresh uploader.
        //
        // `upload_tx` is the ONLY sender clone outside the uploader
        // itself; everything below borrows it. It is explicitly dropped
        // before `uploader.finish()` so the uploader's drain deadline
        // can start (the input channel only closes once every sender is
        // gone).
        let uploader = crate::log_upload::LogUploader::spawn(
            ctx.store_clients.log.clone(),
            assignment.exec_id.clone(),
            drv_path.clone(),
            assignment_token.clone(),
        );
        let upload_tx = uploader.sender();

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
        //
        // Banner ownership: `execute_build` sends the `rio:` header
        // only on the FIRST attempt (`prev_line_count == 0`); retried
        // attempts seed the batcher at the prior attempt's
        // `final_line_count` so output line numbers stay monotone in
        // the scheduler's ring buffer. The footer is sent ONCE here
        // after the loop with the most recent attempt that produced
        // output — emitting it per attempt would write conflicting
        // `rio: result` lines for one exec_id (bug_013).
        // TODO: emit a single `rio: retry  N/M <error>` marker line
        // between attempts so a reader sees why the output content
        // jumps; needs a banner.rs function + spec note.
        //
        // `assignment_start`: total wall time for the assignment
        // including inter-attempt retry backoff (≤ 3.5s). Reported in
        // the footer's `after Ns` so it matches what the user
        // perceives. INTENTIONALLY diverges from the per-attempt
        // `rio_builder_build_duration_seconds` histogram inside
        // `execute_build`, which measures individual build attempts.
        let assignment_start = std::time::Instant::now();
        let mut attempt = 0u32;
        let mut prev_line_count = 0u64;
        // Most recent attempt's footer string (`Some(...)` only when a
        // daemon ran in that attempt). Tracked across the loop so a
        // footer is sent whenever ANY attempt ran a daemon — without
        // this, a final attempt that fails pre-daemon (e.g.
        // `DaemonSpawn` after a prior `Wire(UnexpectedEof)`) would
        // silently drop the footer despite output being in the log.
        let mut last_footer_result: Option<String> = None;
        let outcome = loop {
            // First-attempt invariant: `execute_build` gates the
            // banner header on `first_line == 0`, which is true ONLY
            // on the first attempt. Every error in
            // `is_daemon_transient()` fires AFTER the header
            // (`DaemonSpawn`/`Handshake`/`Wire(UnexpectedEof)` all
            // require a daemon spawn attempt, which is post-header),
            // so a retried attempt always sees
            // `prev_line_count >= HEADER_LINE_COUNT`. If a future
            // transient variant fires pre-header (new ExecutorError,
            // refactor of the early-return order), this catches the
            // bug-recurrence: a re-emitted header at line 0 (bug_013).
            debug_assert!(
                attempt == 0 || prev_line_count >= crate::banner::HEADER_LINE_COUNT,
                "retried attempt must follow a header-emitting prior attempt \
                 (prev_line_count={prev_line_count}, attempt={attempt})"
            );
            let o = executor::execute_build(
                &assignment,
                &build_env,
                &mut store_client,
                &ctx.stream_tx,
                &upload_tx,
                prev_line_count,
            )
            .await;
            prev_line_count = o.final_line_count;
            if o.footer_result.is_some() {
                last_footer_result.clone_from(&o.footer_result);
            }

            match &o.result {
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
                _ => break o,
            }
        };

        // Send CompletionReport. Resource fields flow from the executor
        // (cgroup memory.peak + polled cpu.stat). Peaks live on
        // ExecuteOutcome so they survive the Err path — for CgroupOom,
        // peak_memory_bytes ≈ memory.max is the most actionable sizing
        // signal. On Err, the cancel flag is read BEFORE deciding the
        // status — Acquire pairs with try_cancel_build's Release (not
        // strictly needed, no other state to synchronize, but cheap and
        // documents the pairing).
        let executor::ExecuteOutcome {
            result,
            peak_memory_bytes,
            peak_cpu_cores,
            peak_disk_bytes,
            final_line_count,
            footer_result: _, // tracked across attempts as `last_footer_result`
        } = outcome;

        // r[impl obs.log.worker-header]
        // Banner footer — ONE per assignment, after the retry loop.
        // Carries the most recent daemon-running attempt's outcome
        // (`last_footer_result`), not the post-daemon collect/teardown
        // result on `result`. Skipped only when NO attempt ran a daemon
        // (every attempt was a pre-daemon setup failure or the
        // `RIO_BUILDER_SCRIPT` fixture) — header-without-footer is the
        // documented signal that the build never started. Goes out
        // BEFORE the CompletionReport so the scheduler ring buffer is
        // settled by the time the build resolves.
        // The cancel flag overrides the attempt's string to `cancelled`
        // (`final_footer_result` — same flag read as `err_completion`
        // below, so on the Err arm the footer and the report agree; the
        // Ok arm is deliberately split, see `final_footer_result`'s doc).
        // Best-effort: the scheduler's cancel-path seal drops this
        // footer before it reaches the stored log — see
        // `terminal_log_epilogue`'s sequencing note in rio-scheduler.
        // TODO: the runtime footer-send path (once-per-assignment,
        // skipped-on-None, prev_line_count threading) has no unit test
        // — `spawn_build_task` requires a full BuildContext + live gRPC
        // stream. Covered by `nextest-rio-builder` compilation +
        // existing VM scenarios; a `RIO_BUILDER_SCRIPT` VM test
        // asserting "exactly one rio: result line per exec_id" would
        // close the gap.
        let report_line_count = if let Some(footer_result) = final_footer_result(
            last_footer_result.as_deref(),
            cancelled.load(std::sync::atomic::Ordering::Acquire),
        ) {
            let footer = crate::banner::footer_lines(
                &assignment.exec_id,
                footer_result,
                assignment_start.elapsed(),
            );
            // The footer occupies line numbers [final_line_count,
            // final_line_count + footer.len()); the post-footer
            // high-water mark is the CompletionReport's
            // final_line_count (the store's completeness predicate is
            // "the manifest covers a contiguous [0, final_line_count)").
            let after_footer = final_line_count + footer.len() as u64;
            executor::send_banner_batch(
                &upload_tx,
                &drv_path,
                &build_env.executor_id,
                final_line_count,
                footer,
            )
            .await;
            after_footer
        } else {
            // No footer: either nothing was ever emitted
            // (final_line_count == 0, a pre-header setup failure → the
            // store reads 0 as "not reported") or the log ends at the
            // last line the batcher accounted for (header-only or
            // header+output, both already counted).
            final_line_count
        };

        // Drop the last sender clone outside the uploader, then drain.
        // Every other holder (the stderr loop, the banner sends)
        // borrowed `upload_tx` and has returned by now, so this drop is
        // what lets the uploader observe its input channel closing and
        // start the drain. `finish()` waits up to the grace period for
        // every line to be acked durable by rio-store; past that the
        // upload task detaches and keeps draining in the background
        // while the build slot frees and the CompletionReport goes out
        // — a build is never failed or delayed because its log could
        // not be persisted.
        drop(upload_tx);
        let mut drain_progress = uploader.progress();
        match uploader.finish(LOG_DRAIN_GRACE).await {
            crate::log_upload::DrainStatus::Drained { .. } => {}
            crate::log_upload::DrainStatus::Detached { unacked_lines, .. } => {
                tracing::info!(
                    drv_path = %drv_path,
                    unacked_lines,
                    "log upload still draining; detached (build completion proceeds)"
                );
                // Log the detached drain's eventual outcome — without
                // this, a detached-then-drained upload leaves no trace
                // and a detached-then-abandoned one is only visible as
                // an un-correlated counter + the uploader's own error.
                let detached_drv = drv_path.clone();
                rio_common::task::spawn_monitored("log-drain-watch", async move {
                    while drain_progress.changed().await.is_ok() {
                        let p = *drain_progress.borrow();
                        if p.done {
                            tracing::info!(
                                drv_path = %detached_drv,
                                last_acked_line = ?p.last_acked_line,
                                unacked_lines = p.unacked_lines,
                                "detached log drain finished"
                            );
                            break;
                        }
                    }
                });
            }
            crate::log_upload::DrainStatus::Abandoned {
                unacked_lines,
                reason,
                ..
            } => {
                // The uploader already disclosed (counter + error! for
                // real loss, debug! for a zero-loss CompleteLog); this
                // is just the build-scoped breadcrumb.
                tracing::debug!(
                    drv_path = %drv_path,
                    unacked_lines,
                    reason = reason.as_label(),
                    "log upload ended without draining"
                );
            }
        }

        // bug_286: the upload lane's evidence is folded in by the
        // assembly fns (ok_completion reads ExecutionResult.
        // store_unreachable; err_completion classifies
        // ExecutorError::Upload) — seed false here.
        let stamp = ctx.completion_stamp(peak_disk_bytes, circuit_trips_at_spawn, false);
        let mut completion = match result {
            Ok(exec_result) => ok_completion(exec_result, stamp),
            Err(e) => err_completion(
                &e,
                drv_path,
                assignment_token,
                cancelled.load(std::sync::atomic::Ordering::Acquire),
                stamp,
                peak_memory_bytes,
                peak_cpu_cores,
            ),
        };
        // The worker line-number high-water mark after the footer —
        // header(3) + body + footer(2) for a build that ran a daemon.
        // The store's completeness predicate tests "the manifest covers
        // a contiguous [0, final_line_count)"; an undercount here makes
        // it pass while stored lines sit beyond it, an overcount makes
        // every log read as incomplete forever. 0 = never emitted
        // anything = "not reported" (the scheduler maps it to SQL NULL).
        completion.final_line_count = report_line_count;

        send_completion(&ctx.stream_tx, completion).await;
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
            // Read out before the .await — Mutex/RwLock guards aren't Send.
            let final_resources = Some(*panic_resources.read().unwrap_or_else(|e| e.into_inner()));
            let hw_class = panic_hw_class
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            send_completion(
                &panic_tx,
                panic_completion(
                    panic_drv_path,
                    panic_token,
                    result::CompletionStamp {
                        node_name: panic_node_name,
                        hw_class,
                        final_resources,
                        store_evidence: result::StoreEvidenceSet {
                            fuse_breaker: panic_circuit.is_open()
                                || panic_circuit.trip_count() > circuit_trips_at_spawn,
                            // Panic = the build task died before any
                            // upload outcome existed; no upload-lane
                            // evidence is observable on this path.
                            upload_transport: false,
                        },
                    },
                ),
            )
            .await;
        }
    });
}

/// Fully-wired worker runtime. Built by `setup`; consumed by `run`.
/// Holds every long-lived handle the pull loop needs so `main()`
/// reduces to bootstrap → setup → run → teardown.
pub struct BuilderRuntime {
    scheduler_client: WorkerClient,
    shutdown: rio_common::signal::Token,
    /// FUSE mount session. Dropped explicitly in [`run_teardown`]
    /// (NOT here) so the abort-then-sleep ordering in `r[builder.shutdown.fuse-abort]`
    /// stays adjacent to the comment that explains it.
    fuse_session: crate::fuse::FuseMount,
    slot: Arc<BuildSlot>,
    build_ctx: BuildSpawnContext,
    /// `RIO_EXECUTOR_TOKEN` — presented on every pull/report unary
    /// (metadata header or request body). Empty in dev mode → omitted.
    /// See `r[sec.executor.identity-token]`.
    executor_token: String,
    /// The NotYetReady idle bound (`RIO_IDLE_SECS`, I-116 successor):
    /// a pod that has only ever received `NotYetReady` for this long
    /// exits 0, charge-free.
    idle_timeout: Duration,
    /// `RIO_INTENT_ID` — the derivation this pod was spawned for (the
    /// pull work key). The pull loop refuses to start without it.
    intent_id: String,
    /// Readiness flag shared with the health server. Set by the pull
    /// loop once an assignment is pulled (building); cleared again
    /// before exit so a terminating pod drops out of any Service
    /// endpoints promptly.
    ready: Arc<AtomicBool>,
    /// The receive half of the build-task sink
    /// (`build_ctx.stream_tx`'s counterpart). `Option` only so the
    /// pull loop can take ownership when its build phase starts.
    pull_sink_rx: Option<mpsc::Receiver<BuildTaskMessage>>,
    /// Probe-loop guards for both balanced channels. Held for process
    /// lifetime (dropping a `BalancedChannel` stops its probe loop).
    _balance_guard: BalanceGuards,
}

/// Drive the pull lifecycle (pull → build → report → exit), then run
/// exit teardown (FUSE abort). Pull is the only delivery path — the
/// stream session client (registration, heartbeat, bidi dispatch
/// stream, relay/drain machinery) was removed with the
/// executor-lifecycle collapse; SIGTERM/SIGINT abort semantics live in
/// the pull loop (`r[builder.shutdown.sigint+4]`).
pub async fn run(rt: BuilderRuntime) -> anyhow::Result<()> {
    pull::run_pull(rt).await
}

/// Exit teardown: FUSE abort + drop. By now the single build (if any)
/// has returned its permit and the report phase is over.
fn run_teardown(rt: BuilderRuntime) {
    info!("teardown: aborting FUSE session and exiting");

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
#[cfg(test)]
mod tests {
    use super::*;

    /// `CompletionReport.final_line_count` is the worker line-number
    /// high-water mark AFTER the footer — header(3) + body + footer(2)
    /// — i.e. `last_emitted_line_number + 1`. The store's completeness
    /// predicate is "the manifest covers a contiguous
    /// `[0, final_line_count)`": an undercount (e.g. counting only the
    /// lines the `LogBatcher` processed, which excludes the 3 header
    /// lines that bypass it) makes the predicate pass while stored
    /// lines sit beyond it; an overcount makes every log read as
    /// incomplete forever.
    ///
    /// This composes the real pieces the way `spawn_build_task` does:
    /// the batcher seeded at `HEADER_LINE_COUNT`, N body lines flushed,
    /// `line_count()` as the footer's `first_line_number`, and the
    /// footer's own length added on top.
    #[test]
    fn final_line_count_is_the_post_footer_high_water_mark() {
        const N: u64 = 7;
        let mut batcher = log_stream::LogBatcher::new(
            "/nix/store/x.drv".into(),
            "w".into(),
            log_stream::LogLimits::UNLIMITED,
            crate::banner::HEADER_LINE_COUNT,
        );
        for i in 0..N {
            // UNLIMITED + < 64 lines → always Buffered, never a batch.
            assert!(matches!(
                batcher.add_line(format!("body {i}").into_bytes()),
                log_stream::AddLineResult::Buffered
            ));
        }
        let last_body_batch = batcher.final_flush();
        assert_eq!(last_body_batch.first_line_number, 3);
        assert_eq!(last_body_batch.lines.len() as u64, N);

        // What `run_daemon_lifecycle` returns as `final_line_count`.
        let final_line_count = batcher.line_count();
        assert_eq!(final_line_count, crate::banner::HEADER_LINE_COUNT + N);

        // What the runtime sends as the footer and reports on the
        // CompletionReport.
        let footer =
            crate::banner::footer_lines("exec-id", "ok", std::time::Duration::from_secs(1));
        let report_line_count = final_line_count + footer.len() as u64;
        let last_emitted_line_number = final_line_count + footer.len() as u64 - 1;

        assert_eq!(report_line_count, N + 5, "header(3) + body(N) + footer(2)");
        assert_eq!(
            report_line_count,
            last_emitted_line_number + 1,
            "final_line_count is the high-water mark, not a processed-line count"
        );
    }

    /// bug_014 (helper truth table, happy shape): a resolved class with a
    /// bench factor is stored in the cell and forwarded for the RPC. The
    /// ordering half of the fix — `spawn_build_task` calling this BEFORE
    /// spawning the task that snapshots the cell — is a control-flow
    /// property of `spawn_build_task` and is not unit-testable (see the
    /// TODO at the footer-send path); this test only pins what the helper
    /// does once called.
    #[test]
    fn publish_hw_class_with_factor_sets_cell_and_forwards() {
        let cell = std::sync::Mutex::new(None);
        let send = publish_hw_class(
            Ok(("large-x86".to_string(), Some((1.5, None, None)))),
            &cell,
        );
        assert_eq!(cell.lock().unwrap().as_deref(), Some("large-x86"));
        assert_eq!(send, Some(("large-x86".to_string(), (1.5, None, None))));
    }

    /// Resolve succeeded but the bench was skipped/panicked (`factor=None`):
    /// the class is still published for the banner + completion_stamp, but
    /// nothing is forwarded to `AppendHwPerfSample`.
    #[test]
    fn publish_hw_class_no_factor_still_publishes_class() {
        let cell = std::sync::Mutex::new(None);
        assert_eq!(
            publish_hw_class(Ok(("large-x86".to_string(), None)), &cell),
            None
        );
        assert_eq!(cell.lock().unwrap().as_deref(), Some("large-x86"));
    }

    /// Empty hw_class (resolver expired / non-k8s) → cell is set to `None`
    /// (proto3 "unknown hw" semantics), nothing forwarded. The pre-seeded
    /// `Some` is unreachable in production (the cell is written at most
    /// once); it is here to assert the write is an unconditional
    /// assignment, not an insert-if-empty.
    #[test]
    fn publish_hw_class_empty_class_publishes_none() {
        let cell = std::sync::Mutex::new(Some("stale".to_string()));
        assert_eq!(publish_hw_class(Ok((String::new(), None)), &cell), None);
        assert_eq!(*cell.lock().unwrap(), None);
    }

    /// A panicked resolve→bench task is logged and leaves the cell
    /// untouched — same as the pre-fix behaviour of the spawned harvester.
    #[tokio::test]
    async fn publish_hw_class_join_error_leaves_cell() {
        let cell = std::sync::Mutex::new(None);
        let err = tokio::spawn(async { panic!("bench panicked") })
            .await
            .expect_err("task panicked");
        assert_eq!(publish_hw_class(Err(err), &cell), None);
        assert_eq!(*cell.lock().unwrap(), None);
    }

    #[test]
    fn validate_host_arch_gates_builders() {
        use rio_proto::types::ExecutorKind::Builder;
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
    }

    /// r35 bug_039: a Fetcher worker with arch-typed systems
    /// (`RIO_SYSTEMS=x86_64-linux,builtin` for `pkgs.fetchurl` FODs) on
    /// the wrong host arch must NOT silently `Ok(())`. The pre-r35
    /// `if kind == Fetcher { return Ok(()); }` early-return was the
    /// other half of the bug_039 hole: §13e dropped the helm-static
    /// fetcher arch nodeSelector AND skipped this check, so an
    /// `x86-64-fetcher` pod that landed on arm64 registered, accepted
    /// dispatch, and CrashLoopBackOff'd. The pod-spec arch nodeSelector
    /// (option (a)) is the primary fix; this is the §13d defense-in-depth
    /// backstop against helm chart drift.
    #[test]
    fn validate_host_arch_fetcher_arch_typed_systems() {
        use rio_proto::types::ExecutorKind::Fetcher;
        let s = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };

        // Arch-typed Fetcher on the wrong host: refuse at boot.
        assert!(
            validate_host_arch(Fetcher, &s(&["x86_64-linux", "builtin"]), "aarch64-linux").is_err(),
            "Fetcher worker with arch-typed RIO_SYSTEMS on wrong arch must refuse — \
             a misplaced fetcher refuses arch-typed systems at register time \
             instead of failing builds at dispatch (r35 bug_039)"
        );
        // Arch-typed Fetcher on the right host: ok.
        assert!(
            validate_host_arch(Fetcher, &s(&["x86_64-linux", "builtin"]), "x86_64-linux").is_ok()
        );
        // Multi-arch Fetcher: accepts either.
        assert!(
            validate_host_arch(
                Fetcher,
                &s(&["x86_64-linux", "aarch64-linux"]),
                "aarch64-linux"
            )
            .is_ok()
        );
        // builtin-only Fetcher (the common `builtins.fetchurl` case): no
        // constraint — arch-agnostic, intentionally cheap Gravitons.
        assert!(validate_host_arch(Fetcher, &s(&["builtin"]), "aarch64-linux").is_ok());
    }

    /// §13e biconditional `Fetcher ⟺ [fetcher]` enforced at the
    /// builder's identity-resolution chokepoint. Mirrors the
    /// controller's `effective_features(spec)` — see
    /// `effective_features_fetcher_for_fetcher` in
    /// `rio-controller/src/reconcilers/pool/pod.rs`.
    ///
    /// The controller's `RIO_FEATURES` injection covers k8s-spawned
    /// pods. THIS chokepoint covers every other deployment path
    /// (NixOS module, manual env, future operators). A fetcher
    /// advertising `[]` would `feature-missing`-reject every FOD —
    /// the scheduler's `rejection_reason` reads the FOD's derived
    /// `effective_features=[fetcher]` and `[fetcher] ⊄ []`. The
    /// `vm-protocol-cold-standalone` regression that motivated this:
    /// `RIO_EXECUTOR_KIND=fetcher` set, `RIO_FEATURES` unset, FOD
    /// permanently undispatched, test timed out at 500s.
    #[test]
    fn resolve_executor_identity_fetcher_features_biconditional() {
        use rio_proto::types::ExecutorKind::{Builder, Fetcher};
        let s = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
        let f = rio_common::k8s::FETCHER_FEATURE.to_string();

        // Fetcher with no declared features: derives [fetcher].
        // (Pre-fix: stayed [], fetcher's heartbeat advertised
        // supported_features=[], every FOD permanently feature-missing.)
        let (_, _, feats) =
            resolve_executor_identity(Fetcher, "f".into(), s(&["builtin"]), vec![]).unwrap();
        assert_eq!(
            feats,
            vec![f.clone()],
            "Fetcher + RIO_FEATURES unset → [fetcher]"
        );

        // Fetcher with declared [fetcher]: idempotent (controller path).
        let (_, _, feats) =
            resolve_executor_identity(Fetcher, "f".into(), s(&["builtin"]), vec![f.clone()])
                .unwrap();
        assert_eq!(feats, vec![f.clone()], "Fetcher + [fetcher] → [fetcher]");

        // Fetcher with stale declared [kvm]: overridden (mirrors
        // controller's belt-and-suspenders for pre-CEL specs).
        let (_, _, feats) =
            resolve_executor_identity(Fetcher, "f".into(), s(&["builtin"]), s(&["kvm"])).unwrap();
        assert_eq!(feats, vec![f], "Fetcher + [kvm] → overridden to [fetcher]");

        // Builder: declared verbatim, no auto-derive.
        let (_, _, feats) = resolve_executor_identity(
            Builder,
            "b".into(),
            s(&["x86_64-linux"]),
            s(&["kvm", "big-parallel"]),
        )
        .unwrap();
        assert_eq!(
            feats,
            s(&["kvm", "big-parallel"]),
            "Builder: features verbatim"
        );

        // Builder with no features: stays empty (the common case).
        let (_, _, feats) =
            resolve_executor_identity(Builder, "b".into(), s(&["x86_64-linux"]), vec![]).unwrap();
        assert!(feats.is_empty(), "Builder + no features → []");
    }

    // -----------------------------------------------------------------------

    /// Exercises the `is_busy()` short-circuit — guard drops before
    /// the watcher is first polled (current_thread). Does NOT exercise
    /// `enable()` ordering; see `slot_wait_idle_wakes_after_busy_observed`.
    ///
    /// bug_333: previously named `_no_missed_notification` and claimed
    /// to test slot.rs:113 `enable()`, but under current_thread the
    /// spawned task isn't polled until L1676's `.await` — by then the
    /// guard already dropped synchronously, `is_busy()==false`, and
    /// `wait_idle` early-returns at slot.rs:115. Deleting `enable()`
    /// or rewriting `wait_idle` as the naive `if busy { notified()
    /// .await }` both passed this test.
    #[tokio::test(start_paused = true)]
    async fn slot_wait_idle_returns_when_idle_before_first_poll() {
        let slot = Arc::new(BuildSlot::default());
        let guard = slot.try_claim("/nix/store/ccc-z.drv").unwrap();

        let watch_slot = Arc::clone(&slot);
        let drain = tokio::spawn(async move { watch_slot.wait_idle().await });
        // Drop BEFORE the watcher is first polled. notify_waiters()
        // fires into the void (no registered waiter); the watcher then
        // sees is_busy()==false and short-circuits.
        drop(guard);

        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .expect("is_busy() short-circuit")
            .expect("watcher didn't panic");
    }

    /// Exercises the wake path (slot.rs:117 `notified.await`): the
    /// watcher polls once and observes `is_busy()==true`, parks on
    /// `notified.await`, THEN the guard drops and `notify_waiters()`
    /// has a registered waiter to wake.
    ///
    /// Honesty note: under current_thread the naive `if busy {
    /// notified().await }` ALSO passes here (no preemption between
    /// slot.rs:114 and :117). Fully killing the `enable()` mutant
    /// deterministically requires `loom` — slot.rs:113's correctness
    /// is asserted by code review of the documented tokio pattern,
    /// not by this test. This test does newly exercise slot.rs:117
    /// (line-hit coverage), which the sibling above never reaches.
    #[tokio::test(start_paused = true)]
    async fn slot_wait_idle_wakes_after_busy_observed() {
        let slot = Arc::new(BuildSlot::default());
        let guard = slot.try_claim("/nix/store/ccc-z.drv").unwrap();

        let watch_slot = Arc::clone(&slot);
        let drain = tokio::spawn(async move { watch_slot.wait_idle().await });
        // Let the watcher poll once: is_busy()==true → reaches
        // notified.await (slot.rs:117).
        tokio::task::yield_now().await;
        assert!(!drain.is_finished(), "watcher parked at notified.await");

        drop(guard);
        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .expect("notify_waiters wakes parked watcher")
            .expect("watcher didn't panic");
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
        // covered by the VM scenario tests).
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
    // r[verify builder.cancel.pre-cgroup-deferred+2]
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
}
