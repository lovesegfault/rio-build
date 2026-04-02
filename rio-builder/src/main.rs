//! rio-builder binary entry point.
//!
//! Wires up FUSE daemon, gRPC clients (ExecutorService + StoreService),
//! executor, and heartbeat loop.

mod config;

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::sync::{Semaphore, mpsc, watch};
use tracing::info;

use rio_builder::runtime::handle_prefetch_hint;
use rio_builder::{BuildSpawnContext, build_heartbeat_request, fuse, spawn_build_task};
use rio_proto::types::{ExecutorMessage, ExecutorRegister, executor_message, scheduler_message};

use config::{CliArgs, Config, detect_system};

/// Heartbeat interval. Shared source of truth with the scheduler's timeout
/// check (rio_common::limits::HEARTBEAT_TIMEOUT_SECS derives from this).
const HEARTBEAT_INTERVAL: Duration =
    Duration::from_secs(rio_common::limits::HEARTBEAT_INTERVAL_SECS);

impl rio_common::config::ValidateConfig for Config {
    /// Lives in main.rs (not config.rs) for cross-crate consistency
    /// with scheduler/gateway/controller/store — the validation
    /// target is the call-site, not the struct.
    fn validate(&self) -> anyhow::Result<()> {
        use rio_common::config::ensure_required as required;
        required(&self.scheduler_addr, "scheduler_addr", "builder")?;
        required(&self.store_addr, "store_addr", "builder")?;
        Ok(())
    }
}

impl rio_common::server::HasCommonConfig for Config {
    fn tls(&self) -> &rio_common::tls::TlsConfig {
        &self.tls
    }
    fn metrics_addr(&self) -> std::net::SocketAddr {
        self.metrics_addr
    }
    fn metric_labels(&self) -> Vec<(&'static str, String)> {
        // Fetcher pods share this binary. Without a role label, both
        // export identical rio_builder_* metrics — Prometheus can't
        // tell them apart.
        let role = match self.executor_kind {
            rio_proto::types::ExecutorKind::Builder => "builder",
            rio_proto::types::ExecutorKind::Fetcher => "fetcher",
        };
        vec![("role", role.into())]
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = CliArgs::parse();
    let rio_common::server::Bootstrap::<Config> {
        mut cfg,
        shutdown,
        otel_guard: _otel_guard,
    } = rio_common::server::bootstrap(
        "builder",
        cli,
        rio_proto::client::init_client_tls,
        rio_builder::describe_metrics,
    )?;

    let (executor_id, systems, features, ephemeral) = resolve_executor_identity(
        std::mem::take(&mut cfg.executor_id),
        std::mem::take(&mut cfg.systems),
        std::mem::take(&mut cfg.features),
    )?;
    validate_host_arch(cfg.executor_kind, &systems, &detect_system())?;

    let _root_guard =
        tracing::info_span!("builder", component = "builder", executor_id = %executor_id).entered();
    info!(
        version = env!("CARGO_PKG_VERSION"),
        ephemeral, "starting rio-builder"
    );

    // cgroup setup BEFORE the health server: if cgroup fails, we don't
    // want liveness passing while startup is hung on `?` propagation.
    // Pod goes straight to CrashLoopBackOff with a clear log line.
    let (cgroup_parent, resource_snapshot) = init_cgroup(&cfg.overlay_base_dir, shutdown.clone())?;

    // Readiness flag + HTTP health server. Spawned BEFORE gRPC connect
    // so liveness passes as soon as the process is up (connect may take
    // seconds if scheduler DNS is slow to resolve). Readiness stays
    // false until the first heartbeat comes back accepted — that's the
    // right gate: a worker that can't heartbeat is not useful capacity.
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    rio_builder::health::spawn_health_server(cfg.health_addr, Arc::clone(&ready));

    let Some((store_client, mut scheduler_client, _balance_guard)) =
        connect_upstreams(&cfg, &shutdown).await
    else {
        // Shutdown fired during cold-start connect. Clean exit —
        // nothing to drain (never connected), no FUSE mounted yet.
        return Ok(());
    };
    info!(
        %executor_id,
        scheduler_addr = %cfg.scheduler_addr,
        store_addr = %cfg.store_addr,
        systems = ?systems,
        features = ?features,
        "connected to gRPC services"
    );

    // Set up FUSE cache and mount. Arc so we can clone handles
    // BEFORE moving into mount_fuse_background: bloom_handle for
    // heartbeat, and the Arc itself for the prefetch handler.
    // Same extract-before-move pattern as bloom_handle always used.
    let cache = Arc::new(
        fuse::cache::Cache::new(
            cfg.fuse_cache_dir,
            cfg.fuse_cache_size_gb,
            cfg.bloom_expected_items,
        )
        .await?,
    );
    // Extract the bloom handle. The heartbeat loop reads from the
    // same RwLock that cache.insert() writes to — inserts by FUSE
    // ops show up in subsequent heartbeat snapshots.
    let heartbeat_bloom = cache.bloom_handle();
    // Clone for prefetch. Cache methods use runtime.block_on
    // internally (sync, designed for FUSE callbacks on dedicated
    // threads). The prefetch handler will call them via
    // spawn_blocking — async → nested-runtime panic.
    let prefetch_cache = Arc::clone(&cache);
    let prefetch_store = store_client.clone();
    let runtime = tokio::runtime::Handle::current();
    let prefetch_runtime = runtime.clone();
    // FUSE fetch timeout (60s default) — NOT GRPC_STREAM_TIMEOUT (300s).
    // FUSE is the build-critical path; a stalled fetch blocks a fuser
    // thread. See config.rs fuse_fetch_timeout_secs for the full rationale.
    let fuse_fetch_timeout = Duration::from_secs(cfg.fuse_fetch_timeout_secs);

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
        use std::os::unix::fs::PermissionsExt;
        let mode_755 = std::fs::Permissions::from_mode(0o755);
        let _ = std::fs::set_permissions(&cfg.overlay_base_dir, mode_755.clone());
        if let Some(parent) = cfg.overlay_base_dir.parent() {
            let _ = std::fs::set_permissions(parent, mode_755);
        }
    }

