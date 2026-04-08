//! Builder configuration: Config + CliArgs (two-struct split per
//! rio-common/src/config.rs) and system auto-detection.
//!
//! Extracted from main.rs to keep the binary entry point focused on
//! the event-loop wiring.

use std::path::PathBuf;

use clap::Parser;
use rio_proto::types::ExecutorKind;
use serde::{Deserialize, Serialize};

/// Serde deserializer for ExecutorKind from string ("builder" / "fetcher").
/// Env var `RIO_EXECUTOR_KIND` carries this; prost's i32 repr isn't
/// operator-friendly.
fn executor_kind<'de, D: serde::Deserializer<'de>>(d: D) -> Result<ExecutorKind, D::Error> {
    let s: String = Deserialize::deserialize(d)?;
    match s.as_str() {
        "" | "builder" => Ok(ExecutorKind::Builder),
        "fetcher" => Ok(ExecutorKind::Fetcher),
        other => Err(serde::de::Error::custom(format!(
            "invalid executor kind {other:?}, must be 'builder' or 'fetcher'"
        ))),
    }
}

/// Serde serializer for ExecutorKind as string. Needed for the
/// `Serialized::defaults` base layer in figment.
fn executor_kind_ser<S: serde::Serializer>(k: &ExecutorKind, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(match k {
        ExecutorKind::Builder => "builder",
        ExecutorKind::Fetcher => "fetcher",
    })
}

