//! `rio-scheduler` binary configuration: figment-loaded `Config`
//! struct, clap `CliArgs` overlay, and the `ValidateConfig` bounds
//! checks. Extracted from `main.rs` so config parsing/validation is
//! unit-testable without the full bootstrap (PG connect, gRPC bind,
//! actor spawn).

use clap::Parser;
use serde::{Deserialize, Serialize};

// Two-struct config split — see rio-common/src/config.rs for rationale.

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Config {
    pub(super) listen_addr: std::net::SocketAddr,
    /// rio-store upstream. Env: `RIO_STORE__ADDR`. Scheduler uses
    /// `connect_store_lazy` (re-resolves on reconnect) so
    /// `balance_host` is unused — the lazy channel follows the
    /// ClusterIP Service's current endpoint without an explicit p2c.
    pub(super) store: rio_common::config::UpstreamAddrs,
    pub(super) database_url: String,
    #[serde(flatten)]
    pub(super) common: rio_common::config::CommonConfig,
    #[serde(rename = "tick_interval_secs", with = "rio_common::config::secs")]
    pub(super) tick_interval: std::time::Duration,
    /// S3 bucket for build-log flush. `None` = flush disabled.
    /// Env: `RIO_LOG_S3_BUCKET`. Wired into LogFlusher in main().
    pub(super) log_s3_bucket: Option<String>,
    pub(super) log_s3_prefix: String,
    /// I-204: `requiredSystemFeatures` values that are capability HINTS,
    /// not hardware gates. Stripped from each derivation at DAG-insert so
    /// they don't drive pool spawn or block dispatch. nixpkgs convention:
    /// `big-parallel`, `benchmark`. Helm sets via `scheduler.softFeatures`.
    pub(super) soft_features: Vec<String>,
    /// HMAC key file for signing assignment tokens. The store
    /// verifies on PutPath with the SAME key. Unset = unsigned
    /// tokens (dev mode). Generate: `openssl rand -out /path 32`.
    pub(super) hmac_key_path: Option<std::path::PathBuf>,
    /// HMAC key file for signing `x-rio-service-token` (SEPARATE from
    /// `hmac_key_path`). The scheduler mints `ServiceClaims { caller:
    /// "rio-scheduler" }` so the store honours `x-rio-probe-tenant-id`
    /// on dispatch-time `FindMissingPaths`/`QueryPathInfo` —
    /// `r[sched.dispatch.fod-substitute]`. Unset = dispatch-time
    /// substitution probe disabled (falls back to local-presence-only).
    pub(super) service_hmac_key_path: Option<std::path::PathBuf>,
    /// JWT verification. `key_path` → ConfigMap mount at
    /// `/etc/rio/jwt/ed25519_pubkey` (see helm jwt-pubkey-configmap.yaml).
    /// The gateway signs with the matching seed; scheduler verifies.
    /// Unset = interceptor inert (dev mode / pre-key-rotation-infra).
    /// SIGHUP reloads from the same path — kubelet remounts the
    /// ConfigMap on rotation, operator SIGHUPs the pod. Set via
    /// `RIO_JWT__KEY_PATH` (nested figment key — double underscore).
    pub(super) jwt: rio_common::config::JwtConfig,
    /// Kubernetes Lease name for leader election. `None` = non-K8s
    /// mode (single-scheduler; is_leader=true immediately, generation
    /// stays 1). Env: `RIO_LEASE_NAME`. See rio_scheduler::lease.
    pub(super) lease_name: Option<String>,
    /// Kubernetes namespace for the Lease. `None` = read from the
    /// in-cluster serviceaccount mount, fall back to "default".
    /// Env: `RIO_LEASE_NAMESPACE`. Ignored when `lease_name` is None.
    pub(super) lease_namespace: Option<String>,
    /// Poison-detection thresholds. `[poison]` table in scheduler.toml.
    /// `r[sched.retry.per-executor-budget]` (scheduler.md:110) specifies
    /// both this and `retry` below as TOML-configurable. P0219 shipped
    /// the structs + builders; this wires them. Default: 3 distinct
    /// workers must fail (matches the former `POISON_THRESHOLD` const).
    /// No CLI override — infrequently-tweaked deploy config.
    pub(super) poison: rio_scheduler::PoisonConfig,
    /// Per-worker retry backoff curve. `[retry]` table in scheduler.toml.
    /// Default: 2 retries, 5s→300s exponential with 20% jitter. No CLI
    /// override for the same reason as `poison`.
    pub(super) retry: rio_scheduler::RetryPolicy,
    /// Max concurrent detached substitute-fetch tasks
    /// (r[sched.substitute.detached]). Sizes `DagActor.substitute_sem`.
    /// Env: `RIO_SUBSTITUTE_MAX_CONCURRENT`. Default 256.
    #[serde(default = "default_substitute_concurrency")]
    pub(super) substitute_max_concurrent: usize,
    /// gRPC-Web / CORS config for the dashboard SPA. `[dashboard]`
    /// table in scheduler.toml. Env: `RIO_DASHBOARD__*`.
    pub(super) dashboard: DashboardConfig,
    /// ADR-023 SLA-driven sizing. `[sla]` table in scheduler.toml —
    /// mandatory (helm always renders it; absent → figment falls back
    /// to [`rio_scheduler::sla::config::SlaConfig::test_default`] via
    /// `Default for Config`). No env override — structured config
    /// only. Validated via
    /// [`rio_scheduler::sla::config::SlaConfig::validate`].
    pub(super) sla: rio_scheduler::sla::config::SlaConfig,
}