    let (fuse_session, fuse_circuit) = fuse::mount_fuse_background(
        &cfg.fuse_mount_point,
        cache,
        store_client.clone(),
        runtime,
        cfg.fuse_passthrough,
        cfg.fuse_threads,
        fuse_fetch_timeout,
    )?;

    // Prefetch concurrency limit. Separate from build_semaphore —
    // prefetch shouldn't compete with the build for slots. 8 is
    // conservative: each holds a tokio blocking-pool thread (default
    // pool is 512, so no starvation concern) AND pins an in-flight
    // gRPC stream to the store (which is what we're bounding —
    // don't DDoS the store with 100 parallel NARs when the scheduler
    // sends a big hint list).
    let prefetch_sem = Arc::new(Semaphore::new(8));

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
    let slot = Arc::new(rio_builder::runtime::BuildSlot::default());

    // I-063: drain state. Set true on first SIGTERM. Heartbeat reports
    // it (worker is authority); the assignment handler rejects while
    // set; the reconnect loop KEEPS the stream alive (completions for
    // in-flight builds reach whichever scheduler is leader) until
    // `drain_done` fires (slot idle via the same wait_idle()
    // synchronization the ephemeral path uses).
    let draining = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let drain_done = Arc::new(tokio::sync::Notify::new());

    // Ephemeral-done signal. Notified by the select loop after a
    // build is spawned AND its permit returns (CompletionReport was
    // sent — spawn_build_task's scopeguard drops the permit after
    // build_tx.send(completion).await). The select loop has an arm
    // that awaits this; in ephemeral mode, one notification = exit.
    //
    // Notify not oneshot: Notify is cheaper (no channel allocation)
    // and `notified()` is cancel-safe for select!. We only ever
    // fire it once in ephemeral mode; in STS mode it's never
    // notified (the select arm is conditionally added).
    let ephemeral_done = Arc::new(tokio::sync::Notify::new());
    // Spawn the ephemeral-done watcher task exactly ONCE (on the
    // first assignment), not per-assignment. AtomicBool swap(true)
    // returns the previous value — only the first caller sees false.
    // Lives outside the reconnect loop: a scheduler failover mid-
    // build must NOT spawn a second watcher (the build task keeps
    // running and returns its permit regardless of stream state).
    let ephemeral_watcher_spawned = std::sync::atomic::AtomicBool::new(false);

