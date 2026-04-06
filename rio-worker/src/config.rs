//! Worker configuration: Config + CliArgs (two-struct split per
//! rio-common/src/config.rs) and system auto-detection.
//!
//! Extracted from main.rs to keep the binary entry point focused on
//! the event-loop wiring.

use std::path::PathBuf;

use clap::Parser;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Configuration (two-struct split per rio-common/src/config.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Config {
    /// If empty after merge → auto-detect via hostname.
    pub(crate) worker_id: String,
    pub(crate) scheduler_addr: String,
    /// Headless Service host for health-aware balanced routing.
    /// See rio-gateway's identical field for the full story.
    /// `None` (env unset) = single-channel fallback.
    pub(crate) scheduler_balance_host: Option<String>,
    pub(crate) scheduler_balance_port: u16,
    pub(crate) store_addr: String,
    pub(crate) max_builds: u32,
    /// Systems this worker can build for. Empty after merge →
    /// auto-detect single element via std::env::consts. Multi-
    /// element for qemu-user-static or cross-arch workers.
    /// Env: `RIO_SYSTEMS=x86_64-linux,aarch64-linux` (comma-sep)
    /// or TOML `systems = ["x86_64-linux"]`.
    #[serde(deserialize_with = "rio_common::config::comma_vec")]
    pub(crate) systems: Vec<String>,
    /// requiredSystemFeatures this worker supports (e.g., "kvm",
    /// "big-parallel"). Scheduler's can_build() all-matches the
    /// derivation's required_features against this. Must be populated
    /// or the scheduler's can_build() check fails for any derivation
    /// with requiredSystemFeatures.
    #[serde(deserialize_with = "rio_common::config::comma_vec")]
    pub(crate) features: Vec<String>,
    pub(crate) fuse_mount_point: PathBuf,
    pub(crate) fuse_cache_dir: PathBuf,
    pub(crate) fuse_cache_size_gb: u64,
    pub(crate) fuse_threads: u32,
    /// Defaults to `true`. NOT the serde bool default — see `default_true`.
    /// A drift here (`false`) would silently disable kernel passthrough,
    /// adding a userspace copy per FUSE read and ~2× per-build latency.
    pub(crate) fuse_passthrough: bool,
    /// Timeout (seconds) for FUSE-initiated `GetPath` fetches. Default 180.
    /// NOT the global `GRPC_STREAM_TIMEOUT` (300s) — that's for large-NAR
    /// uploads and passthrough. FUSE fetches are the build-critical path;
    /// a stalled fetch blocks a fuser thread, and a few stalls freeze the
    /// whole mount.
    ///
    /// 180s (not 60s) because a slow-but-alive store under k8s/VM network
    /// overhead can take >60s per fetch — and the circuit's `wall_clock_trip`
    /// (90s) means TWO 60s timeouts opens the circuit even though the store
    /// is healthy. 180s lets slow fetches complete and refresh `last_success`;
    /// a truly dead store still trips within `wall_clock_trip + 1 fetch`
    /// (~270s) or `5 × 180s` (15min) via consecutive-failures — both far
    /// below the 25min pre-circuit behavior. Env: `RIO_FUSE_FETCH_TIMEOUT_SECS`.
    pub(crate) fuse_fetch_timeout_secs: u64,
    pub(crate) overlay_base_dir: PathBuf,
    pub(crate) metrics_addr: std::net::SocketAddr,
    /// HTTP /healthz + /readyz listen address. Worker has no gRPC
    /// server so tonic-health doesn't fit — plain HTTP via axum.
    /// K8s readinessProbe hits /readyz (200 after first accepted
    /// heartbeat), livenessProbe hits /healthz (always 200).
    pub(crate) health_addr: std::net::SocketAddr,
    /// Log limits (configuration.md:68-69). 0 = unlimited.
    /// Wired into LogLimits → LogBatcher in main().
    ///
    /// NOT in WorkerPoolSpec (CRD): the defaults (10k lines/sec,
    /// 100 MiB) are already generous enough that hitting them
    /// means the build is broken, not the limit. Operators
    /// shouldn't be tuning around a runaway log producer. See
    /// plan 21 Batch E `cfg-worker-knobs-unreachable-in-k8s`.
    pub(crate) log_rate_limit: u64,
    pub(crate) log_size_limit: u64,
    /// Size-class this worker is deployed as. Empty = unclassified.
    /// If the scheduler has size_classes configured, unclassified
    /// workers are REJECTED (misconfiguration — set this). Operator
    /// sets it to match the scheduler's size_classes config — e.g.
    /// "small" workers on cheap spot instances, "large" on
    /// memory-optimized. The scheduler routes by estimated duration;
    /// this just declares which bucket this worker serves.
    pub(crate) size_class: String,
    /// Threshold for leaked overlay mounts before refusing new builds.
    /// After N umount2 failures (stuck-busy mounts), the worker is
    /// degraded; execute_build short-circuits with InfrastructureFailure
    /// so the scheduler reassigns and the supervisor can restart.
    ///
    /// NOT in WorkerPoolSpec (CRD): this is a "when to give up"
    /// threshold, not a tunable. 3 leaked mounts is the point where
    /// the overlay namespace is corrupt enough that continuing to
    /// accept builds wastes scheduler time. No operator has a reason
    /// to set this to 5 or 10.
    pub(crate) max_leaked_mounts: usize,
    /// Timeout (seconds) for the local nix-daemon subprocess build when
    /// the client didn't specify BuildOptions.build_timeout. Intentionally
    /// long (2h default) — some builds genuinely take that long; this is
    /// a bound on blast radius of a truly stuck daemon, not an expected
    /// build time.
    pub(crate) daemon_timeout_secs: u64,
    /// Silence timeout (seconds): kill the build if no output for N seconds.
    /// 0 = disabled. Used when the assignment's BuildOptions.max_silent_time
    /// is 0/unset. Env: `RIO_MAX_SILENT_TIME_SECS`.
    ///
    /// Why this exists: the Nix ssh-ng client does NOT send wopSetOptions
    /// to the gateway (protocol 1.38), so client-side `--max-silent-time`
    /// cannot propagate. This config is the operator's fleet-wide default
    /// until a gateway-side propagation path lands.
    /// TODO(P0215): gateway-side client-option propagation follow-up.
    pub(crate) max_silent_time_secs: u64,
    /// mTLS for outgoing gRPC (scheduler + store). Env: `RIO_TLS__*`.
    /// Unset = plaintext.
    pub(crate) tls: rio_common::tls::TlsConfig,
    /// Forward proxy URL for FOD builds. Injected as http_proxy/
    /// https_proxy env into the daemon spawn ONLY when is_fixed_
    /// output is true. Env: `RIO_FOD_PROXY_URL`. Unset = FODs
    /// have direct internet.
    pub(crate) fod_proxy_url: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            worker_id: String::new(),
            scheduler_addr: String::new(),
            scheduler_balance_host: None,
            scheduler_balance_port: 9001,
            store_addr: String::new(),
            max_builds: 1,
            systems: Vec::new(),
            features: Vec::new(),
            // Matches nix/modules/worker.nix. NEVER default to /nix/store:
            // mounting FUSE there shadows the host store, breaking every
            // process on the machine (including the worker itself).
            fuse_mount_point: "/var/rio/fuse-store".into(),
            fuse_cache_dir: "/var/rio/cache".into(),
            fuse_cache_size_gb: 50,
            fuse_threads: 4,
            fuse_passthrough: true,
            fuse_fetch_timeout_secs: 180,
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
            max_silent_time_secs: 0,
            tls: rio_common::tls::TlsConfig::default(),
            fod_proxy_url: None,
        }
    }
}