// ---------------------------------------------------------------------------
// Configuration (two-struct split per rio-common/src/config.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Config {
    /// If empty after merge → auto-detect via hostname.
    pub(crate) executor_id: String,
    /// Builder (airgapped, arbitrary derivation code) or fetcher
    /// (open egress, FOD-only). Env: `RIO_EXECUTOR_KIND=builder|fetcher`.
    /// Default builder (wire-compat pre-ADR-019). Sent in heartbeat
    /// so the scheduler routes FODs to fetchers only
    /// (spec sched.dispatch.fod-to-fetcher).
    #[serde(
        deserialize_with = "executor_kind",
        serialize_with = "executor_kind_ser"
    )]
    pub(crate) executor_kind: ExecutorKind,
    pub(crate) scheduler_addr: String,
    /// Headless Service host for health-aware balanced routing.
    /// See rio-gateway's identical field for the full story.
    /// `None` (env unset) = single-channel fallback.
    pub(crate) scheduler_balance_host: Option<String>,
    pub(crate) scheduler_balance_port: u16,
    pub(crate) store_addr: String,
    /// rio-store headless Service host. Same balance pattern as
    /// scheduler — but for load distribution across replicas, not
    /// leader routing (all store pods serve). `None` = single-channel.
    pub(crate) store_balance_host: Option<String>,
    pub(crate) store_balance_port: u16,
    /// Systems this builder can build for. Empty after merge →
    /// auto-detect single element via std::env::consts. Multi-
    /// element for qemu-user-static or cross-arch builders.
    /// Env: `RIO_SYSTEMS=x86_64-linux,aarch64-linux` (comma-sep)
    /// or TOML `systems = ["x86_64-linux"]`.
    #[serde(deserialize_with = "rio_common::config::comma_vec")]
    pub(crate) systems: Vec<String>,
    /// requiredSystemFeatures this builder supports (e.g., "kvm",
    /// "big-parallel"). Scheduler's can_build() all-matches the
    /// derivation's required_features against this. Must be populated
    /// or the scheduler's can_build() check fails for any derivation
    /// with requiredSystemFeatures.
    #[serde(deserialize_with = "rio_common::config::comma_vec")]
    pub(crate) features: Vec<String>,
    pub(crate) fuse_mount_point: PathBuf,
    pub(crate) fuse_cache_dir: PathBuf,
    pub(crate) fuse_threads: u32,
    /// Defaults to `true`. NOT the serde bool default — see `default_true`.
    /// A drift here (`false`) would silently disable kernel passthrough,
    /// adding a userspace copy per FUSE read and ~2× per-build latency.
    pub(crate) fuse_passthrough: bool,
    /// Timeout (seconds) for FUSE-initiated `GetPath` fetches. Default 60.
    /// NOT the global `GRPC_STREAM_TIMEOUT` (300s) — that's for large-NAR
    /// uploads and passthrough. FUSE fetches are the build-critical path;
    /// a stalled fetch blocks a fuser thread, and a few stalls freeze the
    /// whole mount.
    ///
    /// I-211: this is an IDLE bound — 60s without a stream message — not
    /// a wall-clock bound on the whole fetch. A stuck store (no chunks
    /// arriving) still trips at 60s; a healthy store streaming a 2.9 GB
    /// NAR completes regardless of total duration. Preserves the I-165
    /// circuit-breaker tuning: a genuinely stalled fetch fails at 60s, so
    /// 60s × 5 threshold failures = 300s to circuit-open (matching
    /// `GRPC_STREAM_TIMEOUT`), and detached warm-stat threads unpark
    /// within 60s. Pre-I-211, the wall-clock bound aborted a 2.9 GB
    /// `clang-21.1.8-debug` mid-stream → daemon EIO → build failure on a
    /// healthy store. Env: `RIO_FUSE_FETCH_TIMEOUT_SECS`.
    #[serde(rename = "fuse_fetch_timeout_secs", with = "rio_common::config::secs")]
    pub(crate) fuse_fetch_timeout: std::time::Duration,
    pub(crate) overlay_base_dir: PathBuf,
    #[serde(flatten)]
    pub(crate) common: rio_common::config::CommonConfig,
    /// HTTP /healthz + /readyz listen address. Builder has no gRPC
    /// server so tonic-health doesn't fit — plain HTTP via axum.
    /// K8s readinessProbe hits /readyz (200 after first accepted
    /// heartbeat), livenessProbe hits /healthz (always 200).
    pub(crate) health_addr: std::net::SocketAddr,
    /// Log limits (configuration.md:68-69). 0 = unlimited.
    /// Wired into LogLimits → LogBatcher in main().
    ///
    /// NOT in BuilderPoolSpec (CRD): the defaults (10k lines/sec,
    /// 100 MiB) are already generous enough that hitting them
    /// means the build is broken, not the limit. Operators
    /// shouldn't be tuning around a runaway log producer. See
    /// plan 21 Batch E `cfg-builder-knobs-unreachable-in-k8s`.
    pub(crate) log_rate_limit: u64,
    pub(crate) log_size_limit: u64,
    /// Size-class this builder is deployed as. Empty = unclassified.
    /// If the scheduler has size_classes configured, unclassified
    /// builders are REJECTED (misconfiguration — set this). Operator
    /// sets it to match the scheduler's size_classes config — e.g.
    /// "small" builders on cheap spot instances, "large" on
    /// memory-optimized. The scheduler routes by estimated duration;
    /// this just declares which bucket this builder serves.
    pub(crate) size_class: String,
    /// Timeout (seconds) for the local nix-daemon subprocess build when
    /// the client didn't specify BuildOptions.build_timeout. Intentionally
    /// long (2h default) — some builds genuinely take that long; this is
    /// a bound on blast radius of a truly stuck daemon, not an expected
    /// build time.
    #[serde(rename = "daemon_timeout_secs", with = "rio_common::config::secs")]
    pub(crate) daemon_timeout: std::time::Duration,
    /// Silence timeout (seconds): kill the build if no output for N seconds.
    /// 0 = disabled. Used when the assignment's BuildOptions.max_silent_time
    /// is 0/unset. Env: `RIO_MAX_SILENT_TIME_SECS`.
    ///
    /// WONTFIX(P0310): ssh-ng client options are dropped client-side — Nix
    /// `SSHStore::setOptions()` is an empty override (ssh-store.cc:81-88,
    /// origin 088ef8175, 2018; intentional per NixOS/nix#1713/#1935), and
    /// exec_request argv is hardcoded `nix-daemon --stdio` with no --option
    /// forwarding (ssh-store.cc:201-215). Source-verified P0310 T0; confirmed
    /// by the `setoptions-unreachable` VM subtest (scheduling.nix). This
    /// config is therefore the ONLY mechanism for silence timeout via ssh-ng.
    /// Clients wanting per-build maxSilentTime must use the gRPC API directly
    /// (rio-cli → `SubmitBuildRequest.build_options.max_silent_time`).
    /// Upstream fix 32827b9fb adds selective ssh-ng forwarding but requires
    /// the daemon to advertise `set-options-map-only`, which rio-gateway does
    /// not — tracked under WONTFIX(P0310).
    pub(crate) max_silent_time_secs: u64,
    /// I-116 idle timeout: exit if no assignment arrives for this
    /// long. Controller spawns N Jobs based on queue depth; if the
    /// queue drains before all Jobs receive work, the unlucky ones
    /// would otherwise idle until activeDeadlineSeconds. Env:
    /// `RIO_IDLE_SECS`. Default 120.
    #[serde(rename = "idle_secs", with = "rio_common::config::secs")]
    pub(crate) idle_timeout: std::time::Duration,
    /// FUSE fetch transport (`getpath` | `getchunk`). Env:
    /// `RIO_FETCH_TRANSPORT`. Default `getpath` — chunk fan-out is
    /// opt-in until A/B'd live (see `r[builder.fuse.fetch-chunk-
    /// fanout]`).
    pub(crate) fetch_transport: rio_builder::fuse::fetch::FetchTransport,
    /// Test hook (VM warm-gate ordering proof): inject a delay
    /// between prefetch completion and the `PrefetchComplete` ACK.
    /// Env: `RIO_TEST_PREFETCH_DELAY_MS`. Default 0 (no delay).
    #[serde(rename = "test_prefetch_delay_ms", with = "rio_common::config::millis")]
    pub(crate) test_prefetch_delay: std::time::Duration,
    // fod_proxy_url removed per ADR-019: builders are airgapped; FODs
    // route to fetchers which have direct egress. Squid proxy deleted.
}