/// Dashboard browser-facing settings. The scheduler serves gRPC-Web
/// natively on its main port (D3) so the ingress is a plain HTTP
/// router — CORS therefore lives here, not in a proxy CRD.
// r[impl dash.envoy.grpc-web-translate+3]
#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct DashboardConfig {
    /// Comma-separated CORS allowed origins for gRPC-Web requests.
    /// Env: `RIO_DASHBOARD__CORS_ALLOW_ORIGINS`. The dashboard nginx
    /// Service is the only legitimate browser origin in-cluster;
    /// external access (Ingress/LoadBalancer) appends its public
    /// hostname via helm `dashboard.cors.allowOrigins`. Comma-joined
    /// string (not `Vec<String>`) so figment's env provider works
    /// without a custom split — helm renders `| join ","`.
    pub(super) cors_allow_origins: String,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            cors_allow_origins: "http://rio-dashboard.rio-system.svc.cluster.local".into(),
        }
    }
}

fn default_substitute_concurrency() -> usize {
    rio_scheduler::DEFAULT_SUBSTITUTE_CONCURRENCY
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: rio_common::default_addr(9001),
            store: rio_common::config::UpstreamAddrs::with_port(9002),
            database_url: String::new(),
            common: rio_common::config::CommonConfig::new(9091),
            tick_interval: std::time::Duration::from_secs(10),
            log_s3_bucket: None,
            log_s3_prefix: "logs".into(),
            soft_features: Vec::new(),
            hmac_key_path: None,
            service_hmac_key_path: None,
            jwt: rio_common::config::JwtConfig::default(),
            lease_name: None,
            lease_namespace: None,
            poison: rio_scheduler::PoisonConfig::default(),
            retry: rio_scheduler::RetryPolicy::default(),
            substitute_max_concurrent: default_substitute_concurrency(),
            dashboard: DashboardConfig::default(),
            sla: rio_scheduler::sla::config::SlaConfig::test_default(),
        }
    }
}

#[derive(Parser, Serialize, Default)]
#[command(
    name = "rio-scheduler",
    about = "DAG-aware build scheduler for rio-build"
)]
pub(super) struct CliArgs {
    /// gRPC listen address for SchedulerService + ExecutorService
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) listen_addr: Option<std::net::SocketAddr>,

    /// PostgreSQL connection URL
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) database_url: Option<String>,

    /// Prometheus metrics listen address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) metrics_addr: Option<std::net::SocketAddr>,

    /// Tick interval for housekeeping (seconds)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tick_interval_secs: Option<u64>,

    /// S3 bucket for build-log gzip flush (unset = flush disabled)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) log_s3_bucket: Option<String>,

    /// S3 key prefix for build logs
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) log_s3_prefix: Option<String>,
}

