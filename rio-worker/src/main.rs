//! rio-worker binary entry point.
//!
//! Wires up FUSE daemon, gRPC clients (WorkerService + StoreService),
//! executor, and heartbeat loop.

mod config;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::sync::{RwLock, Semaphore, mpsc};
use tracing::info;

use rio_proto::types::{WorkerMessage, WorkerRegister, scheduler_message, worker_message};
use rio_worker::runtime::handle_prefetch_hint;
use rio_worker::{BuildSpawnContext, build_heartbeat_request, fuse, spawn_build_task};

use config::{CliArgs, Config, detect_system};

/// Heartbeat interval. Shared source of truth with the scheduler's timeout
/// check (rio_common::limits::HEARTBEAT_TIMEOUT_SECS derives from this).
const HEARTBEAT_INTERVAL: Duration =
    Duration::from_secs(rio_common::limits::HEARTBEAT_INTERVAL_SECS);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // rustls CryptoProvider install BEFORE any TLS use. Phase 3b
    // enables tonic tls-aws-lc; without this, rustls panics on
    // first handshake (aws-lc-rs feature active but no provider
    // installed means auto-select fails).
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli = CliArgs::parse();
    let cfg: Config = rio_common::config::load("worker", cli)?;
    let _otel_guard = rio_common::observability::init_tracing("worker")?;

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

    anyhow::ensure!(
        !cfg.scheduler_addr.is_empty(),
        "scheduler_addr is required (set --scheduler-addr, RIO_SCHEDULER_ADDR, or worker.toml)"
    );
    anyhow::ensure!(
        !cfg.store_addr.is_empty(),
        "store_addr is required (set --store-addr, RIO_STORE_ADDR, or worker.toml)"
    );

    // worker_id uniquely identifies this worker to the scheduler. Two workers
    // with the same ID would steal each other's builds via heartbeat merging.
    // Fail hard rather than silently colliding on "unknown".
    let worker_id = if cfg.worker_id.is_empty() {
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
        cfg.worker_id
    };

    // systems: auto-detect single element when not configured.
    // A worker with zero systems is useless (scheduler's can_build
    // always false) — auto-detect is a sensible default, not a
    // silent fallback for misconfiguration.
    let systems = if cfg.systems.is_empty() {
        vec![detect_system()]
    } else {
        cfg.systems
    };
    // features: no auto-detect. Empty is valid (worker supports no
    // special features). Operator sets these explicitly in the CRD
    // — auto-detecting "kvm" by checking /dev/kvm exists would be
    // surprising (worker on a kvm-capable host but operator wants
    // to reserve it for other work).
    let features = cfg.features;

    let _root_guard =
        tracing::info_span!("worker", component = "worker", worker_id = %worker_id).entered();
    info!(version = env!("CARGO_PKG_VERSION"), "starting rio-worker");

    rio_common::observability::init_metrics(cfg.metrics_addr)?;
    rio_worker::describe_metrics();

    // cgroup v2 setup. HARD REQUIREMENT — `?` on both. Fail startup
    // loudly rather than silently fall back to broken metrics (the
    // phase2c VmHWM bug measured ~10MB for every build; poisoning
    // build_history like that takes ~10 EMA cycles to wash out).
    //
    // delegated_root() returns the PARENT of /proc/self/cgroup —
    // NOT own_cgroup(). cgroup v2's no-internal-processes rule means
    // per-build cgroups must be SIBLINGS of where the worker process
    // is, not children. systemd DelegateSubgroup=builds puts the
    // worker in .../service/builds/; delegated_root() returns
    // .../service/ (empty, writable via Delegate=yes); per-build
    // cgroups go there as siblings of builds/.
    //
    // enable_subtree_controllers writes +memory +cpu (fails on EACCES
    // = Delegate=yes not configured).
    //
    // BEFORE the health server: if cgroup fails, we don't want
    // liveness passing while startup is hung on `?` propagation.
    // Pod goes straight to CrashLoopBackOff with a clear log line.
    let cgroup_parent = rio_worker::cgroup::delegated_root()
        .map_err(|e| anyhow::anyhow!("cgroup v2 required: {e}"))?;
    rio_worker::cgroup::enable_subtree_controllers(&cgroup_parent)
        .map_err(|e| anyhow::anyhow!("cgroup delegation required: {e}"))?;
    info!(cgroup = %cgroup_parent.display(), "cgroup v2 subtree ready");

    // Readiness flag + HTTP health server. Spawned BEFORE gRPC connect
    // so liveness passes as soon as the process is up (connect may take
    // seconds if scheduler DNS is slow to resolve). Readiness stays
    // false until the first heartbeat comes back accepted — that's the
    // right gate: a worker that can't heartbeat is not useful capacity.
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    rio_worker::health::spawn_health_server(cfg.health_addr, ready.clone());

    // Connect to gRPC services
    let store_client = rio_proto::client::connect_store(&cfg.store_addr).await?;
    let mut scheduler_client = rio_proto::client::connect_worker(&cfg.scheduler_addr).await?;

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
    let cache =
        Arc::new(fuse::cache::Cache::new(cfg.fuse_cache_dir, cfg.fuse_cache_size_gb).await?);
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

    std::fs::create_dir_all(&cfg.fuse_mount_point)?;
    std::fs::create_dir_all(&cfg.overlay_base_dir)?;

    let fuse_session = fuse::mount_fuse_background(
        &cfg.fuse_mount_point,
        cache,
        store_client.clone(),
        runtime,
        cfg.fuse_passthrough,
        cfg.fuse_threads,
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

    // Set up build execution stream (bidirectional)
    let (stream_tx, stream_rx) = mpsc::channel::<WorkerMessage>(256);

    // Send WorkerRegister as the first message before opening the stream.
    // The scheduler reads this to associate the stream with our worker_id,
    // ensuring stream and heartbeat share the same identity.
    stream_tx
        .send(WorkerMessage {
            msg: Some(worker_message::Msg::Register(WorkerRegister {
                worker_id: worker_id.clone(),
            })),
        })
        .await?;

    let outbound = tokio_stream::wrappers::ReceiverStream::new(stream_rx);

    let build_stream = scheduler_client
        .build_execution(outbound)
        .await?
        .into_inner();

    // Concurrent build semaphore
    let build_semaphore = Arc::new(Semaphore::new(cfg.max_builds as usize));

    // Track running builds (drv_path set) for heartbeat reporting
    let running_builds: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));

    // Spawn heartbeat loop. A panicking heartbeat loop leaves the worker
    // silently alive but unreachable from the scheduler's perspective — the
    // scheduler times it out and re-dispatches its builds to another worker,
    // leading to duplicate builds. Wrap in spawn_monitored so panics are logged,
    // and check liveness in the main event loop.
    let heartbeat_worker_id = worker_id.clone();
    // move (not clone): these are owned Vecs, used only by the
    // heartbeat loop. main() has no further use for them.
    let heartbeat_systems = systems;
    let heartbeat_features = features;
    let heartbeat_max_builds = cfg.max_builds;
    let heartbeat_size_class = cfg.size_class.clone();
    let heartbeat_running = running_builds.clone();
    let heartbeat_ready = ready.clone();
    let mut heartbeat_client = scheduler_client.clone();
    let heartbeat_handle = rio_common::task::spawn_monitored("heartbeat-loop", async move {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        loop {
            interval.tick().await;

            let request = build_heartbeat_request(
                &heartbeat_worker_id,
                &heartbeat_systems,
                &heartbeat_features,
                heartbeat_max_builds,
                &heartbeat_size_class,
                &heartbeat_running,
                Some(&heartbeat_bloom),
            )
            .await;

            match heartbeat_client.heartbeat(request).await {
                Ok(response) => {
                    let resp = response.into_inner();
                    if resp.accepted {
                        // READY. Set unconditionally — it's idempotent
                        // (already-true → true is a no-op at the atomic
                        // level) and cheaper than a load-then-store.
                        heartbeat_ready.store(true, std::sync::atomic::Ordering::Relaxed);
                    } else {
                        tracing::warn!("heartbeat rejected by scheduler");
                        // NOT READY: scheduler is reachable but rejecting
                        // us. Could mean the scheduler doesn't recognize
                        // our worker_id (stale registration, scheduler
                        // restarted and lost in-memory state). The
                        // BuildExecution stream reconnect logic handles
                        // the actual recovery; readiness flag just
                        // reflects the current state.
                        heartbeat_ready.store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "heartbeat failed");
                    // NOT READY: gRPC error. Scheduler unreachable or
                    // overloaded. Don't flip liveness (restarting won't
                    // fix the network) but do flip readiness. The next
                    // successful heartbeat flips back — this tracks
                    // the scheduler's availability from our perspective.
                    heartbeat_ready.store(false, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
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
        stream_tx,
        running_builds,
        leaked_mounts: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        log_limits: rio_worker::log_stream::LogLimits {
            rate_lines_per_sec: cfg.log_rate_limit,
            total_bytes: cfg.log_size_limit,
        },
        max_leaked_mounts: cfg.max_leaked_mounts,
        daemon_timeout: Duration::from_secs(cfg.daemon_timeout_secs),
        cgroup_parent,
        cancel_registry: cancel_registry.clone(),
        fod_proxy_url: cfg.fod_proxy_url,
    };

    // Process incoming scheduler messages + SIGTERM for graceful drain.
    //
    // select! is biased toward sigterm: poll it FIRST each iteration.
    // Without `biased;`, select! picks a ready branch pseudorandomly —
    // under heavy assignment traffic, SIGTERM could starve behind
    // stream messages. K8s sends SIGTERM then starts the grace period
    // clock; we want to react immediately, not after the next gap in
    // assignments.
    let mut build_stream = build_stream;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let drain_reason = loop {
        // A worker without a live heartbeat is a liability (scheduler will
        // time it out and re-dispatch its builds). Die fast rather than
        // silently duplicate work. Check BEFORE awaiting select — if
        // heartbeat died mid-iteration, don't process another message.
        if heartbeat_handle.is_finished() {
            tracing::error!("heartbeat loop terminated unexpectedly; exiting");
            std::process::exit(1);
        }

        tokio::select! {
            biased;

            _ = sigterm.recv() => {
                break DrainReason::Sigterm;
            }

            msg_result = tokio_stream::StreamExt::next(&mut build_stream) => {
                let Some(msg_result) = msg_result else {
                    // Stream closed by server (scheduler shutdown/restart).
                    // Not a SIGTERM drain — just exit. The scheduler
                    // will reassign via WorkerDisconnected.
                    break DrainReason::StreamClosed;
                };
                let msg = match msg_result {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::error!(error = %e, "build execution stream error");
                        break DrainReason::StreamError;
                    }
                };

                match msg.msg {
                    Some(scheduler_message::Msg::Assignment(assignment)) => {
                        info!(drv_path = %assignment.drv_path, "received work assignment");

                        // Acquire permit BEFORE ACKing: don't tell the
                        // scheduler we accepted work we can't immediately
                        // start. On Err(Closed): semaphore.close() was
                        // called — impossible here (close happens in the
                        // drain path below, AFTER loop exit), so this is
                        // a bug. Break with a distinct reason.
                        let permit = match build_semaphore.clone().acquire_owned().await {
                            Ok(p) => p,
                            Err(_) => {
                                tracing::error!("semaphore closed mid-loop (bug)");
                                break DrainReason::StreamError;
                            }
                        };

                        spawn_build_task(assignment, permit, &build_ctx).await;
                    }
                    Some(scheduler_message::Msg::Cancel(cancel)) => {
                        info!(
                            drv_path = %cancel.drv_path,
                            reason = %cancel.reason,
                            "received cancel signal"
                        );
                        // Look up cgroup → write cgroup.kill → daemon
                        // + builder + children SIGKILLed → run_daemon_
                        // build sees stdout EOF → Err → execute_build
                        // returns Err → spawn_build_task checks the
                        // cancelled flag → reports Cancelled (not
                        // InfrastructureFailure). The semaphore permit
                        // drops when the spawned task exits (scopeguard).
                        //
                        // Fire-and-forget: scheduler doesn't wait for
                        // confirmation. If the build already finished
                        // (race), try_cancel_build returns false and
                        // we log at debug.
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
                        );
                    }
                    None => {
                        tracing::warn!("received empty scheduler message");
                    }
                }
            }
        }
    };

    run_drain(
        drain_reason,
        &build_semaphore,
        cfg.max_builds,
        &cfg.scheduler_addr,
        &build_ctx.worker_id,
    )
    .await;

    // Explicit unmount+join: BackgroundSession has NO Drop impl, so
    // dropping it detaches the FUSE thread. Mount's Drop DOES umount
    // (kernel sends DESTROY to the session), but the main thread
    // races to exit() before the detached FUSE thread can process
    // DESTROY → Filesystem::destroy() never runs → passthrough-
    // failure stats lost, profraw never flushed for that code.
    // umount_and_join() unmounts THEN joins the thread, ensuring
    // destroy() completes before main returns.
    //
    // Detached std::thread + channel-with-timeout: if the FUSE mount
    // is busy (overlay stacked on top, or some process holds an open
    // fd to it), umount_and_join() blocks forever in the FUSE thread
    // join. Using tokio::spawn_blocking here would wedge the tokio
    // Runtime::drop (it waits for blocking tasks) — we'd time out the
    // future but the process would never exit. A std thread is
    // independent of tokio's lifecycle: when main returns, the
    // runtime drops cleanly, and the detached thread is killed by
    // libc at process exit.
    //
    // 5s wait: generous for normal umount (<100ms) but bounded so
    // k8s pod termination doesn't wedge. If we abandon, kernel
    // unmounts on process death anyway — we just lose destroy().
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(fuse_session.umount_and_join());
    });
    // recv_timeout blocks the current async task — fine here, this
    // is the last thing main() does and nothing else is scheduled.
    match done_rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(())) => tracing::debug!("FUSE session unmounted and joined"),
        Ok(Err(e)) => tracing::warn!(error = %e, "FUSE umount/join failed"),
        Err(_) => tracing::warn!(
            "FUSE umount/join timed out (5s); abandoning graceful unmount. \
             Mount may be busy — kernel will unmount on process exit."
        ),
    }

    Ok(())
}

