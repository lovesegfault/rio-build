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
/// compiled-defaults base layer of the config loader.
fn executor_kind_ser<S: serde::Serializer>(k: &ExecutorKind, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(match k {
        ExecutorKind::Builder => "builder",
        ExecutorKind::Fetcher => "fetcher",
    })
}

// ---------------------------------------------------------------------------
// Configuration (two-struct split per rio-common/src/config.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
/// Builder runtime configuration: the merged CLI + file + env view
/// `load()` produces (see the module doc for precedence).
pub struct Config {
    /// If empty after merge → auto-detect via hostname.
    pub executor_id: String,
    /// Builder (airgapped, arbitrary derivation code) or fetcher
    /// (open egress, FOD-only). Env: `RIO_EXECUTOR_KIND=builder|fetcher`.
    /// Default builder (wire-compat pre-ADR-019). Bound into the
    /// executor-token claims and presented on `PullAssignment` so the
    /// scheduler routes FODs to fetchers only
    /// (spec sched.dispatch.fod-to-fetcher).
    #[serde(
        deserialize_with = "executor_kind",
        serialize_with = "executor_kind_ser"
    )]
    #[schemars(with = "String")]
    pub executor_kind: ExecutorKind,
    /// rio-scheduler upstream. Env: `RIO_SCHEDULER__ADDR` /
    /// `__BALANCE_HOST` / `__BALANCE_PORT`. `balance_host = Some` →
    /// health-aware p2c (route to leader). `None` = single-channel.
    pub scheduler: rio_common::config::UpstreamAddrs,
    /// rio-store upstream. Env: `RIO_STORE__ADDR` / `__BALANCE_HOST` /
    /// `__BALANCE_PORT`. Balance is for load distribution across
    /// replicas, not leader routing (all store pods serve). I-077: a
    /// sticky single-channel meant scaling rio-store 1→4 didn't help.
    pub store: rio_common::config::UpstreamAddrs,
    /// Systems this builder can build for. Empty after merge →
    /// auto-detect single element via std::env::consts. Multi-
    /// element for qemu-user-static or cross-arch builders.
    /// Env: `RIO_SYSTEMS=x86_64-linux,aarch64-linux` (comma-sep)
    /// or TOML `systems = ["x86_64-linux"]`.
    #[serde(deserialize_with = "rio_common::config::comma_vec")]
    #[schemars(with = "Vec<String>")]
    pub systems: Vec<String>,
    /// requiredSystemFeatures this builder supports (e.g., "kvm",
    /// "big-parallel"). Scheduler's `hard_filter()` all-matches the
    /// derivation's required_features against this. Must be populated
    /// or `rejection_reason()` rejects any derivation with
    /// requiredSystemFeatures.
    #[serde(deserialize_with = "rio_common::config::comma_vec")]
    #[schemars(with = "Vec<String>")]
    pub features: Vec<String>,
    /// rio-mountd UDS socket path. The DaemonSet (P0567) listens here;
    /// every build's castore-FUSE mount starts with a `Mount{build_id}`
    /// handshake on this socket.
    pub mountd_socket: PathBuf,
    /// Per-build castore-FUSE mountpoint root. mountd mounts each
    /// build's handed-off `/dev/fuse` fd at `{castore_dir}/{build_id}`;
    /// the builder uses that path as the overlay lowerdir. Must match
    /// the mountd DaemonSet's `--castore-dir`.
    pub castore_dir: PathBuf,
    /// Per-build staging root (mountd-owned, builder-writable per-build
    /// subdirs). Must match the mountd DaemonSet's `--staging-dir`.
    pub staging_dir: PathBuf,
    /// Shared node file-digest backing cache (mountd-owned, read-only
    /// to the builder). Must match the mountd DaemonSet's `--cache-dir`.
    pub cache_dir: PathBuf,
    /// Shared node chunk cache (mountd-owned, read-only to the
    /// builder). Must match the mountd DaemonSet's `--chunks-dir`.
    pub chunks_dir: PathBuf,
    /// Budget (seconds) for the mount-time `GetDirectory(recursive)`
    /// Directory-DAG prefetch. Expiry is an infrastructure failure
    /// (re-queue), never a wedged mount. Env:
    /// `RIO_DAG_PREFETCH_TIMEOUT_SECS`.
    #[serde(
        rename = "dag_prefetch_timeout_secs",
        with = "rio_common::config::secs"
    )]
    #[schemars(with = "u64")]
    pub dag_prefetch_timeout: std::time::Duration,
    /// Castore-FUSE `open()` cache misses larger than this (bytes) take
    /// the streaming-open path (reply at the first chunk, fill in the
    /// background) instead of a whole-file fetch.
    pub stream_threshold: u64,
    /// Ceiling on concurrently registered passthrough backing ids per
    /// build. The kernel never reclaims a backing slot until
    /// `BACKING_CLOSE`, so a build leaking opens would otherwise grow
    /// mountd's IDR without bound; at the ceiling, opens of new digests
    /// degrade to userspace `FOPEN_KEEP_CACHE` reads instead of
    /// registering more backings.
    pub max_backing_ids: u32,
    /// Escape hatch: reply plain `FOPEN_KEEP_CACHE` and serve `read()`
    /// from userspace instead of registering kernel passthrough
    /// backings. Adds a userspace copy per read (~2× per-build
    /// latency) — only for debugging passthrough itself.
    pub disable_passthrough: bool,
    /// Number of FUSE event-loop threads per build session.
    pub fuse_threads: u32,
    /// Base timeout (seconds) for castore-FUSE `open()` store fetches
    /// (`ReadBlob`/`GetChunks`). Default 60. NOT the global
    /// `GRPC_STREAM_TIMEOUT` (300s) — that's for large-NAR uploads.
    /// open() fetches are the build-critical path; a stalled fetch
    /// blocks a fuser thread. Scaled per file size via
    /// `jit_fetch_timeout` (I-178) so multi-GB inputs get a
    /// size-proportional budget. Env: `RIO_FUSE_FETCH_TIMEOUT_SECS`.
    #[serde(
        rename = "fuse_fetch_timeout_secs",
        with = "rio_common::config::secs_bounded"
    )]
    #[schemars(with = "u64")]
    pub fuse_fetch_timeout: std::time::Duration,
    /// Base directory for per-build overlay upper/work layers.
    pub overlay_base_dir: PathBuf,
    #[serde(flatten)]
    /// Shared service config (telemetry, log format, shutdown grace).
    pub common: rio_common::config::CommonConfig,
    /// HTTP /healthz + /readyz listen address. Builder has no gRPC
    /// server so tonic-health doesn't fit — plain HTTP via axum.
    /// K8s readinessProbe hits /readyz (200 once the assignment is
    /// pulled — readiness = pulled/building), livenessProbe hits
    /// /healthz (always 200).
    pub health_addr: std::net::SocketAddr,
    /// Log rate limit (lines/s). 0 = unlimited.
    /// Wired into LogLimits → LogBatcher in main().
    ///
    /// NOT in PoolSpec (CRD): rate-exceeded lines are suppressed
    /// (not failed), so there's no per-workload tuning need; size
    /// is the hard cap and 100 MiB is large enough that hitting it
    /// means the build is broken. See plan 21 Batch E
    /// `cfg-builder-knobs-unreachable-in-k8s`.
    pub log_rate_limit: u64,
    /// Max total log bytes per build before `LogLimitExceeded`. (0 = unlimited)
    pub log_size_limit: u64,
    /// k8s `spec.nodeName` (downward API → `RIO_NODE_NAME`). Attached
    /// to `CompletionReport` for ADR-023's hw_class join (scheduler
    /// resolves instance type from the Node object). Empty outside k8s.
    pub node_name: String,
    /// ADR-023 phase-10: `rio.build/hw-class` pod annotation
    /// (controller-stamped from the Node informer). Resolved at
    /// runtime via the downward-API VOLUME (`/etc/rio/downward/
    /// hw-class`, see [`crate::hw_class::resolve`]) — NOT an env var,
    /// because the env-var form resolves once at container-create and
    /// races `run_pod_annotator` permanently. This config field is
    /// kept for non-k8s test injection (`RIO_HW_CLASS`); when empty,
    /// the volume resolver runs.
    pub hw_class: String,
    /// ADR-023 §13a: `rio.build/hw-bench-needed` pod annotation
    /// (downward API → `RIO_HW_BENCH_NEEDED`). Set by the controller
    /// at pod-create when any h in the intent's admissible set has <3
    /// distinct `pod_id` AND `requests.memory ≥ sla.hwBenchMemFloor`.
    /// Fail-closed: when `false` only the scalar `alu` probe runs and
    /// the K=3 bench is skipped (`r[sched.sla.hw-class.k3-bench]`).
    pub hw_bench_needed: bool,
    /// ADR-023 SpawnIntent match key from the pod's
    /// `rio.build/intent-id` annotation (downward API →
    /// `RIO_INTENT_ID`). Sent on `PullAssignment` so the scheduler can
    /// match this pod to its pre-computed assignment. Empty =
    /// Static-sized pod (no intent).
    pub intent_id: String,
    /// HMAC-signed `ExecutorClaims{intent_id, kind, expiry}` from the
    /// controller's `RIO_EXECUTOR_TOKEN` env var (passed through from
    /// `SpawnIntent.executor_token`). Presented as
    /// `x-rio-executor-token` on every `ExecutorService` unary (and
    /// kept in the `PullAssignment` body — frozen signature) so the
    /// scheduler can bind this pod's pulls to the intent it was
    /// spawned for. Empty in dev mode → header omitted.
    /// See `r[sec.executor.identity-token]`.
    pub executor_token: String,
    /// Timeout (seconds) for the local nix-daemon subprocess build when
    /// the client didn't specify BuildOptions.build_timeout. Intentionally
    /// long (2h default) — some builds genuinely take that long; this is
    /// a bound on blast radius of a truly stuck daemon, not an expected
    /// build time.
    /// Bounded at the shared one-year absurdity ceiling BY TYPE
    /// (saturating at parse): a `u64::MAX`-class "disable the timeout"
    /// value becomes effectively-unbounded-but-arithmetic-safe instead
    /// of panicking the stderr-loop deadline add (bug_117).
    #[serde(rename = "daemon_timeout_secs")]
    #[schemars(with = "u64")]
    pub daemon_timeout: rio_common::config::BoundedSecs,
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
    #[serde(
        rename = "max_silent_time_secs",
        with = "rio_common::config::secs_bounded"
    )]
    #[schemars(with = "u64")]
    pub max_silent_time: std::time::Duration,
    /// I-116 idle timeout: exit if no assignment arrives for this
    /// long. Controller spawns N Jobs based on queue depth; if the
    /// queue drains before all Jobs receive work, the unlucky ones
    /// would otherwise idle until activeDeadlineSeconds. Env:
    /// `RIO_IDLE_SECS`. Default 120. In pull mode this bounds
    /// accumulated told-not-deliverable time: only `NotYetReady`
    /// answers advance the idle clock (each interval capped at twice
    /// the previous answer's suggested pacing), so scheduler outages
    /// between answers do not count and a pod exits 0 charge-free only
    /// after this much *answered* idle time.
    #[serde(rename = "idle_secs", with = "rio_common::config::secs_bounded")]
    #[schemars(with = "u64")]
    pub idle_timeout: std::time::Duration,
    // dispatch_mode removed with the stream client (executor-lifecycle
    // 1d collapse): pull is the only delivery path. The pod-template
    // discriminator (`RIO_DISPATCH_MODE`) retired with the Pool CRD
    // dispatchMode knob; a stray env of that name is ignored by the
    // config loader.
    // fod_proxy_url removed per ADR-019: builders are airgapped; FODs
    // route to fetchers which have direct egress. Squid proxy deleted.
}