impl Default for Config {
    fn default() -> Self {
        Self {
            executor_id: String::new(),
            executor_kind: ExecutorKind::Builder,
            scheduler_addr: String::new(),
            scheduler_balance_host: None,
            scheduler_balance_port: 9001,
            store_addr: String::new(),
            store_balance_host: None,
            store_balance_port: 9002,
            systems: Vec::new(),
            features: Vec::new(),
            // Matches nix/modules/builder.nix. NEVER default to /nix/store:
            // mounting FUSE there shadows the host store, breaking every
            // process on the machine (including the builder itself).
            fuse_mount_point: "/var/rio/fuse-store".into(),
            fuse_cache_dir: "/var/rio/cache".into(),
            fuse_threads: 4,
            fuse_passthrough: true,
            fuse_fetch_timeout: std::time::Duration::from_secs(60),
            overlay_base_dir: "/var/rio/overlays".into(),
            common: rio_common::config::CommonConfig::new(9093),
            // 9193 = metrics (9093) + 100. Same +100 pattern as
            // gateway (9090→9190). Scheduler/store piggyback health
            // on their gRPC ports; builder+gateway have no gRPC server.
            health_addr: rio_common::default_addr(9193),
            // configuration.md:68-69 specs these defaults.
            log_rate_limit: 10_000,
            log_size_limit: 100 * 1024 * 1024, // 100 MiB
            size_class: String::new(),
            daemon_timeout: rio_builder::executor::DEFAULT_DAEMON_TIMEOUT,
            max_silent_time_secs: 0,
            idle_timeout: std::time::Duration::from_secs(120),
            fetch_transport: rio_builder::fuse::fetch::FetchTransport::GetPath,
            test_prefetch_delay: std::time::Duration::ZERO,
        }
    }
}

#[derive(Parser, Serialize, Default)]
#[command(
    name = "rio-builder",
    about = "Build executor with FUSE store for rio-build"
)]
pub(crate) struct CliArgs {
    /// Executor ID (defaults to hostname)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    executor_id: Option<String>,

    /// rio-scheduler gRPC address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    scheduler_addr: Option<String>,

    /// rio-store gRPC address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    store_addr: Option<String>,

    /// Systems this builder builds for (repeatable: `--system
    /// x86_64-linux --system aarch64-linux`). Auto-detected if
    /// not set. Clap's `action = Append` collects repeated flags
    /// into a Vec; serde name `systems` matches the Config field.
    #[arg(long = "system", action = clap::ArgAction::Append)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    systems: Vec<String>,

