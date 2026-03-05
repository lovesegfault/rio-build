//! rio-worker binary entry point.
//!
//! Wires up FUSE daemon, gRPC clients (WorkerService + StoreService),
//! executor, and heartbeat loop.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, Semaphore, mpsc};
use tracing::info;

use rio_proto::types::{WorkerMessage, WorkerRegister, scheduler_message, worker_message};
use rio_worker::{BuildSpawnContext, build_heartbeat_request, fuse, spawn_build_task};

// ---------------------------------------------------------------------------
// Configuration (two-struct split per rio-common/src/config.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    /// If empty after merge → auto-detect via hostname.
    worker_id: String,
    scheduler_addr: String,
    store_addr: String,
    max_builds: u32,
    /// If empty after merge → auto-detect via std::env::consts.
    system: String,
    fuse_mount_point: PathBuf,
    fuse_cache_dir: PathBuf,
    fuse_cache_size_gb: u64,
    fuse_threads: u32,
    /// Defaults to `true`. NOT the serde bool default — see `default_true`.
    /// A drift here (`false`) would silently disable kernel passthrough,
    /// adding a userspace copy per FUSE read and ~2× per-build latency.
    fuse_passthrough: bool,
    overlay_base_dir: PathBuf,
    metrics_addr: std::net::SocketAddr,
    /// HTTP /healthz + /readyz listen address. Worker has no gRPC
    /// server so tonic-health doesn't fit — plain HTTP via axum.
    /// K8s readinessProbe hits /readyz (200 after first accepted
    /// heartbeat), livenessProbe hits /healthz (always 200).
    health_addr: std::net::SocketAddr,
    /// Log limits (configuration.md:68-69). 0 = unlimited.
    /// Wired into LogLimits → LogBatcher in main().
    log_rate_limit: u64,
    log_size_limit: u64,
    /// Size-class this worker is deployed as. Empty = unclassified.
    /// If the scheduler has size_classes configured, unclassified
    /// workers are REJECTED (misconfiguration — set this). Operator
    /// sets it to match the scheduler's size_classes config — e.g.
    /// "small" workers on cheap spot instances, "large" on
    /// memory-optimized. The scheduler routes by estimated duration;
    /// this just declares which bucket this worker serves.
    size_class: String,
    /// Threshold for leaked overlay mounts before refusing new builds.
    /// After N umount2 failures (stuck-busy mounts), the worker is
    /// degraded; execute_build short-circuits with InfrastructureFailure
    /// so the scheduler reassigns and the supervisor can restart.
    max_leaked_mounts: usize,
    /// Timeout (seconds) for the local nix-daemon subprocess build when
    /// the client didn't specify BuildOptions.build_timeout. Intentionally
    /// long (2h default) — some builds genuinely take that long; this is
    /// a bound on blast radius of a truly stuck daemon, not an expected
    /// build time.
    daemon_timeout_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            worker_id: String::new(),
            scheduler_addr: String::new(),
            store_addr: String::new(),
            max_builds: 1,
            system: String::new(),
            // Matches nix/modules/worker.nix. NEVER default to /nix/store:
            // mounting FUSE there shadows the host store, breaking every
            // process on the machine (including the worker itself).
            fuse_mount_point: "/var/rio/fuse-store".into(),
            fuse_cache_dir: "/var/rio/cache".into(),
            fuse_cache_size_gb: 50,
            fuse_threads: 4,
            fuse_passthrough: true,
            overlay_base_dir: "/var/rio/overlays".into(),
            metrics_addr: "0.0.0.0:9093".parse().unwrap(),
            // 9193 = metrics (9093) + 100. Same +100 pattern as
            // gateway (9090→9190). Scheduler/store piggyback health
            // on their gRPC ports; worker+gateway have no gRPC server.
            health_addr: "0.0.0.0:9193".parse().unwrap(),
            // configuration.md:68-69 specs these defaults.
            log_rate_limit: 10_000,
            log_size_limit: 100 * 1024 * 1024, // 100 MiB
            size_class: String::new(),
            max_leaked_mounts: 3,
            daemon_timeout_secs: rio_worker::executor::DEFAULT_DAEMON_TIMEOUT.as_secs(),
        }
    }
}

