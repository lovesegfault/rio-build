//! `rio-mountd` — privileged per-node broker for castore-FUSE.
//!
//! Thin clap wrapper around [`rio_builder::castore_fuse::mountd::run`];
//! all behavior (and its tests) lives in the library module. Deployed
//! as a DaemonSet with `CAP_SYS_ADMIN` on builder/fetcher nodes; see
//! `infra/helm/rio-build/templates/mountd-ds.yaml`.

use std::path::PathBuf;

use clap::Parser;
use rio_builder::castore_fuse::mountd::{self, DEFAULT_MAX_PROMOTE_BYTES, MountdConfig};

#[derive(Parser, Debug)]
#[command(
    name = "rio-mountd",
    about = "Privileged per-node broker: /dev/fuse handoff, BACKING_OPEN, verified cache promotion"
)]
struct Args {
    /// UDS listen path (created mode 0660, group --allowed-gid).
    #[arg(long, default_value = "/run/rio-mountd.sock")]
    socket: PathBuf,
    /// Per-build FUSE mountpoint root.
    #[arg(long, default_value = "/var/rio/castore")]
    castore_dir: PathBuf,
    /// Per-build staging root (XFS with prjquota in production).
    #[arg(long, default_value = "/var/rio/staging")]
    staging_dir: PathBuf,
    /// Shared file-digest backing cache root.
    #[arg(long, default_value = "/var/rio/cache")]
    cache_dir: PathBuf,
    /// Shared chunk-digest cache root.
    #[arg(long, default_value = "/var/rio/chunks")]
    chunks_dir: PathBuf,
    /// Kernel-enforced per-build staging quota in bytes. 0 disables the
    /// quota (only for staging filesystems without project-quota
    /// support; production always sets this).
    #[arg(long, default_value_t = 10 << 30)]
    staging_quota_bytes: u64,
    /// Per-Promote size ceiling in bytes.
    #[arg(long, default_value_t = DEFAULT_MAX_PROMOTE_BYTES)]
    max_promote_bytes: u64,
    /// SO_PEERCRED gid allowed to connect (the builder pods' fsGroup).
    #[arg(long)]
    allowed_gid: u32,
    /// Prometheus exporter listen address.
    #[arg(long, default_value = "[::]:9095")]
    metrics_addr: std::net::SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    // Not `rio_common::server::bootstrap`: that path exists for
    // binaries with a layered TOML config and a CommonConfig
    // embed. mountd has eight flags and no config file; the full
    // bootstrap would add a Config type, a validate impl, and a config
    // search path for nothing. Tracing + metrics init is the part that
    // matters and is shared directly.
    let _otel_guard = rio_common::observability::init_tracing("mountd")?;
    // The crate-wide bucket table includes the rio_mountd_* histogram
    // ranges; passing the whole table is harmless for the buckets that
    // belong to metrics this binary never emits.
    rio_common::observability::init_metrics(
        args.metrics_addr,
        &[],
        rio_builder::HISTOGRAM_BUCKETS,
    )?;
    mountd::describe_metrics();
    // SIGTERM/SIGINT → return from main so atexit handlers run (otel
    // span flush, LLVM profraw flush in coverage builds) instead of the
    // default disposition's immediate kill. Deliberately no connection
    // drain: live FUSE connections abort when the kept fds close, and
    // the next incarnation's startup orphan scan reaps the leftover
    // mountpoints, staging trees, and .promoting placeholders.
    let shutdown = rio_common::signal::shutdown_signal();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting rio-mountd");

    let serve = mountd::run(MountdConfig {
        socket_path: args.socket,
        castore_dir: args.castore_dir,
        staging_dir: args.staging_dir,
        cache_dir: args.cache_dir,
        chunks_dir: args.chunks_dir,
        staging_quota_bytes: args.staging_quota_bytes,
        max_promote_bytes: args.max_promote_bytes,
        allowed_gid: args.allowed_gid,
    });
    tokio::select! {
        r = serve => r,
        () = shutdown.cancelled() => Ok(()),
    }
}