    /// requiredSystemFeatures this builder supports (repeatable).
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
            d.executor_id.is_empty(),
            "executor_id auto-detects via hostname"
        );
        assert!(d.scheduler_addr.is_empty(), "required, no default");
        assert!(d.store_addr.is_empty(), "required, no default");
        assert!(d.systems.is_empty(), "systems auto-detect");
        assert!(d.features.is_empty(), "features empty by default");
        assert_eq!(d.fuse_mount_point, PathBuf::from("/var/rio/fuse-store"));
        assert_eq!(d.fuse_cache_dir, PathBuf::from("/var/rio/cache"));
        assert_eq!(d.fuse_threads, 4);
        assert_eq!(
            d.fuse_fetch_timeout,
            std::time::Duration::from_secs(60),
            "FUSE fetch timeout: 60s NOT 300s (GRPC_STREAM_TIMEOUT). \
             I-165: on a fresh ephemeral builder wall_clock_trip can't \
             fire (no last_success), so only the 5-consecutive-failure \
             threshold is live. 60s × 5 = 300s to circuit-open; detached \
             warm-stat threads unpark within 60s. A drift back to 600 \
             means warm hangs for ~10min per 3-path batch under store \
             saturation. I-178: this is the BASE timeout; JIT lookup \
             uses jit_fetch_timeout(this, nar_size) per path so large \
             inputs get a size-proportional budget."
        );
        assert!(
            d.fuse_passthrough,
            "fuse_passthrough MUST default to true (phase2a behavior); \
             serde's bool default is false so this needs explicit handling"
        );
        assert_eq!(d.overlay_base_dir, PathBuf::from("/var/rio/overlays"));
        assert_eq!(d.common.metrics_addr.to_string(), "[::]:9093");
        assert_eq!(d.health_addr.to_string(), "[::]:9193");
        // Spec values from configuration.md:68-69.
        assert_eq!(d.log_rate_limit, 10_000);
        assert_eq!(d.log_size_limit, 100 * 1024 * 1024);
    }

    #[test]
    fn cli_args_parse_help() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert();
    }

    /// `--fuse-passthrough` must accept explicit true/false (not a flag).
    #[test]
    fn cli_fuse_passthrough_explicit_bool() {
        let args = CliArgs::try_parse_from(["rio-builder", "--fuse-passthrough", "false"]).unwrap();
        assert_eq!(args.fuse_passthrough, Some(false));
        let args = CliArgs::try_parse_from(["rio-builder", "--fuse-passthrough", "true"]).unwrap();
        assert_eq!(args.fuse_passthrough, Some(true));
        // Absent → None (layering: don't overlay).
        let args = CliArgs::try_parse_from(["rio-builder"]).unwrap();
        assert_eq!(args.fuse_passthrough, None);
    }

    // figment::Jail standing-guard tests — see rio-test-support/src/config.rs.
    // When you add Config.newfield: ADD IT to both assert blocks below.

    rio_test_support::jail_roundtrip!(
        "builder",
        r#"
        fuse_passthrough = false
        fuse_fetch_timeout_secs = 222
        fetch_transport = "getchunk"
        systems = ["x86_64-linux", "aarch64-linux"]

        [tls]
        cert_path = "/etc/tls/cert.pem"
        "#,
        |cfg: Config| {
            assert!(
                !cfg.fuse_passthrough,
                "TOML scalar must override the non-serde-bool default of true"
            );
            assert_eq!(cfg.fuse_fetch_timeout, std::time::Duration::from_secs(222));
            assert_eq!(
                cfg.fetch_transport,
                rio_builder::fuse::fetch::FetchTransport::GetChunk
            );
            assert_eq!(cfg.systems, vec!["x86_64-linux", "aarch64-linux"]);
            assert_eq!(
                cfg.common.tls.cert_path.as_deref(),
                Some(std::path::Path::new("/etc/tls/cert.pem")),
                "[tls] table must thread through figment into TlsConfig"
            );
            // Unspecified sub-field defaults via #[serde(default)]
            // on TlsConfig (partial table must work).
            assert!(cfg.common.tls.key_path.is_none());
        }
    );

    rio_test_support::jail_defaults!("builder", "", |cfg: Config| {
        assert!(!cfg.common.tls.is_configured());
        assert!(cfg.scheduler_balance_host.is_none());
        assert_eq!(cfg.executor_kind, ExecutorKind::Builder);
        assert!(cfg.systems.is_empty());
        assert!(cfg.features.is_empty());
        // The critical non-serde-bool default: fuse_passthrough
        // must survive the Serialized::defaults → TOML merge as
        // `true`. This is the load-bearing check — serde's bool
        // default is `false`, so if figment's base-layer
        // serialization drops it, a pre-phase3a config silently
        // loses kernel passthrough.
        assert!(
            cfg.fuse_passthrough,
            "near-empty TOML must preserve fuse_passthrough=true \
             via Serialized::defaults base layer"
        );
        assert_eq!(cfg.fuse_fetch_timeout, std::time::Duration::from_secs(60));
    });
}