impl rio_common::config::ValidateConfig for Config {
    /// Bounds checks on operator-settable fields. Extracted from
    /// `main()` so the checks are unit-testable without spinning up
    /// the full scheduler (PG connect, gRPC bind, actor spawn). Every
    /// `ensure!` here documents a specific crash or silent-wrong
    /// failure that occurs AFTER startup if the bad value gets through
    /// — fail loud at config load instead of a rand panic on the third
    /// retry.
    ///
    /// When P0307 or a later plan wires a new field into
    /// `scheduler.toml`, add its bounds check here (and a rejection
    /// test in `tests.rs`). See the scrutiny recipe on
    /// [`rio_common::config::ValidateConfig`].
    fn validate(&self) -> anyhow::Result<()> {
        let cfg = self;
        use rio_common::config::ensure_required as required;
        cfg.store.ensure_required("store.addr", "scheduler")?;
        required(&cfg.database_url, "database_url", "scheduler")?;
        // `tokio::time::interval(ZERO)` panics. The tick loop feeds
        // `cfg.tick_interval` straight in — `tick_interval_secs = 0`
        // would crash the scheduler on spawn, AFTER migrations ran and
        // the gRPC port was already bound (a very late, very confusing
        // failure). Fail fast at config load.
        anyhow::ensure!(
            !cfg.tick_interval.is_zero(),
            "tick_interval_secs must be positive (tokio::time::interval panics on ZERO)"
        );
        // r[impl sched.retry.per-executor-budget]
        // `RetryPolicy::backoff_duration` computes
        // `random_range(-jf..=jf)` — rand panics if low > high, so jf < 0
        // crashes on the first retry. And jf > 1 makes `clamped * (1 - jf)`
        // negative, which `.max(0.0)` clamp silently turns
        // into ZERO backoff (retries become thrashing, not backoff). [0.0,
        // 1.0] inclusive — jf=0 means deterministic (no jitter), jf=1 means
        // backoff ∈ [0, 2*clamped] (wide but sane).
        anyhow::ensure!(
            (0.0..=1.0).contains(&cfg.retry.jitter_fraction),
            "retry.jitter_fraction must be in [0.0, 1.0], got {} \
         (negative panics rand::random_range; >1 silently zeros backoff)",
            cfg.retry.jitter_fraction
        );
        // `RetryPolicy::backoff_duration` computes
        // `base_secs * multiplier.powi(attempt)` then clamps `.max(0.0)`.
        // Negative base_secs → negative product → silently zero backoff
        // (retries thrash). NaN/inf → .max(0.0) swallows but the INTENT
        // was a real backoff. Require finite + positive — base_secs=0
        // is also nonsense (zero backoff by design defeats the policy).
        anyhow::ensure!(
            cfg.retry.backoff_base_secs.is_finite() && cfg.retry.backoff_base_secs > 0.0,
            "retry.backoff_base_secs must be finite and positive, got {} \
         (negative/NaN silently zero backoff via the Duration .max(0.0) clamp)",
            cfg.retry.backoff_base_secs
        );
        // `multiplier.powi(attempt)` — attempt grows, so
        // multiplier < 1.0 means backoff SHRINKS with retries (attempt=2
        // waits LESS than attempt=1). multiplier == 1.0 is valid (constant
        // backoff). NaN.powi() = NaN → zero via clamp. Require finite + ≥1.0.
        anyhow::ensure!(
            cfg.retry.backoff_multiplier.is_finite() && cfg.retry.backoff_multiplier >= 1.0,
            "retry.backoff_multiplier must be finite and >= 1.0, got {} \
         (<1.0 makes backoff SHRINK with retries; NaN silently zeros)",
            cfg.retry.backoff_multiplier
        );
        // `base.min(max_secs)` — negative max_secs caps
        // everything negative → zero via clamp. NaN.min(x) = NaN → zero.
        // Infinity is HANDLED (state::derivation::test_retry_backoff_infinity_clamped
        // proves the 1-year clamp catches it), but it's still operator-
        // error — no sane deployment wants unbounded backoff. Require
        // finite + positive, and >= base_secs (max < base is contradictory).
        anyhow::ensure!(
            cfg.retry.backoff_max_secs.is_finite()
                && cfg.retry.backoff_max_secs > 0.0
                && cfg.retry.backoff_max_secs >= cfg.retry.backoff_base_secs,
            "retry.backoff_max_secs must be finite, positive, and >= backoff_base_secs \
         (got max={}, base={})",
            cfg.retry.backoff_max_secs,
            cfg.retry.backoff_base_secs
        );
        // `PoisonConfig::is_poisoned` checks `count >= threshold` — threshold=0
        // makes `0 >= 0` vacuously true at DAG-merge time, before any dispatch.
        // Every derivation instantly poisons. threshold=1 is the practical
        // minimum (poison-on-first-failure — aggressive but valid for single-
        // worker dev deployments with require_distinct_workers=false).
        anyhow::ensure!(
            cfg.poison.threshold > 0,
            "poison.threshold must be positive, got {} \
         (threshold=0 means is_poisoned() is always true — \
         every derivation poisons immediately)",
            cfg.poison.threshold
        );
        cfg.sla.validate()?;
        Ok(())
    }
}

rio_common::impl_has_common_config!(Config);