#[derive(Parser, Serialize, Default)]
#[command(
    name = "rio-worker",
    about = "Build executor with FUSE store for rio-build"
)]
struct CliArgs {
    /// Worker ID (defaults to hostname)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    worker_id: Option<String>,

    /// rio-scheduler gRPC address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    scheduler_addr: Option<String>,

    /// rio-store gRPC address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    store_addr: Option<String>,

    /// Maximum concurrent builds
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    max_builds: Option<u32>,

    /// System architecture (auto-detected if not set)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,

    /// FUSE mount point
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    fuse_mount_point: Option<PathBuf>,

    /// FUSE cache directory
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    fuse_cache_dir: Option<PathBuf>,

    /// FUSE cache size limit in GB
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    fuse_cache_size_gb: Option<u64>,

    /// Number of FUSE threads
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    fuse_threads: Option<u32>,

    /// Enable FUSE passthrough mode. Use --fuse-passthrough=false to disable.
    //
    // clap's `bool` is a flag (presence=true, absence=false), which would
    // make it impossible to NOT set from CLI (defeating layering).
    // `Option<bool>` with an explicit value parser makes clap accept
    // `--fuse-passthrough=true|false` and leaves it None when absent.
    #[arg(long, value_parser = clap::value_parser!(bool))]
    #[serde(skip_serializing_if = "Option::is_none")]
    fuse_passthrough: Option<bool>,

    /// Overlay base directory
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    overlay_base_dir: Option<PathBuf>,

    /// Prometheus metrics listen address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics_addr: Option<std::net::SocketAddr>,

    /// Max log lines/sec per build (0 = unlimited)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    log_rate_limit: Option<u64>,

    /// Max total log bytes per build (0 = unlimited)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    log_size_limit: Option<u64>,

    /// Size-class (matches scheduler config; e.g. "small", "large")
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    size_class: Option<String>,

    /// Max leaked overlay mounts before refusing builds (default: 3)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    max_leaked_mounts: Option<usize>,

    /// Daemon build timeout seconds (default: 7200)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon_timeout_secs: Option<u64>,
}

/// Heartbeat interval. Shared source of truth with the scheduler's timeout
/// check (rio_common::limits::HEARTBEAT_TIMEOUT_SECS derives from this).
const HEARTBEAT_INTERVAL: Duration =
    Duration::from_secs(rio_common::limits::HEARTBEAT_INTERVAL_SECS);