impl Default for Config {
    fn default() -> Self {
        Self {
            executor_id: String::new(),
            executor_kind: ExecutorKind::Builder,
            scheduler: rio_common::config::UpstreamAddrs::with_port(9001),
            store: rio_common::config::UpstreamAddrs::with_port(9002),
            systems: Vec::new(),
            features: Vec::new(),
            // Defaults mirror rio-mountd's CLI defaults (bin/rio-mountd.rs)
            // and the helm hostPath layout — the builder only consumes
            // these mountd-owned directories.
            mountd_socket: "/run/rio-mountd.sock".into(),
            castore_dir: "/var/rio/castore".into(),
            staging_dir: "/var/rio/staging".into(),
            cache_dir: "/var/rio/cache".into(),
            chunks_dir: "/var/rio/chunks".into(),
            dag_prefetch_timeout: std::time::Duration::from_secs(30),
            // 8 MiB: ADR-022 §2.8 STREAM_THRESHOLD default — large enough
            // that the bulk of inputs take the simple whole-file path,
            // small enough that the multi-hundred-MB tail streams.
            stream_threshold: 8 * 1024 * 1024,
            // ADR-022 implementation plan value. Large parallel builds
            // (rustc keeping every dependency rlib mapped, C++ TUs with
            // hundreds of headers) legitimately hold >512 distinct
            // store files open at once; each registration costs ~64
            // bytes of kernel memory, so the ceiling is generous.
            max_backing_ids: 4096,
            disable_passthrough: false,
            fuse_threads: 4,
            fuse_fetch_timeout: std::time::Duration::from_secs(60),
            overlay_base_dir: "/var/rio/overlays".into(),
            common: rio_common::config::CommonConfig::new(9093),
            // 9193 = metrics (9093) + 100. Same +100 pattern as
            // gateway (9090→9190). Scheduler/store piggyback health
            // on their gRPC ports; builder+gateway have no gRPC server.
            health_addr: rio_common::default_addr(9193),
            log_rate_limit: 250_000,
            log_size_limit: 100 * 1024 * 1024, // 100 MiB
            node_name: String::new(),
            hw_class: String::new(),
            hw_bench_needed: false,
            intent_id: String::new(),
            executor_token: String::new(),
            daemon_timeout: rio_common::config::BoundedSecs::from_duration(
                crate::executor::DEFAULT_DAEMON_TIMEOUT,
            ),
            max_silent_time: std::time::Duration::ZERO,
            idle_timeout: std::time::Duration::from_secs(120),
        }
    }
}

