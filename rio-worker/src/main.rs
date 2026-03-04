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
    /// Phase2b log limits (configuration.md:68-69). 0 = unlimited.
    /// Not yet wired to a consumer — C7 does that.
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
            // configuration.md:68-69 specs these; current behavior (unlimited)
            // is preserved until C7 wires them into LogBatcher.
            log_rate_limit: 10_000,
            log_size_limit: 100 * 1024 * 1024, // 100 MiB
            size_class: String::new(),
            max_leaked_mounts: 3,
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

    // Set up FUSE cache and mount
    let cache = fuse::cache::Cache::new(cfg.fuse_cache_dir, cfg.fuse_cache_size_gb).await?;
    // Extract the bloom handle BEFORE cache moves into mount_fuse_background.
    // The heartbeat loop reads from the same RwLock that cache.insert() writes
    // to — inserts by FUSE ops show up in subsequent heartbeat snapshots.
    let heartbeat_bloom = cache.bloom_handle();
    let runtime = tokio::runtime::Handle::current();

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
}
