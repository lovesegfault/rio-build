//! rio-worker binary entry point.
//!
//! Wires up FUSE daemon, gRPC clients (WorkerService + StoreService),
//! executor, and heartbeat loop.

mod config;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::sync::{RwLock, Semaphore, mpsc, watch};
use tracing::{info, warn};

use rio_proto::types::{WorkerMessage, WorkerRegister, scheduler_message, worker_message};
use rio_worker::runtime::handle_prefetch_hint;
use rio_worker::{BuildSpawnContext, build_heartbeat_request, fuse, spawn_build_task};

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
        required(&self.scheduler_addr, "scheduler_addr", "worker")?;
        required(&self.store_addr, "store_addr", "worker")?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // rustls CryptoProvider install BEFORE any TLS use. Phase 3b
    // enables tonic tls-aws-lc; without this, rustls panics on
    // first handshake (aws-lc-rs feature active but no provider
    // installed means auto-select fails).
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli = CliArgs::parse();
    let mut cfg: Config = rio_common::config::load("worker", cli)?;
    let _otel_guard = rio_common::observability::init_tracing("worker")?;

    // One token, cancelled on SIGTERM OR SIGINT. Cloned into every
    // loop that must break for main() to return (profraw flush,
    // FUSE Drop).
    let shutdown = rio_common::signal::shutdown_signal();

    // Client TLS init BEFORE connect_store/connect_worker. Same
    // pattern as gateway: one config, all outgoing connections.
    // server_name matches the most common target (scheduler);
    // actual SAN verification uses the :authority header from
    // the endpoint URL (K8s DNS: "rio-scheduler", "rio-store").
    rio_proto::client::init_client_tls(
        rio_common::tls::load_client_tls(&cfg.tls)
            .map_err(|e| anyhow::anyhow!("TLS config: {e}"))?,
    );
    if cfg.tls.is_configured() {
        info!("client mTLS enabled for outgoing gRPC");
    }

    use rio_common::config::ValidateConfig as _;
    cfg.validate()?;

    let (worker_id, systems, features, ephemeral) = resolve_worker_identity(
        std::mem::take(&mut cfg.worker_id),
        std::mem::take(&mut cfg.systems),
        std::mem::take(&mut cfg.features),
    )?;

    let _root_guard =
        tracing::info_span!("worker", component = "worker", worker_id = %worker_id).entered();
    info!(
        version = env!("CARGO_PKG_VERSION"),
        ephemeral, "starting rio-worker"
    );

    rio_common::observability::init_metrics(cfg.metrics_addr)?;
    rio_worker::describe_metrics();

    // cgroup setup BEFORE the health server: if cgroup fails, we don't
    // want liveness passing while startup is hung on `?` propagation.
    // Pod goes straight to CrashLoopBackOff with a clear log line.
    let (cgroup_parent, resource_snapshot) = init_cgroup(&cfg.overlay_base_dir)?;

    // Readiness flag + HTTP health server. Spawned BEFORE gRPC connect
    // so liveness passes as soon as the process is up (connect may take
    // seconds if scheduler DNS is slow to resolve). Readiness stays
    // false until the first heartbeat comes back accepted — that's the
    // right gate: a worker that can't heartbeat is not useful capacity.
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    rio_worker::health::spawn_health_server(cfg.health_addr, Arc::clone(&ready));

    let (store_client, mut scheduler_client, _balance_guard) = connect_upstreams(&cfg).await;
    info!(
        %worker_id,
        scheduler_addr = %cfg.scheduler_addr,
        store_addr = %cfg.store_addr,
        max_builds = cfg.max_builds,
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

    std::fs::create_dir_all(&cfg.fuse_mount_point)?;
    std::fs::create_dir_all(&cfg.overlay_base_dir)?;

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
    // prefetch shouldn't compete with builds for slots. 8 is
    // conservative: each holds a tokio blocking-pool thread (default
    // pool is 512, so no starvation concern) AND pins an in-flight
    // gRPC stream to the store (which is what we're bounding —
    // don't DDoS the store with 100 parallel NARs when the scheduler
    // sends a big hint list).
    //
    // Why not max_builds? Prefetch is OPPORTUNISTIC — it's about
    // warming the cache BEFORE the build needs those paths. If we
    // limited to max_builds, a worker with max_builds=1 would
    // prefetch serially (one at a time) which defeats "get ahead."
    // 8 is enough parallelism to saturate a typical store without
    // overwhelming it.
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
    let (sink_tx, sink_rx) = mpsc::channel::<WorkerMessage>(256);

    // Relay target: Some(grpc_tx) while connected, None during
    // the reconnect gap. Starts None — the reconnect loop sets
    // it before opening the first stream.
    let (relay_target_tx, relay_target_rx) =
        watch::channel::<Option<mpsc::Sender<WorkerMessage>>>(None);

    rio_common::task::spawn_monitored("stream-relay", relay_loop(sink_rx, relay_target_rx));

    // Concurrent build semaphore
    let build_semaphore = Arc::new(Semaphore::new(cfg.max_builds as usize));

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
    // first assignment), not per-assignment. Per-assignment spawning
    // is wasteful (N watchers all blocked on acquire_many) and only
    // happened to work when max_builds=1. AtomicBool swap(true)
    // returns the previous value — only the first caller sees false.
    // Lives outside the reconnect loop: a scheduler failover mid-
    // build must NOT spawn a second watcher (the build task keeps
    // running and returns its permit regardless of stream state).
    let ephemeral_watcher_spawned = std::sync::atomic::AtomicBool::new(false);

    // Track running builds (drv_path set) for heartbeat reporting
    let running_builds: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));

    // Latest generation observed in an accepted HeartbeatResponse.
    // Starts at 0 — scheduler generation is always ≥1 (lease/mod.rs
    // non-K8s path starts at 1; k8s Lease increments from 1 on first
    // acquire), so 0 never rejects a real assignment. Relaxed ordering:
    // this is a fence against a DIFFERENT process's stale writes, not a
    // within-process happens-before. The value itself is the signal.
    let latest_generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let heartbeat_handle = spawn_heartbeat(HeartbeatCtx {
        worker_id: worker_id.clone(),
        // move (not clone): owned Vecs, used only by the heartbeat
        // loop. main() has no further use for them.
        systems,
        features,
        max_builds: cfg.max_builds,
        size_class: cfg.size_class.clone(),
        running: Arc::clone(&running_builds),
        ready: Arc::clone(&ready),
        resources: Arc::clone(&resource_snapshot),
        bloom: heartbeat_bloom,
        // FUSE circuit breaker: polled each tick. move into the task:
        // main() has no other use for the handle.
        circuit: fuse_circuit,
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
        worker_id,
        fuse_mount_point: cfg.fuse_mount_point,
        overlay_base_dir: cfg.overlay_base_dir,
        // The permanent sink, NOT a per-connection gRPC channel.
        // Build tasks' sends never fail on scheduler failover.
        stream_tx: sink_tx,
        running_builds,
        leaked_mounts: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        log_limits: rio_worker::log_stream::LogLimits {
            rate_lines_per_sec: cfg.log_rate_limit,
            total_bytes: cfg.log_size_limit,
        },
        max_leaked_mounts: cfg.max_leaked_mounts,
        daemon_timeout: Duration::from_secs(cfg.daemon_timeout_secs),
        max_silent_time: cfg.max_silent_time_secs,
        cgroup_parent,
        cancel_registry: Arc::clone(&cancel_registry),
        fod_proxy_url: cfg.fod_proxy_url,
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
        // Fresh per-connection outbound channel. The relay pumps
        // the permanent sink into this; when the gRPC bidi dies,
        // grpc_rx (wrapped in ReceiverStream → build_execution)
        // is dropped → grpc_tx.send() fails in the relay → relay
        // buffers and waits for the next swap.
        let (grpc_tx, grpc_rx) = mpsc::channel::<WorkerMessage>(256);

        // WorkerRegister MUST be the first message. Send it on
        // grpc_tx directly (not via the sink — we want it to go
        // out on THIS connection, not be buffered by a relay that
        // might still be pointing at the old channel).
        grpc_tx
            .send(WorkerMessage {
                msg: Some(worker_message::Msg::Register(WorkerRegister {
                    worker_id: build_ctx.worker_id.clone(),
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
                    _ = shutdown.cancelled() => break 'reconnect,
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
                // if it's dead, DrainWorker won't land anyway.
                anyhow::bail!("heartbeat loop terminated unexpectedly");
            }

            tokio::select! {
                biased;

                // r[impl worker.shutdown.sigint]
                _ = shutdown.cancelled() => {
                    break StreamEnd::Shutdown;
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
                            if rio_worker::runtime::is_stale_assignment(
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
                                    "rio_worker_stale_assignments_rejected_total"
                                )
                                .increment(1);
                                continue;
                            }
                            info!(
                                drv_path = %assignment.drv_path,
                                generation = assignment.generation,
                                "received work assignment"
                            );

                            // Acquire permit BEFORE ACKing: don't tell the
                            // scheduler we accepted work we can't immediately
                            // start. On Err(Closed): semaphore.close() was
                            // called — impossible here (close happens in the
                            // drain path below, AFTER loop exit), so this is
                            // a bug. Break with a distinct reason.
                            let permit = match Arc::clone(&build_semaphore).acquire_owned().await {
                                Ok(p) => p,
                                Err(_) => {
                                    tracing::error!("semaphore closed mid-loop (bug)");
                                    break StreamEnd::Error;
                                }
                            };

                            spawn_build_task(assignment, permit, &build_ctx).await;

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
                            // here: acquire_many succeeds immediately,
                            // DrainWorker deregisters us) → FUSE drop
                            // → exit 0 → pod terminates → Job complete.
                            //
                            // Why not break immediately here: the build
                            // is still RUNNING (spawn_build_task
                            // returned, but the spawned task is live).
                            // We need to wait for it to finish AND for
                            // the CompletionReport to land in the
                            // scheduler. The permit-return is the
                            // synchronization point — same mechanism
                            // run_drain uses.
                            //
                            // Why spawn a watcher task (not inline
                            // acquire_many here): inlining would block
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
                            // in ephemeral mode — CRD CEL enforces
                            // max=1, build_job defensively overrides
                            // RIO_MAX_BUILDS to 1 — but belt-and-
                            // suspenders) sees true and skips.
                            // Previously this spawned a fresh watcher
                            // per assignment — with max>1, N watchers
                            // all blocked on acquire_many(N), all fire
                            // at once when the Nth build completes.
                            // Wasteful and only accidentally correct
                            // for max=1. Under permit churn
                            // (scheduler dispatches N<max then M more
                            // before N completes) acquire_many may
                            // never win; CEL forbids the config that
                            // exposes the race.
                            if ephemeral
                                && !ephemeral_watcher_spawned
                                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                            {
                                let sem = Arc::clone(&build_semaphore);
                                let max = cfg.max_builds;
                                let done = Arc::clone(&ephemeral_done);
                                tokio::spawn(async move {
                                    // acquire_many(max) succeeds when
                                    // ALL permits are back — i.e., the
                                    // one build's OwnedPermit dropped.
                                    // Same synchronization point as
                                    // run_drain (see its step-2 comment
                                    // about NOT close()'ing first).
                                    if sem.acquire_many(max).await.is_ok() {
                                        done.notify_one();
                                    }
                                    // Err = semaphore closed (can't
                                    // happen — close is never called).
                                    // Silently return; the heartbeat
                                    // loop dying is a separate bail.
                                });
                            }
                        }
                        Some(scheduler_message::Msg::Cancel(cancel)) => {
                            info!(
                                drv_path = %cancel.drv_path,
                                reason = %cancel.reason,
                                "received cancel signal"
                            );
                            rio_worker::runtime::try_cancel_build(
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
            StreamEnd::Shutdown | StreamEnd::EphemeralDone => break 'reconnect,
            StreamEnd::Closed | StreamEnd::Error => {
                // Swap relay target to None — relay buffers until
                // we open the next stream. Running builds' send()s
                // to the permanent sink succeed; the relay just
                // holds them. 256-cap sink → up to 256 messages
                // buffered before build tasks block on send. At
                // typical log rates (100ms batch flush), that's
                // ~25s of buffer — far more than the ~1s gap.
                tracing::warn!(
                    running = build_ctx.running_builds.read().await.len(),
                    "BuildExecution stream ended; reconnecting (running builds continue)"
                );
                relay_target_tx.send_replace(None);
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break 'reconnect,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => continue 'reconnect,
                }
            }
        }
    }

    run_drain(
        &build_semaphore,
        cfg.max_builds,
        &cfg.scheduler_addr,
        &build_ctx.worker_id,
    )
    .await;

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

/// Execute the shutdown drain sequence (SIGTERM or SIGINT).
///
/// K8s preStop (controller.md:211-216):
///   1. DrainWorker: scheduler stops sending assignments
///   2. Wait for in-flight builds
///   3. (Outputs already uploaded by each build task — no step here)
///   4. Exit 0
///
/// terminationGracePeriodSeconds=7200 (2h). If we exceed that,
/// SIGKILL — builds lost. 2h is enough for ~any single build; the
/// ones that take longer (LLVM+ccache-cold) are rare.
///
/// Only called on shutdown signal. Stream close/error is handled
/// by the reconnect loop — we no longer exit on those.
async fn run_drain(semaphore: &Semaphore, max_builds: u32, scheduler_addr: &str, worker_id: &str) {
    info!(
        in_flight = max_builds as usize - semaphore.available_permits(),
        "shutdown signal received, draining"
    );

    // Step 1: DrainWorker. Best-effort — if it fails (scheduler
    // unreachable, actor dead), log and continue. The scheduler
    // will eventually time us out via heartbeat (we're still
    // heartbeating until exit) and WorkerDisconnected will
    // reassign. We lose nothing by trying.
    //
    // Fresh admin client: ClusterIP Service works here (we're
    // a one-shot unary call at shutdown; 50% chance of hitting
    // standby → UNAVAILABLE → we log and move on, same as any
    // other DrainWorker failure).
    match rio_proto::client::connect_admin(scheduler_addr).await {
        Ok(mut admin) => {
            match admin
                .drain_worker(rio_proto::types::DrainWorkerRequest {
                    worker_id: worker_id.to_string(),
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
                }
                Err(e) => {
                    tracing::warn!(error = %e, "DrainWorker RPC failed; continuing drain");
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "admin connect failed; continuing drain without DrainWorker");
        }
    }

    // Step 2: wait for in-flight. acquire_many(max_builds) succeeds
    // when ALL permits are returned — every build task dropped its
    // OwnedPermit on completion. This is the synchronization point:
    // when this returns, no build is mid-upload.
    //
    // DON'T close() before acquire_many: close makes waiting
    // acquires return Err even when permits return. The loop
    // already broke — no new acquires can happen.
    match semaphore.acquire_many(max_builds).await {
        Ok(_all_permits) => {
            info!("all in-flight builds complete");
        }
        Err(_) => {
            tracing::error!("semaphore closed unexpectedly (bug); exiting anyway");
        }
    }

    info!("drain complete, exiting");
}

/// Why the inner select loop exited. Shutdown and EphemeralDone
/// break the outer reconnect loop; Closed/Error trigger a reconnect.
enum StreamEnd {
    Shutdown,
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
    mut sink_rx: mpsc::Receiver<WorkerMessage>,
    mut target: watch::Receiver<Option<mpsc::Sender<WorkerMessage>>>,
) {
    // One-message buffer for the transition case: we recv'd from
    // the sink, tried to send to gRPC, gRPC channel is dead.
    // `mpsc::error::SendError<T>` holds the unsent message —
    // extract it, wait for the next target swap, retry.
    let mut buffered: Option<WorkerMessage> = None;

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

        // Pump until this gRPC target dies.
        loop {
            let Some(msg) = sink_rx.recv().await else {
                // Permanent sink closed — all sink_tx clones
                // dropped. BuildSpawnContext holds one for process
                // lifetime, so this is shutdown (main returning).
                return;
            };
            if let Err(e) = grpc_tx.send(msg).await {
                // gRPC channel died (reconnect loop is about to
                // or already did swap the target to None). Buffer
                // this one message and go back to the top.
                buffered = Some(e.0);
                break;
            }
        }
    }
}

// ── bootstrap helpers (extracted from main) ──────────────────────────

/// Resolve worker_id / systems / features / ephemeral from config +
/// environment. Consumes the config's owned fields (caller passes via
/// `mem::take` — main() has no further use for them).
///
/// Errors if worker_id is empty AND gethostname() fails — two workers
/// with the same ID would steal each other's builds via heartbeat
/// merging, so we fail hard rather than silently colliding on "unknown".
fn resolve_worker_identity(
    worker_id: String,
    systems: Vec<String>,
    features: Vec<String>,
) -> anyhow::Result<(String, Vec<String>, Vec<String>, bool)> {
    let worker_id = if worker_id.is_empty() {
        nix::unistd::gethostname()
            .ok()
            .and_then(|h| h.into_string().ok())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "cannot determine worker_id: gethostname() failed and \
                     worker_id not set (--worker-id, RIO_WORKER_ID, or worker.toml)"
                )
            })?
    } else {
        worker_id
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
    // RIO_EPHEMERAL=1 on Job pods. Worker exits after one build
    // round (all maxConcurrentBuilds permits back; CRD CEL enforces
    // max=1 in ephemeral mode so "one build round" == "one build")
    // → pod terminates → Job goes Complete → ttlSecondsAfterFinished
    // reaps it. Fresh pod per build = zero cross-build state (FUSE
    // cache, overlayfs upper, filesystem are all emptyDir, wiped on
    // pod termination).
    //
    // is_ok() not == "1": the controller sets "1" but any non-empty
    // value is a clear intent signal. Matches the pattern at
    // builders.rs:554 for LLVM_PROFILE_FILE (var_os.is_some).
    let ephemeral = std::env::var("RIO_EPHEMERAL").is_ok();

    Ok((worker_id, systems, features, ephemeral))
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
) -> anyhow::Result<(
    std::path::PathBuf,
    rio_worker::cgroup::ResourceSnapshotHandle,
)> {
    let cgroup_parent = rio_worker::cgroup::delegated_root()
        .map_err(|e| anyhow::anyhow!("cgroup v2 required: {e}"))?;
    rio_worker::cgroup::enable_subtree_controllers(&cgroup_parent)
        .map_err(|e| anyhow::anyhow!("cgroup delegation required: {e}"))?;
    info!(cgroup = %cgroup_parent.display(), "cgroup v2 subtree ready");

    // Background utilization reporter: polls parent cgroup cpu.stat +
    // memory.current/max every 10s → Prometheus gauges AND the shared
    // snapshot the heartbeat loop reads for ResourceUsage. Single
    // sampling site means Prometheus and ListWorkers always agree.
    let resource_snapshot: rio_worker::cgroup::ResourceSnapshotHandle = Default::default();
    rio_common::task::spawn_monitored(
        "cgroup-utilization-reporter",
        rio_worker::cgroup::utilization_reporter_loop(
            cgroup_parent.clone(),
            overlay_base_dir.to_path_buf(),
            std::sync::Arc::clone(&resource_snapshot),
        ),
    );

    Ok((cgroup_parent, resource_snapshot))
}

type StoreClient = rio_proto::StoreServiceClient<tonic::transport::Channel>;
type WorkerClient = rio_proto::WorkerServiceClient<tonic::transport::Channel>;
type BalanceGuard = Option<rio_proto::client::balance::BalancedChannel>;

/// Retry-until-connected store + scheduler clients.
///
/// Cold-start race (see rio-controller/src/main.rs for the full
/// story): store/scheduler Services may have no endpoints yet. Bare
/// `?` → process exit → kubelet sees exit-after-liveness-passed →
/// restart → same race → flapping. Retry internally: /healthz stays
/// 200 (process IS alive, restart won't help), /readyz stays 503
/// (ready flag won't flip until first heartbeat accepted, far past
/// this loop).
///
/// Scheduler has two modes:
/// - Balanced (K8s, multi-replica): DNS-resolve headless Service,
///   health-probe pod IPs, route to leader. Heartbeat routes through
///   the same balanced channel — leadership flip detected within one
///   probe tick (~3s).
/// - Single (non-K8s): plain connect. VM tests use this.
async fn connect_upstreams(cfg: &Config) -> (StoreClient, WorkerClient, BalanceGuard) {
    loop {
        let result: anyhow::Result<_> = async {
            let store = rio_proto::client::connect_store(&cfg.store_addr).await?;
            let (sched, guard) = match &cfg.scheduler_balance_host {
                None => {
                    info!(addr = %cfg.scheduler_addr, "scheduler: single-channel mode");
                    let c = rio_proto::client::connect_worker(&cfg.scheduler_addr).await?;
                    (c, None)
                }
                Some(host) => {
                    info!(
                        %host, port = cfg.scheduler_balance_port,
                        "scheduler: health-aware balanced mode"
                    );
                    let (c, bc) = rio_proto::client::balance::connect_worker_balanced(
                        host.clone(),
                        cfg.scheduler_balance_port,
                    )
                    .await?;
                    (c, Some(bc))
                }
            };
            Ok((store, sched, guard))
        }
        .await;
        match result {
            Ok(triple) => return triple,
            Err(e) => {
                warn!(error = %e, "upstream connect failed; retrying in 2s (liveness stays 200, readiness stays 503)");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
}

/// Inputs to the heartbeat loop. Grouped so main() doesn't grow 12
/// `let heartbeat_* = ...` prelude lines before the spawn.
struct HeartbeatCtx {
    worker_id: String,
    systems: Vec<String>,
    features: Vec<String>,
    max_builds: u32,
    size_class: String,
    running: Arc<RwLock<HashSet<String>>>,
    ready: Arc<std::sync::atomic::AtomicBool>,
    resources: rio_worker::cgroup::ResourceSnapshotHandle,
    bloom: Arc<std::sync::RwLock<rio_common::bloom::BloomFilter>>,
    circuit: Arc<rio_worker::fuse::circuit::CircuitBreaker>,
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
        worker_id,
        systems,
        features,
        max_builds,
        size_class,
        running,
        ready,
        resources,
        bloom,
        circuit,
        generation,
        mut client,
    } = ctx;
    rio_common::task::spawn_monitored("heartbeat-loop", async move {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        loop {
            interval.tick().await;

            let request = build_heartbeat_request(
                &worker_id,
                &systems,
                &features,
                max_builds,
                &size_class,
                &running,
                Some(&bloom),
                &resources,
                circuit.is_open(),
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
                        // our worker_id (stale registration, scheduler
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

    fn msg(id: &str) -> WorkerMessage {
        WorkerMessage {
            msg: Some(worker_message::Msg::Register(WorkerRegister {
                worker_id: id.into(),
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
            Some(worker_message::Msg::Register(WorkerRegister { worker_id })) if worker_id == "a"
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
            Some(worker_message::Msg::Register(WorkerRegister { worker_id })) if worker_id == "b"
        ));
        let r = tokio::time::timeout(Duration::from_secs(1), grpc2_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            r.msg,
            Some(worker_message::Msg::Register(WorkerRegister { worker_id })) if worker_id == "c"
        ));

        // Cleanup: drop sink → relay exits.
        drop(sink_tx);
        tokio::time::timeout(Duration::from_secs(1), relay)
            .await
            .unwrap()
            .unwrap();
    }

    /// The drain-wait synchronization: `acquire_many(max)` succeeds
    /// exactly when all in-flight build tasks have dropped their
    /// OwnedPermits.
    ///
    /// This test caught a real bug in an earlier drain implementation. The
    /// original drain sequence was:
    ///   1. `sem.close()`   (reject new acquires)
    ///   2. `sem.acquire_many(max)` (wait for in-flight)
    ///
    /// The bug: close() makes ANY waiting acquire return Err, even
    /// when permits DO become available. So step 2 returned Err
    /// immediately (the wait got cancelled), and the drain logged
    /// a spurious "stuck permit?" warning on EVERY sigterm that
    /// arrived while builds were running — which is the normal case.
    /// The builds still completed (OwnedPermit Drop isn't affected
    /// by close), we just couldn't observe it. Mostly cosmetic but
    /// the Err path exited without confirming completion.
    ///
    /// Fix: DON'T close. The loop already broke out — no new acquires
    /// can happen. close() was belt-and-suspenders that tripped itself.
    /// Without close, acquire_many blocks until permits return, then
    /// returns Ok. This test asserts the fixed behavior.
    ///
    /// Not testing SIGTERM-to-self: signal delivery under cargo test
    /// is nondeterministic, and nextest's per-process model means a
    /// stray SIGTERM kills the test binary. vm-phase3a does real
    /// SIGTERM via `k3s kubectl delete pod`.
    #[tokio::test]
    async fn drain_wait_semaphore_synchronization() {
        const MAX: u32 = 4;
        let sem = Arc::new(Semaphore::new(MAX as usize));

        // Acquire 3 permits as "in-flight builds." Hold them.
        let permit_a = Arc::clone(&sem).acquire_owned().await.unwrap();
        let permit_b = Arc::clone(&sem).acquire_owned().await.unwrap();
        let permit_c = Arc::clone(&sem).acquire_owned().await.unwrap();
        assert_eq!(sem.available_permits(), 1, "1 of 4 free");

        // Drain: acquire_many (NO close — that was the bug). Spawn so
        // we can drop permits from the test thread while it waits.
        //
        // acquire_many returns SemaphorePermit<'_> (borrows the sem)
        // which can't escape the task. Return just the discriminant.
        let drain_sem = Arc::clone(&sem);
        let drain = tokio::spawn(async move { drain_sem.acquire_many(MAX).await.is_ok() });

        // Give drain a tick to reach the wait point.
        tokio::task::yield_now().await;
        assert!(
            !drain.is_finished(),
            "acquire_many waiting (3 permits held)"
        );

        // Drop permits one at a time. Drain waits until ALL return.
        drop(permit_a);
        tokio::task::yield_now().await;
        assert!(!drain.is_finished(), "2 still held — not all MAX available");

        drop(permit_b);
        tokio::task::yield_now().await;
        assert!(!drain.is_finished(), "1 still held");

        drop(permit_c);
        // NOW all 4 permits are available → drain completes with Ok.
        let ok = tokio::time::timeout(Duration::from_secs(2), drain)
            .await
            .expect("drain completes once all permits return")
            .expect("task didn't panic");
        assert!(
            ok,
            "WITHOUT close(), acquire_many succeeds when permits return. \
             This is the fixed drain sequence: wait observes completion."
        );
    }

    /// Regression: `close()` + waiting `acquire_many` → Err. This is
    /// why main.rs does NOT call close() before the drain wait. Keep
    /// this test so if someone adds close() back (it looks safe!),
    /// this fails and they read the comment.
    #[tokio::test]
    async fn drain_wait_close_is_a_footgun() {
        let sem = Arc::new(Semaphore::new(2));
        let _held = Arc::clone(&sem).acquire_owned().await.unwrap();

        sem.close();
        // 1 permit held, 1 available. acquire_many(2) would wait.
        // On a closed sem, waiting acquire → Err immediately.
        let result = sem.acquire_many(2).await;
        assert!(
            result.is_err(),
            "close() cancels waiting acquires even when permits would return. \
             This is why main.rs skips close() before draining."
        );
    }
}