impl rio_common::config::ValidateConfig for Config {
    fn validate(&self) -> anyhow::Result<()> {
        self.scheduler
            .ensure_required("scheduler.addr", "builder")?;
        self.store.ensure_required("store.addr", "builder")?;
        Ok(())
    }
}

impl rio_common::server::HasCommonConfig for Config {
    fn common(&self) -> &rio_common::config::CommonConfig {
        &self.common
    }
    fn metric_labels(&self) -> Vec<(&'static str, String)> {
        // Fetcher pods share this binary. Without a role label, both
        // export identical rio_builder_* metrics — Prometheus can't
        // tell them apart.
        let role = match self.executor_kind {
            ExecutorKind::Builder => "builder",
            ExecutorKind::Fetcher => "fetcher",
        };
        vec![("role", role.into())]
    }
}

#[derive(Parser, Serialize, Default)]
#[command(
    name = "rio-builder",
    about = "Build executor with FUSE store for rio-build"
)]
/// Command-line arguments (clap). Every field shadows a `Config` knob;
/// CLI wins over file/env at merge time.
pub struct CliArgs {
    /// Executor ID (defaults to hostname)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    executor_id: Option<String>,

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

    /// rio-mountd UDS socket path
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    mountd_socket: Option<PathBuf>,

    /// Per-build castore-FUSE mountpoint root (mountd-owned)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    castore_dir: Option<PathBuf>,