/// Detect the system architecture (e.g. "x86_64-linux").
fn detect_system() -> String {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    // Map Rust arch names to Nix system names
    let nix_arch = match arch {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        "x86" => "i686",
        other => other,
    };
    format!("{nix_arch}-{os}")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = CliArgs::parse();
    let cfg: Config = rio_common::config::load("worker", cli)?;
    let _otel_guard = rio_common::observability::init_tracing("worker")?;

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

    let system = if cfg.system.is_empty() {
        detect_system()
    } else {
        cfg.system
    };

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
        %system,
        "connected to gRPC services"
    );

    // Set up FUSE cache and mount. Arc so we can clone handles
    // BEFORE moving into mount_fuse_background: bloom_handle for
    // heartbeat, and the Arc itself for B2's prefetch handler.
    // Same extract-before-move pattern as bloom_handle always used.
    let cache =
        Arc::new(fuse::cache::Cache::new(cfg.fuse_cache_dir, cfg.fuse_cache_size_gb).await?);
    // Extract the bloom handle. The heartbeat loop reads from the
    // same RwLock that cache.insert() writes to — inserts by FUSE
    // ops show up in subsequent heartbeat snapshots.
    let heartbeat_bloom = cache.bloom_handle();
    // Clone for prefetch (B2). Cache methods use runtime.block_on
    // internally (sync, designed for FUSE callbacks on dedicated
    // threads). The prefetch handler will call them via
    // spawn_blocking — async → nested-runtime panic.
    let prefetch_cache = Arc::clone(&cache);
    let prefetch_store = store_client.clone();
    let runtime = tokio::runtime::Handle::current();
    let prefetch_runtime = runtime.clone();

    std::fs::create_dir_all(&cfg.fuse_mount_point)?;
    std::fs::create_dir_all(&cfg.overlay_base_dir)?;

    let _fuse_session = fuse::mount_fuse_background(
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
    let heartbeat_system = system.clone();
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
                &heartbeat_system,
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
    };

    // Process incoming scheduler messages + SIGTERM for graceful drain.
    //
    // select! is biased toward sigterm: poll it FIRST each iteration.
    // Without `biased;`, select! picks a ready branch pseudorandomly —
    // under heavy assignment traffic, SIGTERM could starve behind
    // stream messages. K8s sends SIGTERM then starts the grace period
    // clock; we want to react immediately, not after the next gap in
    // assignments.
    //
    // The loop body is identical to the old while-let; SIGTERM just
    // breaks out so the drain sequence below runs.
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
                        tracing::warn!(
                            drv_path = %cancel.drv_path,
                            reason = %cancel.reason,
                            "received cancel signal (cancel not yet implemented)"
                        );
                    }
                    Some(scheduler_message::Msg::Prefetch(prefetch)) => {
                        tracing::debug!(
                            paths = prefetch.store_paths.len(),
                            "received prefetch hint"
                        );
                        // Spawn one task per path. Don't await — the
                        // whole point is to NOT block the stream loop
                        // on prefetch. The semaphore bounds concurrent
                        // in-flight; excess queue in tokio's task
                        // scheduler (cheap — no blocking-pool thread
                        // is held until the permit is acquired).
                        //
                        // No JoinHandle tracking: prefetch is fire-and-
                        // forget. If the worker SIGTERMs mid-prefetch,
                        // the tasks abort with the runtime — the
                        // partial fetch is in a .tmp-XXXX sibling dir
                        // (see fetch_extract_insert) which cache init
                        // cleans up on next start.
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
                                metrics::counter!("rio_worker_prefetch_total", "result" => "malformed")
                                    .increment(1);
                                continue;
                            };
                            let basename = basename.to_string();

                            // Clone handles into the task. All cheap:
                            // Arc clone, tonic Channel is Arc-internal,
                            // tokio Handle is a lightweight token.
                            let cache = Arc::clone(&prefetch_cache);
                            let client = prefetch_store.clone();
                            let rt = prefetch_runtime.clone();
                            let sem = Arc::clone(&prefetch_sem);

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
                                    use rio_worker::fuse::fetch::{PrefetchSkip, prefetch_path_blocking};
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
                                metrics::counter!("rio_worker_prefetch_total", "result" => label)
                                    .increment(1);
                            });
                        }
                    }
                    None => {
                        tracing::warn!("received empty scheduler message");
                    }
                }
            }
        }
    };

    // ---- Drain sequence -------------------------------------------------
    //
    // K8s preStop (controller.md:211-216):
    //   1. DrainWorker: scheduler stops sending assignments
    //   2. Wait for in-flight builds
    //   3. (Outputs already uploaded by each build task — no step here)
    //   4. Exit 0
    //
    // terminationGracePeriodSeconds=7200 (2h). If we exceed that,
    // SIGKILL — builds lost. 2h is enough for ~any single build; the
    // ones that take longer (LLVM+ccache-cold) are rare.
    //
    // Only DrainReason::Sigterm does the full sequence. Stream close/
    // error means the scheduler is gone — DrainWorker would fail, and
    // WorkerDisconnected already reassigned our builds. Just exit.
    match drain_reason {
        DrainReason::Sigterm => {
            info!(
                in_flight = cfg.max_builds as usize - build_semaphore.available_permits(),
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
            match rio_proto::client::connect_admin(&cfg.scheduler_addr).await {
                Ok(mut admin) => {
                    match admin
                        .drain_worker(rio_proto::types::DrainWorkerRequest {
                            worker_id: build_ctx.worker_id.clone(),
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
            // close() BEFORE acquire_many: close doesn't affect already-
            // issued permits (they return on drop as usual), it just
            // rejects new acquires. With the loop broken, no new
            // acquires happen anyway — close() is belt-and-suspenders.
            // close() BEFORE the wait has a subtle interaction (see
            // drain_wait_semaphore_synchronization test): a CLOSED
            // semaphore returns Err to waiters even when permits DO
            // return. So if any build is still running when we hit
            // acquire_many, we get Err — but the builds still complete
            // (their OwnedPermit Drop works fine; close doesn't affect
            // that). We just can't OBSERVE completion via acquire.
            //
            // Fix: DON'T close() before waiting. close() was meant to
            // reject new acquires, but we already broke out of the
            // loop — no new acquires can happen. Skip close() entirely
            // and acquire_many works as the synchronization primitive
            // it's meant to be.
            match build_semaphore.acquire_many(cfg.max_builds).await {
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

            // _fuse_session Drop handles unmount. build_stream Drop
            // closes the gRPC stream → scheduler WorkerDisconnected
            // → removes our entry (if DrainWorker didn't already).
            info!("drain complete, exiting");
        }
        DrainReason::StreamClosed => {
            info!("build execution stream closed, shutting down");
        }
        DrainReason::StreamError => {
            info!("build execution stream error, shutting down");
        }
    }

    Ok(())
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

    /// Regression guard against silent default drift. CRITICAL case:
    /// `fuse_passthrough` defaults to `true` — NOT the serde bool default.
    /// A drift to `false` adds a userspace copy per FUSE read (~2× per-build
    /// latency) and would only show up as a vm-phase2a timing regression,
    /// not a hard failure.
    #[test]
    fn config_defaults_are_stable() {
        let d = Config::default();
        assert!(
            d.worker_id.is_empty(),
            "worker_id auto-detects via hostname"
        );
        assert!(d.scheduler_addr.is_empty(), "required, no default");
        assert!(d.store_addr.is_empty(), "required, no default");
        assert_eq!(d.max_builds, 1);
        assert!(d.system.is_empty(), "system auto-detects");
        assert_eq!(d.fuse_mount_point, PathBuf::from("/var/rio/fuse-store"));
        assert_eq!(d.fuse_cache_dir, PathBuf::from("/var/rio/cache"));
        assert_eq!(d.fuse_cache_size_gb, 50);
        assert_eq!(d.fuse_threads, 4);
        assert!(
            d.fuse_passthrough,
            "fuse_passthrough MUST default to true (phase2a behavior); \
             serde's bool default is false so this needs explicit handling"
        );
        assert_eq!(d.overlay_base_dir, PathBuf::from("/var/rio/overlays"));
        assert_eq!(d.metrics_addr.to_string(), "0.0.0.0:9093");
        assert_eq!(d.health_addr.to_string(), "0.0.0.0:9193");
        // Phase2b additions — spec values from configuration.md:68-69.
        assert_eq!(d.log_rate_limit, 10_000);
        assert_eq!(d.log_size_limit, 100 * 1024 * 1024);
        assert_eq!(d.max_leaked_mounts, 3);
    }

    #[test]
    fn cli_args_parse_help() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert();
    }

    /// `--fuse-passthrough` must accept explicit true/false (not a flag).
    #[test]
    fn cli_fuse_passthrough_explicit_bool() {
        let args = CliArgs::try_parse_from(["rio-worker", "--fuse-passthrough", "false"]).unwrap();
        assert_eq!(args.fuse_passthrough, Some(false));
        let args = CliArgs::try_parse_from(["rio-worker", "--fuse-passthrough", "true"]).unwrap();
        assert_eq!(args.fuse_passthrough, Some(true));
        // Absent → None (layering: don't overlay).
        let args = CliArgs::try_parse_from(["rio-worker"]).unwrap();
        assert_eq!(args.fuse_passthrough, None);
    }

    /// The drain-wait synchronization: `acquire_many(max)` succeeds
    /// exactly when all in-flight build tasks have dropped their
    /// OwnedPermits.
    ///
    /// This test CAUGHT A BUG in the initial D3 implementation. The
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
             This is why main.rs skips close() — it was the D3 bug."
        );
    }
}
