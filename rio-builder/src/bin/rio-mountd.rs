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
    /// UDS listen path (created mode 0660, group --allowed-gid; the
    /// parent directory is created if missing).
    #[arg(long, default_value = "/run/rio-mountd/mountd.sock")]
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
    /// SO_PEERCRED gid allowed to connect — the host `rio-builder`
    /// group (helm `mountd.allowedGid`). Without --token-key-path this
    /// is the only admission path (socket 0660, wrong gid dropped
    /// before any frame); with it, peers outside this gid may instead
    /// be admitted by a verifying Mount token (ADR-022 §P0559).
    #[arg(long)]
    allowed_gid: u32,
    /// HMAC key file for verifying scheduler-minted Mount-admission
    /// tokens (ADR-022 §P0559) — the DEDICATED mountd key, never the
    /// store-facing assignment key. Enables token mode: the socket
    /// becomes world-connectable (0666) and a connection whose gid is
    /// not --allowed-gid is admitted only if its Mount{} carries a
    /// token that verifies (signature, expiry, audience, build_id).
    /// Unset = token mode off; gid-only admission, socket 0660 —
    /// exactly the pre-token posture. Helm sets this via the
    /// `mountdHmac.secretName` Secret mount + env. Superseded by
    /// --token-pubkey-path (the symmetric arm is deleted in the final
    /// ADR-022 §P0590 phase); both may be set only as the documented
    /// contingency overlap.
    #[arg(long, env = "RIO_MOUNTD_HMAC_KEY_PATH")]
    token_key_path: Option<PathBuf>,
    /// Ed25519 trust-roots file for verifying scheduler-minted `rmt2`
    /// Mount-admission tokens (ADR-022 mount-admission credentials,
    /// §P0590): one `rio-mountd-<n>:base64(32-byte pubkey)` line per
    /// active key (multiple lines = rotation overlap). PUBLIC material
    /// only — holding it mints nothing. Enables token mode exactly like
    /// --token-key-path (socket 0666; non-gid peers admitted by a
    /// verifying Mount token). Helm sets this via the
    /// `mountdSigning.publicKeySecretName` Secret mount + env.
    #[arg(long, env = "RIO_MOUNTD_PUBKEY_PATH")]
    token_pubkey_path: Option<PathBuf>,
    /// This node's kube `spec.nodeName` (helm: downward API). When set,
    /// an `rmt2` token is admitted only if its node claim names exactly
    /// this node — the cross-node replay closure. Unset = node check
    /// skipped (standalone/systemd posture). Ignored for legacy HMAC
    /// tokens (they carry no node claim).
    #[arg(long, env = "RIO_MOUNTD_NODE_NAME")]
    node_name: Option<String>,
    /// Disk-pressure sweep period in seconds: how often statvfs probes
    /// the cache/chunks/staging trees and, below 10% free, evicts
    /// (orphaned staging, then chunks, then cache, oldest first) until
    /// 20% is free again. 0 disables the sweep.
    #[arg(long, default_value_t = 30)]
    sweep_interval_secs: u64,
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
        token_key_path: args.token_key_path,
        token_pubkey_path: args.token_pubkey_path,
        node_name: args.node_name,
        sweep_interval: std::time::Duration::from_secs(args.sweep_interval_secs),
    });
    tokio::select! {
        r = serve => r,
        () = shutdown.cancelled() => Ok(()),
    }
}