    /// Number of FUSE threads
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    fuse_threads: Option<u32>,

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

    /// Daemon build timeout seconds (default: 7200)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon_timeout_secs: Option<u64>,
}

/// Detect the system architecture (e.g. "x86_64-linux").
pub fn detect_system() -> String {
    nix_system(std::env::consts::ARCH, std::env::consts::OS)
}

/// Map Rust `std::env::consts::{ARCH,OS}` to a Nix system double.
/// Factored out so the mapping is unit-testable (consts can't be
/// overridden at runtime).
pub(crate) fn nix_system(arch: &str, os: &str) -> String {
    // Map Rust arch/OS names to Nix system names.
    let nix_arch = match arch {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        "x86" => "i686",
        other => other,
    };
    let nix_os = match os {
        "macos" => "darwin",
        other => other,
    };
    format!("{nix_arch}-{nix_os}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression guard against silent default drift. The castore dirs
    /// and the mountd socket MUST stay in lock-step with rio-mountd's
    /// CLI defaults (`bin/rio-mountd.rs`) — a drift means the builder
    /// looks for the mount/caches where the daemon never put them.
    #[test]
    fn config_defaults_are_stable() {
        let d = Config::default();
        assert!(
            d.executor_id.is_empty(),
            "executor_id auto-detects via hostname"
        );
        assert!(d.scheduler.addr.is_empty(), "required, no default");
        assert!(d.store.addr.is_empty(), "required, no default");
        assert!(d.systems.is_empty(), "systems auto-detect");
        assert!(d.features.is_empty(), "features empty by default");
        assert_eq!(d.mountd_socket, PathBuf::from("/run/rio-mountd.sock"));
        assert_eq!(d.castore_dir, PathBuf::from("/var/rio/castore"));
        assert_eq!(d.staging_dir, PathBuf::from("/var/rio/staging"));
        assert_eq!(d.cache_dir, PathBuf::from("/var/rio/cache"));
        assert_eq!(d.chunks_dir, PathBuf::from("/var/rio/chunks"));
        assert_eq!(
            d.dag_prefetch_timeout,
            std::time::Duration::from_secs(30),
            "DAG prefetch budget: expiry is an infra-retry, never a wedged mount"
        );
        assert_eq!(
            d.stream_threshold,
            8 * 1024 * 1024,
            "ADR-022 §2.8 STREAM_THRESHOLD default"
        );
        assert_eq!(d.max_backing_ids, 4096, "ADR-022 implementation-plan value");
        assert!(
            !d.disable_passthrough,
            "passthrough is the default data path; disabling it is a debug-only \
             escape hatch (~2× per-build latency)"
        );
        assert_eq!(d.fuse_threads, 4);
        assert_eq!(
            d.fuse_fetch_timeout,
            std::time::Duration::from_secs(60),
            "open() fetch timeout: 60s NOT 300s (GRPC_STREAM_TIMEOUT). \
             The fetch blocks a fuser callback thread, and the circuit \
             breaker is tuned around 60s × 5 failures = 300s to open. \
             I-178: this is the BASE timeout; the opener uses \
             jit_fetch_timeout(this, size) per file so large inputs get \
             a size-proportional budget."
        );
        assert_eq!(d.overlay_base_dir, PathBuf::from("/var/rio/overlays"));
        assert_eq!(d.common.metrics_addr.to_string(), "[::]:9093");
        assert_eq!(d.health_addr.to_string(), "[::]:9193");
        assert_eq!(d.log_rate_limit, 250_000);
        assert_eq!(d.log_size_limit, 100 * 1024 * 1024);
    }

    #[test]
    fn cli_args_parse_help() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert();
    }

    // Jailed standing-guard tests — see rio-test-support/src/config.rs.
    // When you add Config.newfield: ADD IT to both assert blocks below.

    rio_test_support::jail_roundtrip!(
        "builder",
        r#"
        disable_passthrough = true
        fuse_fetch_timeout_secs = 222
        dag_prefetch_timeout_secs = 11
        stream_threshold = 1048576
        systems = ["x86_64-linux", "aarch64-linux"]
        "#,
        |cfg: Config| {
            assert!(
                cfg.disable_passthrough,
                "TOML scalar must override the compiled default of false"
            );
            assert_eq!(cfg.fuse_fetch_timeout, std::time::Duration::from_secs(222));
            assert_eq!(cfg.dag_prefetch_timeout, std::time::Duration::from_secs(11));
            assert_eq!(cfg.stream_threshold, 1048576);
            assert_eq!(cfg.systems, vec!["x86_64-linux", "aarch64-linux"]);
        }
    );

    rio_test_support::jail_defaults!("builder", "", |cfg: Config| {
        assert!(cfg.scheduler.balance_host.is_none());
        assert_eq!(cfg.executor_kind, ExecutorKind::Builder);
        assert!(cfg.systems.is_empty());
        assert!(cfg.features.is_empty());
        // Non-trivial defaults must survive the compiled-defaults →
        // TOML merge: a near-empty TOML must not zero them out (serde's
        // numeric default would silently disable streaming-open and
        // every backing registration).
        assert_eq!(cfg.mountd_socket, PathBuf::from("/run/rio-mountd.sock"));
        assert_eq!(cfg.stream_threshold, 8 * 1024 * 1024);
        assert_eq!(cfg.max_backing_ids, 4096);
        assert_eq!(cfg.dag_prefetch_timeout, std::time::Duration::from_secs(30));
        assert_eq!(cfg.fuse_fetch_timeout, std::time::Duration::from_secs(60));
    });

    /// `detect_system()`'s contract is "Nix-style system double". Rust's
    /// `std::env::consts::OS` is `"macos"` on Darwin; passing it through
    /// unchanged would advertise `"aarch64-macos"` and make
    /// `validate_host_arch` reject correct `RIO_SYSTEMS=aarch64-darwin`.
    #[test]
    fn detect_system_maps_nix_conventions() {
        assert_eq!(nix_system("x86_64", "linux"), "x86_64-linux");
        assert_eq!(nix_system("aarch64", "linux"), "aarch64-linux");
        assert_eq!(nix_system("x86", "linux"), "i686-linux");
        assert_eq!(nix_system("aarch64", "macos"), "aarch64-darwin");
        assert_eq!(nix_system("x86_64", "macos"), "x86_64-darwin");
    }
}