    // Latest generation observed in an accepted HeartbeatResponse.
    // Starts at 0 — scheduler generation is always ≥1 (lease/mod.rs
    // non-K8s path starts at 1; k8s Lease increments from 1 on first
    // acquire), so 0 never rejects a real assignment. Relaxed ordering:
    // this is a fence against a DIFFERENT process's stale writes, not a
    // within-process happens-before. The value itself is the signal.
    let latest_generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let heartbeat_handle = spawn_heartbeat(HeartbeatCtx {
        executor_id: executor_id.clone(),
        executor_kind: cfg.executor_kind,
        // move (not clone): owned Vecs, used only by the heartbeat
        // loop. main() has no further use for them.
        systems,
        features,
        size_class: cfg.size_class.clone(),
        slot: Arc::clone(&slot),
        ready: Arc::clone(&ready),
        resources: Arc::clone(&resource_snapshot),
        bloom: heartbeat_bloom,
        // FUSE circuit breaker: polled each tick. move into the task:
        // main() has no other use for the handle.
        circuit: fuse_circuit,
        draining: Arc::clone(&draining),
        generation: Arc::clone(&latest_generation),
        client: scheduler_client.clone(),
    });

    // Cancel registry: drv_path → (cgroup path, cancelled flag).
    // Populated by spawn_build_task after cgroup creation; the
    // Cancel handler below looks up and writes cgroup.kill.
    let cancel_registry = std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::<
        String,
        (std::path::PathBuf, Arc<std::sync::atomic::AtomicBool>),
    >::new()));

    // Shared context for spawning build tasks (clones done once per assignment
    // inside spawn_build_task, not here).
    let build_ctx = BuildSpawnContext {
        store_client,
        executor_id,
        fuse_mount_point: cfg.fuse_mount_point,
        overlay_base_dir: cfg.overlay_base_dir,
        // The permanent sink, NOT a per-connection gRPC channel.
        // Build tasks' sends never fail on scheduler failover.
        stream_tx: sink_tx,
        slot: Arc::clone(&slot),
        leaked_mounts: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        log_limits: rio_builder::log_stream::LogLimits {
            rate_lines_per_sec: cfg.log_rate_limit,
            total_bytes: cfg.log_size_limit,
        },
        max_leaked_mounts: cfg.max_leaked_mounts,
        daemon_timeout: Duration::from_secs(cfg.daemon_timeout_secs),
        max_silent_time: cfg.max_silent_time_secs,
        cgroup_parent,
        executor_kind: cfg.executor_kind,
        cancel_registry: Arc::clone(&cancel_registry),
    };

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
    //
    // select! is biased toward shutdown: poll it FIRST each iteration.
    // Without `biased;`, select! picks a ready branch pseudorandomly —
    // under heavy assignment traffic, the token could starve behind
    // stream messages. K8s sends SIGTERM then starts the grace period
    // clock; we want to react immediately, not after the next gap in
    // assignments.
    'reconnect: loop {
        // I-063: drain transition. swap is the test-and-set — only the
        // FIRST iteration after SIGTERM enters the body. Hoisted to the
        // top of 'reconnect (not inside the inner select) so it fires
        // regardless of which select arm was active when SIGTERM
        // arrived: all three `shutdown.cancelled()` arms below
        // `continue 'reconnect` to reach this. The reconnect loop then
        // KEEPS RUNNING — completions for in-flight builds reach the
        // current leader (even if the scheduler restarts under us) via
        // the same relay machinery as steady-state. Exit only when
        // `drain_done` fires (in_flight=0).
        if shutdown.is_cancelled() && !draining.swap(true, std::sync::atomic::Ordering::Relaxed) {
            info!(
                in_flight = u8::from(slot.is_busy()),
                "shutdown signal received, draining \
                 (stream stays connected for completion reports)"
            );
            // Watcher: same wait_idle synchronization the ephemeral
            // path uses. Spawned (not awaited): the reconnect loop
            // keeps the stream alive; the select arms below pick up
            // the notification.
            let watch_slot = Arc::clone(&slot);
            let done = Arc::clone(&drain_done);
            tokio::spawn(async move {
                watch_slot.wait_idle().await;
                done.notify_one();
            });
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
                    executor_id: build_ctx.executor_id.clone(),
                })),
            })
            .await?;

        // Swap in the new gRPC target. Relay resumes pumping.
        // send_replace because we don't care about the old value
        // (it's a dead channel or None).
        relay_target_tx.send_replace(Some(grpc_tx));

        let mut build_stream = match scheduler_client
            .build_execution(tokio_stream::wrappers::ReceiverStream::new(grpc_rx))
            .await
        {
            Ok(s) => s.into_inner(),
            Err(e) => {
                // Leader still settling, or balance channel hasn't
                // caught up. Back off and retry. The balanced
                // channel's probe loop rediscovers within ~3s.
                tracing::warn!(error = %e, "BuildExecution open failed; retrying in 1s");
                relay_target_tx.send_replace(None);
                tokio::select! {
                    biased;
                    // First SIGTERM: skip the sleep, transition at the
                    // top of 'reconnect, then resume reconnecting.
                    _ = shutdown.cancelled(),
                        if !draining.load(std::sync::atomic::Ordering::Relaxed)
                        => continue 'reconnect,
                    _ = drain_done.notified() => break 'reconnect,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => continue 'reconnect,
                }
            }
        };
        info!("BuildExecution stream open");

        let stream_end = loop {
            if heartbeat_handle.is_finished() {
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
                _ = shutdown.cancelled(),
                    if !draining.load(std::sync::atomic::Ordering::Relaxed) => {
                    continue 'reconnect;
                }

                // Drain complete: in_flight=0, all completions reported.
                // Guard: don't poll Notify before the watcher exists.
                _ = drain_done.notified(),
                    if draining.load(std::sync::atomic::Ordering::Relaxed) => {
                    break 'reconnect;
                }

                // Ephemeral single-shot exit. Guarded on `ephemeral`
                // so STS-mode workers don't pay the Notify poll cost
                // (select! with an always-pending arm is cheap, but
                // why bother). The watcher task spawned after
                // spawn_build_task fires this once the build's
                // permit returns.
                //
                // biased; ordering: this comes AFTER shutdown (SIGTERM
                // always wins) but BEFORE the stream arm. If a Cancel
                // arrives at the same instant the build completes, we
                // prefer to exit (the build is done; Cancel is moot).
                _ = ephemeral_done.notified(), if ephemeral => {
                    info!("ephemeral build complete; exiting single-shot mode");
                    break StreamEnd::EphemeralDone;
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

                    match msg.msg {
                        Some(scheduler_message::Msg::Assignment(assignment)) => {
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
                            let latest = latest_generation
                                .load(std::sync::atomic::Ordering::Relaxed);
                            if rio_builder::runtime::is_stale_assignment(
                                assignment.generation,
                                latest,
                            ) {
                                info!(
                                    drv_path = %assignment.drv_path,
                                    assignment_gen = assignment.generation,
                                    latest_gen = latest,
                                    "rejecting stale-generation assignment (deposed leader)"
                                );
                                metrics::counter!(
                                    "rio_builder_stale_assignments_rejected_total"
                                )
                                .increment(1);
                                continue;
                            }
                            // I-063: belt-and-suspenders. Heartbeat
                            // carries `draining` so the scheduler
                            // shouldn't dispatch here, but the next
                            // heartbeat is up to 10s away. No ACK
                            // sent — same rationale as the stale-
                            // generation reject above.
                            if draining.load(std::sync::atomic::Ordering::Relaxed) {
                                info!(
                                    drv_path = %assignment.drv_path,
                                    "rejecting assignment while draining"
                                );
                                continue;
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
                            // shows running_builds nonempty (its bug,
                            // or a race within one heartbeat tick).
                            // No ACK sent — same rationale as the
                            // stale-generation reject above.
                            let Some(guard) = slot.try_claim(&assignment.drv_path) else {
                                tracing::warn!(
                                    drv_path = %assignment.drv_path,
                                    running = ?slot.running(),
                                    "rejecting assignment: slot busy \
                                     (scheduler dispatched while heartbeat shows running)"
                                );
                                continue;
                            };

                            spawn_build_task(assignment, guard, &build_ctx).await;

                            // r[impl ctrl.pool.ephemeral]
                            // Ephemeral mode: after spawning the ONE
                            // build, wait for its permit to return
                            // (build complete + CompletionReport sent
                            // — spawn_build_task's scopeguard drops
                            // the permit after the send), then exit.
                            //
                            // This is the single-shot gate. The select
                            // arm below on `ephemeral_done.notified()`
                            // breaks the inner loop with
                            // StreamEnd::EphemeralDone → outer loop
                            // breaks → run_drain (which is a no-op
                            // here: slot already idle, DrainExecutor
                            // deregisters us) → FUSE drop
                            // → exit 0 → pod terminates → Job complete.
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
                            // build runs. An ephemeral build MUST
                            // still be cancellable (a stuck 2h build
                            // in a Job pod is wasted compute). The
                            // watcher runs concurrently; select still
                            // processes Cancel.
                            //
                            // swap(true) gates to ONCE: the first
                            // assignment sees false and spawns; any
                            // subsequent assignment (shouldn't happen
                            // — one build per pod, but belt-and-
                            // suspenders against scheduler bugs) sees
                            // true and skips.
                            if ephemeral
                                && !ephemeral_watcher_spawned
                                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                            {
                                let watch_slot = Arc::clone(&slot);
                                let done = Arc::clone(&ephemeral_done);
                                tokio::spawn(async move {
                                    watch_slot.wait_idle().await;
                                    done.notify_one();
                                });
                            }
                        }
                        Some(scheduler_message::Msg::Cancel(cancel)) => {
                            info!(
                                drv_path = %cancel.drv_path,
                                reason = %cancel.reason,
                                "received cancel signal"
                            );
                            rio_builder::runtime::try_cancel_build(
                                &cancel_registry,
                                &cancel.drv_path,
                            );
                        }
                        Some(scheduler_message::Msg::Prefetch(prefetch)) => {
                            tracing::debug!(
                                paths = prefetch.store_paths.len(),
                                "received prefetch hint"
                            );
                            handle_prefetch_hint(
                                prefetch,
                                Arc::clone(&prefetch_cache),
                                prefetch_store.clone(),
                                prefetch_runtime.clone(),
                                Arc::clone(&prefetch_sem),
                                fuse_fetch_timeout,
                                // Warm-gate ACK goes through the
                                // permanent sink (same as completions
                                // and log batches) — survives stream
                                // reconnect. A worker that warms then
                                // briefly loses its stream still
                                // delivers PrefetchComplete to the new
                                // leader once the relay reconnects.
                                build_ctx.stream_tx.clone(),
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
            StreamEnd::EphemeralDone => break 'reconnect,
            StreamEnd::Closed | StreamEnd::Error => {
                // Swap relay target to None — relay buffers until
                // we open the next stream. Running builds' send()s
                // to the permanent sink succeed; the relay just
                // holds them. 256-cap sink → up to 256 messages
                // buffered before build tasks block on send. At
                // typical log rates (100ms batch flush), that's
                // ~25s of buffer — far more than the ~1s gap.
                tracing::warn!(
                    running = ?build_ctx.slot.running(),
                    "BuildExecution stream ended; reconnecting (running build continues)"
                );
                relay_target_tx.send_replace(None);
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled(),
                        if !draining.load(std::sync::atomic::Ordering::Relaxed)
                        => continue 'reconnect,
                    _ = drain_done.notified() => break 'reconnect,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => continue 'reconnect,
                }
            }
        }
    }

    // Exit deregister. By now in_flight=0 (drain_done fired, or
    // ephemeral's single build returned its permit). DrainExecutor
    // here is the explicit "I'm leaving" — heartbeat already told
    // the scheduler `draining=true` during the wait. Best-effort:
    // 50% chance of standby (I-046), but the stream-close that
    // follows (process exit drops the bidi) triggers
    // ExecutorDisconnected anyway.
    run_drain(&cfg.scheduler_addr, &build_ctx.executor_id).await;

    // Dropping BackgroundSession:
    //   - detaches the FUSE thread (BackgroundSession has NO Drop impl)
    //   - drops the inner Mount, whose Drop DOES call fusermount -u
    //     → kernel unmounts → sends DESTROY to the FUSE session
    //     → detached FUSE thread processes DESTROY → Filesystem::destroy()
    //       runs (flushes passthrough-failure stats, profraw)
    //     → FUSE thread exits (but we're not joining it)
    //
    // The race: main thread can reach libc exit() before the detached
    // FUSE thread processes DESTROY → destroy() never runs → profraw
    // lost for that code. The short sleep gives the FUSE thread time
    // to process DESTROY in the common case. It's best-effort — if the
    // mount is busy (fusermount fails EBUSY) or the FUSE thread is
    // stuck on a slow request, destroy() won't run. That's fine:
    // kernel unmounts on process death anyway (the fd closes), and
    // the next worker instance can remount.
    //
    // Why not umount_and_join()? It takes self by value — if it
    // blocks (busy mount → join never returns), there's no clean way
    // to fall back to the Drop path without fuse_session ownership
    // gymnastics. The Drop path is already correct for shutdown
    // (mount cleaned up, process exits); it's only the profraw
    // flush we're optimizing for, and a sleep is sufficient.
    drop(fuse_session);
    std::thread::sleep(std::time::Duration::from_millis(200));

    Ok(())
}

/// Exit-time deregister. The wait-for-in-flight is already done by
/// the drain watcher (or ephemeral's permit return) before we get
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

/// Why the inner select loop exited. EphemeralDone breaks the outer
/// reconnect loop; Closed/Error trigger a reconnect. Shutdown no
/// longer flows through here — the SIGTERM arm `continue 'reconnect`s
/// directly so the drain transition runs, then `drain_done` (in_flight
/// =0) `break 'reconnect`s directly.
enum StreamEnd {
    Closed,
    Error,
    /// Ephemeral mode: one build completed. Exit the process so
    /// the pod terminates → Job completes → ttlSecondsAfterFinished
    /// reaps it. See `RIO_EPHEMERAL` handling in main().
    EphemeralDone,
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

/// Resolve executor_id / systems / features / ephemeral from config +
/// environment. Consumes the config's owned fields (caller passes via
/// `mem::take` — main() has no further use for them).
///
/// Errors if executor_id is empty AND gethostname() fails — two workers
/// with the same ID would steal each other's builds via heartbeat
/// merging, so we fail hard rather than silently colliding on "unknown".
fn resolve_executor_identity(
    executor_id: String,
    systems: Vec<String>,
    features: Vec<String>,
) -> anyhow::Result<(String, Vec<String>, Vec<String>, bool)> {
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
    // Every nix-daemon supports builtin:fetchurl — it's handled
    // internally, no real process forked. Bootstrap derivations
    // (busybox, bootstrap-tools) have system="builtin"; without
    // this, a cold store permanently stalls at the DAG leaves.
    if !systems.iter().any(|s| s == "builtin") {
        systems.push("builtin".to_string());
    }
    // features: no auto-detect. Empty is valid (worker supports no
    // special features). Operator sets these explicitly in the CRD
    // — auto-detecting "kvm" by checking /dev/kvm exists would be
    // surprising (worker on a kvm-capable host but operator wants
    // to reserve it for other work).

    // r[impl ctrl.pool.ephemeral]
    // Ephemeral mode: controller's build_job (ephemeral.rs) sets
    // RIO_EPHEMERAL=1 on Job pods. Worker exits after the one build
    // completes → pod terminates → Job goes Complete →
    // ttlSecondsAfterFinished reaps it. Fresh pod per build = zero
    // cross-build state (FUSE cache, overlayfs upper, filesystem
    // are all emptyDir, wiped on pod termination).
    //
    // is_ok() not == "1": the controller sets "1" but any non-empty
    // value is a clear intent signal. Matches the pattern at
    // builders.rs:554 for LLVM_PROFILE_FILE (var_os.is_some).
    let ephemeral = std::env::var("RIO_EPHEMERAL").is_ok();

    Ok((executor_id, systems, features, ephemeral))
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
) -> anyhow::Result<(
    std::path::PathBuf,
    rio_builder::cgroup::ResourceSnapshotHandle,
)> {
    let cgroup_parent = rio_builder::cgroup::delegated_root()
        .map_err(|e| anyhow::anyhow!("cgroup v2 required: {e}"))?;
    rio_builder::cgroup::enable_subtree_controllers(&cgroup_parent)
        .map_err(|e| anyhow::anyhow!("cgroup delegation required: {e}"))?;
    info!(cgroup = %cgroup_parent.display(), "cgroup v2 subtree ready");

    // Background utilization reporter: polls parent cgroup cpu.stat +
    // memory.current/max every 10s → Prometheus gauges AND the shared
    // snapshot the heartbeat loop reads for ResourceUsage. Single
    // sampling site means Prometheus and ListExecutors always agree.
    // Shutdown token lets the 10s sleep break immediately on SIGTERM
    // so main() can return and profraw flush.
    let resource_snapshot: rio_builder::cgroup::ResourceSnapshotHandle = Default::default();
    rio_common::task::spawn_monitored(
        "cgroup-utilization-reporter",
        rio_builder::cgroup::utilization_reporter_loop_with_shutdown(
            cgroup_parent.clone(),
            overlay_base_dir.to_path_buf(),
            std::sync::Arc::clone(&resource_snapshot),
            shutdown,
        ),
    );

    Ok((cgroup_parent, resource_snapshot))
}

type StoreClient = rio_proto::StoreServiceClient<tonic::transport::Channel>;
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
    cfg: &Config,
    shutdown: &rio_common::signal::Token,
) -> Option<(StoreClient, WorkerClient, BalanceGuards)> {
    rio_proto::client::connect_with_retry(
        shutdown,
        || async {
            let (store, store_guard) = match &cfg.store_balance_host {
                None => {
                    info!(addr = %cfg.store_addr, "store: single-channel mode");
                    let c = rio_proto::client::connect_store(&cfg.store_addr).await?;
                    (c, None)
                }
                Some(host) => {
                    info!(
                        %host, port = cfg.store_balance_port,
                        "store: health-aware balanced mode"
                    );
                    let (c, bc) = rio_proto::client::balance::connect_store_balanced(
                        host.clone(),
                        cfg.store_balance_port,
                    )
                    .await?;
                    (c, Some(bc))
                }
            };
            let (sched, sched_guard) = match &cfg.scheduler_balance_host {
                None => {
                    info!(addr = %cfg.scheduler_addr, "scheduler: single-channel mode");
                    let c = rio_proto::client::connect_executor(&cfg.scheduler_addr).await?;
                    (c, None)
                }
                Some(host) => {
                    info!(
                        %host, port = cfg.scheduler_balance_port,
                        "scheduler: health-aware balanced mode"
                    );
                    let (c, bc) = rio_proto::client::balance::connect_executor_balanced(
                        host.clone(),
                        cfg.scheduler_balance_port,
                    )
                    .await?;
                    (c, Some(bc))
                }
            };
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
    slot: Arc<rio_builder::runtime::BuildSlot>,
    ready: Arc<std::sync::atomic::AtomicBool>,
    resources: rio_builder::cgroup::ResourceSnapshotHandle,
    bloom: Arc<std::sync::RwLock<rio_common::bloom::BloomFilter>>,
    circuit: Arc<rio_builder::fuse::circuit::CircuitBreaker>,
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
        bloom,
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
                Some(&bloom),
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
    use rio_common::config::ValidateConfig as _;

    // -----------------------------------------------------------------------
    // validate_config rejection tests — spreads the P0409 pattern
    // (rio-scheduler/src/main.rs) to the worker.
    // -----------------------------------------------------------------------

    /// Both required fields filled with placeholders. Rejection
    /// tests patch ONE field to prove that specific check fires.
    fn test_valid_config() -> Config {
        Config {
            scheduler_addr: "http://localhost:9000".into(),
            store_addr: "http://localhost:9001".into(),
            ..Config::default()
        }
    }

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

    #[test]
    fn config_rejects_empty_addrs() {
        type Patch = fn(&mut Config);
        let cases: &[(&str, Patch)] = &[
            ("scheduler_addr", |c| c.scheduler_addr = String::new()),
            ("store_addr", |c| c.store_addr = String::new()),
        ];
        for (field, patch) in cases {
            let mut cfg = test_valid_config();
            patch(&mut cfg);
            let err = cfg
                .validate()
                .expect_err("cleared required field must be rejected")
                .to_string();
            assert!(
                err.contains(field),
                "error for cleared {field} must name it: {err}"
            );
        }
    }

    /// Whitespace-only scheduler_addr must be rejected as empty.
    /// Regression guard for `ensure_required`'s trim — pre-helper,
    /// bare `is_empty()` accepted `"   "`, startup failed later at
    /// gRPC connect with "invalid socket address syntax: '  '".
    #[test]
    fn config_rejects_whitespace_scheduler_addr() {
        let mut cfg = test_valid_config();
        cfg.scheduler_addr = "   ".into();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("scheduler_addr is required"),
            "whitespace-only scheduler_addr must be rejected as empty, got: {err}"
        );
    }

    /// Baseline: `test_valid_config()` itself passes — proves
    /// rejection tests test ONLY their mutation.
    #[test]
    fn config_accepts_valid() {
        test_valid_config()
            .validate()
            .expect("valid config should pass");
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
        use rio_builder::runtime::BuildSlot;
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

    /// Missed-notification race: guard drops BETWEEN the watcher's
    /// `is_busy()` check and its `notified().await`. Exercises the
    /// `enable()` ordering in `BuildSlot::wait_idle`.
    #[tokio::test(start_paused = true)]
    async fn slot_wait_idle_no_missed_notification() {
        use rio_builder::runtime::BuildSlot;
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
}