#[derive(Parser, Serialize, Default)]
#[command(
    name = "rio-worker",
    about = "Build executor with FUSE store for rio-build"
)]
pub(crate) struct CliArgs {
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

    /// Systems this worker builds for (repeatable: `--system
    /// x86_64-linux --system aarch64-linux`). Auto-detected if
    /// not set. Clap's `action = Append` collects repeated flags
    /// into a Vec; serde name `systems` matches the Config field.
    #[arg(long = "system", action = clap::ArgAction::Append)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    systems: Vec<String>,

    /// requiredSystemFeatures this worker supports (repeatable).
    #[arg(long = "feature", action = clap::ArgAction::Append)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    features: Vec<String>,

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

/// Detect the system architecture (e.g. "x86_64-linux").
pub(crate) fn detect_system() -> String {
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
        assert!(d.systems.is_empty(), "systems auto-detect");
        assert!(d.features.is_empty(), "features empty by default");
        assert_eq!(d.fuse_mount_point, PathBuf::from("/var/rio/fuse-store"));
        assert_eq!(d.fuse_cache_dir, PathBuf::from("/var/rio/cache"));
        assert_eq!(d.fuse_cache_size_gb, 50);
        assert_eq!(d.fuse_threads, 4);
        assert_eq!(
            d.fuse_fetch_timeout_secs, 180,
            "FUSE fetch timeout: 180s NOT 300s (GRPC_STREAM_TIMEOUT). \
             60s was too tight — k8s/VM overhead makes slow-but-alive \
             fetches take >60s; two timeouts → wall_clock_trip (90s) \
             opens the circuit on a healthy store. A drift to 300 means \
             the circuit never trips before the old 25min behavior."
        );
        assert!(
            d.fuse_passthrough,
            "fuse_passthrough MUST default to true (phase2a behavior); \
             serde's bool default is false so this needs explicit handling"
        );
        assert_eq!(d.overlay_base_dir, PathBuf::from("/var/rio/overlays"));
        assert_eq!(d.metrics_addr.to_string(), "0.0.0.0:9093");
        assert_eq!(d.health_addr.to_string(), "0.0.0.0:9193");
        // Spec values from configuration.md:68-69.
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