/// Execute the drain sequence after the event loop exits.
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
/// Only DrainReason::Sigterm does the full sequence. Stream close/
/// error means the scheduler is gone — DrainWorker would fail, and
/// WorkerDisconnected already reassigned our builds. Just exit.
async fn run_drain(
    reason: DrainReason,
    semaphore: &Semaphore,
    max_builds: u32,
    scheduler_addr: &str,
    worker_id: &str,
) {
    match reason {
        DrainReason::Sigterm => {
            info!(
                in_flight = max_builds as usize - semaphore.available_permits(),
                "SIGTERM received, draining"
            );

            // Step 1: DrainWorker. Best-effort — if it fails (scheduler
            // unreachable, actor dead), log and continue. The scheduler
            // will eventually time us out via heartbeat (we're still
            // heartbeating until exit) and WorkerDisconnected will
            // reassign. We lose nothing by trying.
            //
            // Fresh admin client: could clone the scheduler_client's
            // channel, but that's internal to WorkerServiceClient.
            // Simpler to just connect — the address is in cfg, and
            // connect is cheap (happy path: one TCP handshake, ~1ms
            // on localhost, ~RTT over network). This is a one-shot
            // call at shutdown, not a hot path.
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
                            // accepted=false means the scheduler already
                            // removed us (WorkerDisconnected raced). Fine —
                            // our builds are reassigned, but our local
                            // nix-daemons are still running. We still
                            // wait for them below (they'll complete,
                            // upload outputs, and CompletionReport will
                            // be dropped by the scheduler as "unknown
                            // drv" — wasted but correct).
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

            // Step 2: wait for in-flight. close() makes future
            // acquire_owned() return Err(Closed) — if the scheduler
            // DOES send more assignments (DrainWorker raced), the
            // Assignment arm's acquire fails and... well, we already
            // broke out of the loop, so the stream is undriven.
            // Messages queue in the gRPC buffer and are dropped on
            // drop(build_stream) below. The scheduler sees stream
            // close → WorkerDisconnected → reassigns those. Correct.
            //
            // acquire_many(max_builds) succeeds when ALL permits are
            // returned — every build task dropped its OwnedPermit on
            // completion. This is the synchronization point: when this
            // returns, no build is mid-upload.
            //
            // DON'T close() before acquire_many: close makes waiting
            // acquires return Err even when permits return. The loop
            // already broke — no new acquires can happen.
            match semaphore.acquire_many(max_builds).await {
                Ok(_all_permits) => {
                    info!("all in-flight builds complete");
                }
                Err(_) => {
                    // Err on a non-closed semaphore is impossible
                    // (acquire only errs on close). If we hit this,
                    // someone else closed it — a bug. Exit anyway:
                    // hanging here is worse than abandoning a build
                    // that may-or-may-not be stuck.
                    tracing::error!("semaphore closed unexpectedly (bug); exiting anyway");
                }
            }

            // FUSE unmount+join after return (see end of main).
            // build_stream Drop closes gRPC → scheduler
            // WorkerDisconnected → removes our entry (if DrainWorker
            // didn't already).
            info!("drain complete, exiting");
        }
        DrainReason::StreamClosed => {
            info!("build execution stream closed, shutting down");
        }
        DrainReason::StreamError => {
            info!("build execution stream error, shutting down");
        }
    }
}

/// Why the event loop exited. Determines whether to run the full
/// drain sequence (SIGTERM) or just exit (scheduler gone).
enum DrainReason {
    /// K8s preStop → full drain: DrainWorker + wait for in-flight.
    Sigterm,
    /// Server closed stream (scheduler restart). Scheduler-side
    /// WorkerDisconnected already reassigned; just exit.
    StreamClosed,
    /// gRPC error on stream. Same treatment as StreamClosed.
    StreamError,
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let permit_a = sem.clone().acquire_owned().await.unwrap();
        let permit_b = sem.clone().acquire_owned().await.unwrap();
        let permit_c = sem.clone().acquire_owned().await.unwrap();
        assert_eq!(sem.available_permits(), 1, "1 of 4 free");

        // Drain: acquire_many (NO close — that was the bug). Spawn so
        // we can drop permits from the test thread while it waits.
        //
        // acquire_many returns SemaphorePermit<'_> (borrows the sem)
        // which can't escape the task. Return just the discriminant.
        let drain_sem = sem.clone();
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
        let _held = sem.clone().acquire_owned().await.unwrap();

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
