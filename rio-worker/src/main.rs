//! rio-worker binary entry point.
//!
//! Wires up FUSE daemon, gRPC clients (WorkerService + StoreService),
//! executor, and heartbeat loop.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::sync::{RwLock, Semaphore, mpsc};
use tracing::info;

use rio_proto::types::{WorkerMessage, WorkerRegister, scheduler_message, worker_message};
use rio_worker::{BuildSpawnContext, build_heartbeat_request, fuse, spawn_build_task};

#[derive(Parser, Debug)]
#[command(
    name = "rio-worker",
    about = "Build executor with FUSE store for rio-build"
)]
struct Args {
    /// Worker ID (defaults to hostname)
    #[arg(long, env = "RIO_WORKER_ID")]
    worker_id: Option<String>,

    /// rio-scheduler gRPC address
    #[arg(long, env = "RIO_SCHEDULER_ADDR")]
    scheduler_addr: String,

    /// rio-store gRPC address
    #[arg(long, env = "RIO_STORE_ADDR")]
    store_addr: String,

    /// Maximum concurrent builds
    #[arg(long, env = "RIO_WORKER_MAX_BUILDS", default_value = "1")]
    max_builds: u32,

    /// System architecture (auto-detected if not set)
    #[arg(long, env = "RIO_WORKER_SYSTEM")]
    system: Option<String>,

    /// FUSE mount point
    #[arg(long, env = "RIO_FUSE_MOUNT_POINT", default_value = "/nix/store")]
    fuse_mount_point: PathBuf,

    /// FUSE cache directory
    #[arg(long, env = "RIO_FUSE_CACHE_DIR", default_value = "/var/rio/cache")]
    fuse_cache_dir: PathBuf,

    /// FUSE cache size limit in GB
    #[arg(long, env = "RIO_FUSE_CACHE_SIZE_GB", default_value = "50")]
    fuse_cache_size_gb: u64,

    /// Number of FUSE threads
    #[arg(long, env = "RIO_FUSE_THREADS", default_value = "4")]
    fuse_threads: u32,

    /// Enable FUSE passthrough mode
    #[arg(long, env = "RIO_FUSE_PASSTHROUGH", default_value = "true")]
    fuse_passthrough: bool,

    /// Overlay base directory
    #[arg(
        long,
        env = "RIO_OVERLAY_BASE_DIR",
        default_value = "/var/rio/overlays"
    )]
    overlay_base_dir: PathBuf,

    /// Prometheus metrics listen address
    #[arg(long, env = "RIO_METRICS_ADDR", default_value = "0.0.0.0:9093")]
    metrics_addr: std::net::SocketAddr,
}

/// Heartbeat interval.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

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
    let args = Args::parse();
    rio_common::observability::init_from_env()?;

    // worker_id uniquely identifies this worker to the scheduler. Two workers
    // with the same ID would steal each other's builds via heartbeat merging.
    // Fail hard rather than silently colliding on "unknown".
    let worker_id = args.worker_id.unwrap_or_else(|| {
        nix::unistd::gethostname()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| {
                tracing::error!(
                    "cannot determine worker_id: gethostname() failed and --worker-id not provided"
                );
                std::process::exit(1);
            })
    });

    let system = args.system.unwrap_or_else(detect_system);

    let _root_guard =
        tracing::info_span!("worker", component = "worker", worker_id = %worker_id).entered();
    info!(version = env!("CARGO_PKG_VERSION"), "starting rio-worker");

    rio_common::observability::init_metrics(args.metrics_addr)?;

    // Connect to gRPC services
    let store_client = rio_proto::client::connect_store(&args.store_addr).await?;
    let mut scheduler_client = rio_proto::client::connect_worker(&args.scheduler_addr).await?;

    info!(
        %worker_id,
        scheduler_addr = %args.scheduler_addr,
        store_addr = %args.store_addr,
        max_builds = args.max_builds,
        %system,
        "connected to gRPC services"
    );

    // Set up FUSE cache and mount
    let cache = fuse::cache::Cache::new(args.fuse_cache_dir, args.fuse_cache_size_gb).await?;
    let runtime = tokio::runtime::Handle::current();

    std::fs::create_dir_all(&args.fuse_mount_point)?;
    std::fs::create_dir_all(&args.overlay_base_dir)?;

    let _fuse_session = fuse::mount_fuse_background(
        &args.fuse_mount_point,
        cache,
        store_client.clone(),
        runtime,
        args.fuse_passthrough,
        args.fuse_threads,
    )?;

    info!(
        mount_point = %args.fuse_mount_point.display(),
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
    let build_semaphore = Arc::new(Semaphore::new(args.max_builds as usize));

    // Track running builds (drv_path set) for heartbeat reporting
    let running_builds: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));

    // Spawn heartbeat loop. A panicking heartbeat loop leaves the worker
    // silently alive but unreachable from the scheduler's perspective — the
    // scheduler times it out and re-dispatches its builds to another worker,
    // leading to duplicate builds. Wrap in spawn_monitored so panics are logged,
    // and check liveness in the main event loop.
    let heartbeat_worker_id = worker_id.clone();
    let heartbeat_system = system.clone();
    let heartbeat_max_builds = args.max_builds;
    let heartbeat_running = running_builds.clone();
    let mut heartbeat_client = scheduler_client.clone();
    let heartbeat_handle = rio_common::task::spawn_monitored("heartbeat-loop", async move {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        loop {
            interval.tick().await;

            let request = build_heartbeat_request(
                &heartbeat_worker_id,
                &heartbeat_system,
                heartbeat_max_builds,
                &heartbeat_running,
            )
            .await;

            match heartbeat_client.heartbeat(request).await {
                Ok(response) => {
                    let resp = response.into_inner();
                    if !resp.accepted {
                        tracing::warn!("heartbeat rejected by scheduler");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "heartbeat failed");
                }
            }
        }
    });

    // Shared context for spawning build tasks (clones done once per assignment
    // inside spawn_build_task, not here).
    let build_ctx = BuildSpawnContext {
        store_client,
        worker_id,
        fuse_mount_point: args.fuse_mount_point,
        overlay_base_dir: args.overlay_base_dir,
        stream_tx,
        running_builds,
        leaked_mounts: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };

    // Process incoming scheduler messages
    let mut build_stream = build_stream;

    while let Some(msg_result) = tokio_stream::StreamExt::next(&mut build_stream).await {
        // A worker without a live heartbeat is a liability (scheduler will
        // time it out and re-dispatch its builds). Die fast rather than
        // silently duplicate work.
        if heartbeat_handle.is_finished() {
            tracing::error!("heartbeat loop terminated unexpectedly; exiting");
            std::process::exit(1);
        }

        let msg = match msg_result {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(error = %e, "build execution stream error");
                break;
            }
        };

        match msg.msg {
            Some(scheduler_message::Msg::Assignment(assignment)) => {
                info!(drv_path = %assignment.drv_path, "received work assignment");

                // Acquire permit BEFORE ACKing: don't tell the scheduler we
                // accepted work we can't immediately start.
                let permit = match build_semaphore.clone().acquire_owned().await {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!(error = %e, "semaphore closed");
                        break;
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
                    "received prefetch hint (prefetch not yet implemented)"
                );
            }
            None => {
                tracing::warn!("received empty scheduler message");
            }
        }
    }

    info!("build execution stream closed, shutting down");
    Ok(())
}
