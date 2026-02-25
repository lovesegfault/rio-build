//! rio-worker binary entry point.
//!
//! Wires up FUSE daemon, gRPC clients (WorkerService + StoreService),
//! executor, and heartbeat loop.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use tokio::sync::{Semaphore, mpsc};
use tonic::transport::Channel;
use tracing::info;

use rio_proto::store::store_service_client::StoreServiceClient;
use rio_proto::types::{
    CompletionReport, HeartbeatRequest, ResourceUsage, WorkAssignmentAck, WorkerMessage,
    scheduler_message, worker_message,
};
use rio_proto::worker::worker_service_client::WorkerServiceClient;
use rio_worker::executor;
use rio_worker::fuse;

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
    let log_format = rio_common::observability::log_format_from_env();
    rio_common::observability::init_logging(log_format, None)?;

    let worker_id = args.worker_id.unwrap_or_else(|| {
        nix::unistd::gethostname()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "unknown".to_string())
    });

    let system = args.system.unwrap_or_else(detect_system);

    let _root_guard =
        tracing::info_span!("worker", component = "worker", worker_id = %worker_id).entered();
    info!(version = env!("CARGO_PKG_VERSION"), "starting rio-worker");

    rio_common::observability::init_metrics(args.metrics_addr)?;

    // Connect to gRPC services
    let max_msg_size = rio_proto::max_message_size();

    let store_endpoint = format!("http://{}", args.store_addr);
    let store_channel = Channel::from_shared(store_endpoint)?.connect().await?;
    let store_client = StoreServiceClient::new(store_channel)
        .max_decoding_message_size(max_msg_size)
        .max_encoding_message_size(max_msg_size);

    let scheduler_endpoint = format!("http://{}", args.scheduler_addr);
    let scheduler_channel = Channel::from_shared(scheduler_endpoint)?.connect().await?;
    let mut scheduler_client = WorkerServiceClient::new(scheduler_channel)
        .max_decoding_message_size(max_msg_size)
        .max_encoding_message_size(max_msg_size);

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
    let outbound = tokio_stream::wrappers::ReceiverStream::new(stream_rx);

    let build_stream = scheduler_client
        .build_execution(outbound)
        .await?
        .into_inner();

    // Concurrent build semaphore
    let build_semaphore = std::sync::Arc::new(Semaphore::new(args.max_builds as usize));

    // Spawn heartbeat loop
    let heartbeat_worker_id = worker_id.clone();
    let heartbeat_system = system.clone();
    let heartbeat_max_builds = args.max_builds;
    let mut heartbeat_client = scheduler_client.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        loop {
            interval.tick().await;

            let request = HeartbeatRequest {
                worker_id: heartbeat_worker_id.clone(),
                running_builds: Vec::new(),
                resources: Some(ResourceUsage::default()),
                local_paths: None,
                system: heartbeat_system.clone(),
                supported_features: Vec::new(),
                max_builds: heartbeat_max_builds,
            };

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

    // Process incoming scheduler messages
    let mut build_stream = build_stream;
    let fuse_mount_point = args.fuse_mount_point.clone();
    let overlay_base_dir = args.overlay_base_dir.clone();

    while let Some(msg_result) = tokio_stream::StreamExt::next(&mut build_stream).await {
        let msg = match msg_result {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(error = %e, "build execution stream error");
                break;
            }
        };

        match msg.msg {
            Some(scheduler_message::Msg::Assignment(assignment)) => {
                let drv_path = assignment.drv_path.clone();
                let assignment_token = assignment.assignment_token.clone();

                info!(drv_path = %drv_path, "received work assignment");

                // Send ACK
                let ack = WorkerMessage {
                    msg: Some(worker_message::Msg::Ack(WorkAssignmentAck {
                        drv_path: drv_path.clone(),
                        assignment_token: assignment_token.clone(),
                    })),
                };
                if let Err(e) = stream_tx.send(ack).await {
                    tracing::error!(error = %e, "failed to send ACK");
                    continue;
                }

                // Acquire semaphore permit for concurrent build control
                let permit = match build_semaphore.clone().acquire_owned().await {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!(error = %e, "semaphore closed");
                        break;
                    }
                };

                // Spawn build task
                let mut build_store_client = store_client.clone();
                let build_worker_id = worker_id.clone();
                let build_fuse_mount = fuse_mount_point.clone();
                let build_overlay_dir = overlay_base_dir.clone();
                let build_tx = stream_tx.clone();

                tokio::spawn(async move {
                    let _permit = permit; // Hold permit until build completes

                    let result = executor::execute_build(
                        &assignment,
                        &build_fuse_mount,
                        &build_overlay_dir,
                        &mut build_store_client,
                        &build_worker_id,
                        &build_tx,
                    )
                    .await;

                    // Send CompletionReport
                    let completion = match result {
                        Ok(exec_result) => CompletionReport {
                            drv_path: exec_result.drv_path,
                            result: Some(exec_result.result),
                            assignment_token: exec_result.assignment_token,
                        },
                        Err(e) => {
                            tracing::error!(
                                drv_path = %drv_path,
                                error = %e,
                                "build execution failed"
                            );
                            CompletionReport {
                                drv_path,
                                result: Some(rio_proto::types::BuildResult {
                                    status:
                                        rio_proto::types::BuildResultStatus::InfrastructureFailure
                                            .into(),
                                    error_msg: format!("{e}"),
                                    ..Default::default()
                                }),
                                assignment_token,
                            }
                        }
                    };

                    let msg = WorkerMessage {
                        msg: Some(worker_message::Msg::Completion(completion)),
                    };
                    if let Err(e) = build_tx.send(msg).await {
                        tracing::error!(error = %e, "failed to send completion report");
                    }
                });
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
