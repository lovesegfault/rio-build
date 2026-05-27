//! `rio-scheduler` binary configuration: layered-config-loaded `Config`
//! struct, clap `CliArgs` overlay, and the `ValidateConfig` bounds
//! checks. Extracted from `main.rs` so config parsing/validation is
//! unit-testable without the full bootstrap (PG connect, gRPC bind,
//! actor spawn).

use clap::Parser;
use serde::{Deserialize, Serialize};

// Two-struct config split — see rio-common/src/config.rs for rationale.

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Config {
    /// gRPC listen address for SchedulerService + ExecutorService.
    pub listen_addr: std::net::SocketAddr,
    /// rio-store upstream. Env: `RIO_STORE__ADDR`. Scheduler uses
    /// `connect_store_lazy` (re-resolves on reconnect) so
    /// `balance_host` is unused — the lazy channel follows the
    /// ClusterIP Service's current endpoint without an explicit p2c.
    pub store: rio_common::config::UpstreamAddrs,
    /// PostgreSQL connection URL. Required.
    pub database_url: String,
    #[serde(flatten)]
    pub common: rio_common::config::CommonConfig,
    /// Tick interval (seconds) for scheduler housekeeping.
    #[serde(rename = "tick_interval_secs", with = "rio_common::config::secs")]
    #[schemars(with = "u64")]
    pub tick_interval: std::time::Duration,
    /// S3 bucket for build-log flush. `None` = flush disabled.
    /// Env: `RIO_LOG_S3_BUCKET`. Wired into LogFlusher in main().
    pub log_s3_bucket: Option<String>,
    /// Build log retention in days. The `LogFlusher`'s GC tick deletes
    /// `drv_logs` rows (and their `.log.zst` + `.partial.log.zst` S3
    /// blobs) older than this. Default 30. Set high to effectively
    /// disable; do NOT set to 0 — that's "delete every log on every
    /// sweep," not "disable." The age filter already excludes active
    /// builds (no build runs 30 days; the daemon timeout is ~2h), so
    /// there's no `is_complete` discriminator. Env:
    /// `RIO_LOG_RETENTION_DAYS`. Helm: `scheduler.logRetentionDays`.
    pub log_retention_days: u32,
    /// I-204: `requiredSystemFeatures` values that are capability HINTS,
    /// not hardware gates. Stripped from each derivation at DAG-insert so
    /// they don't drive pool spawn or block dispatch. nixpkgs convention:
    /// `big-parallel`, `benchmark`. Helm sets via `scheduler.softFeatures`.
    pub soft_features: Vec<String>,
    /// HMAC key file for signing assignment tokens. The store
    /// verifies on PutPath with the SAME key. Unset = unsigned
    /// tokens (dev mode). Generate: `openssl rand -out /path 32`.
    pub hmac_key_path: Option<std::path::PathBuf>,
    /// Cap (seconds) on the assignment-token TTL minted at dispatch.
    /// The TTL is 2× the client-supplied `build_timeout` clamped to
    /// [4 h, this cap]; `build_timeout = 0` ("unlimited") gets the cap
    /// — `r[common.hmac.expiry-cap]`. Default 172800 (48 h): 2× the
    /// executor-pod `activeDeadlineSeconds` ceiling (24 h), so the
    /// token comfortably outlives the longest k8s build plus its
    /// upload window, and covers a ~24 h standalone build with the
    /// same 2× slack. Raise only if standalone builds legitimately run
    /// longer than 24 h; must be ≥ 14400 (the 4 h floor). Env:
    /// `RIO_ASSIGNMENT_TOKEN_TTL_CAP_SECS`.
    pub assignment_token_ttl_cap_secs: u64,
    /// HMAC key file for signing `x-rio-service-token` (SEPARATE from
    /// `hmac_key_path`). The scheduler mints `ServiceClaims { caller:
    /// "rio-scheduler" }` so the store honours `x-rio-probe-tenant-id`
    /// on dispatch-time `FindMissingPaths`/`QueryPathInfo` —
    /// `r[sched.dispatch.fod-substitute]`. Unset = dispatch-time
    /// substitution probe disabled (falls back to local-presence-only).
    pub service_hmac_key_path: Option<std::path::PathBuf>,
    /// HMAC key file for signing rio-mountd Mount-admission tokens
    /// (ADR-022 §P0559). SEPARATE from `hmac_key_path`: the matching
    /// verifier key is mounted into the rio-mountd DaemonSet on every
    /// builder node, and a node compromise must not yield a key the
    /// store trusts. When set, dispatch mints a `MountdClaims{aud,
    /// build_id, tenant, expiry}` token into
    /// `WorkAssignment.mountd_token` with the same TTL as the
    /// assignment token; the builder presents it in the mountd
    /// `Mount{}` frame so `hostUsers: false` executor pods (which
    /// cannot present the host rio-builder gid) are admitted. Unset =
    /// no mountd token minted (dev/standalone — mountd's gid gate is
    /// the only admission path). Env: `RIO_MOUNTD_HMAC_KEY_PATH`.
    /// Helm: `mountdHmac.secretName`. Superseded by
    /// `mountd_signing_key_path` (the never-provisioned symmetric arm
    /// is deleted in the final ADR-022 §P0590 phase); ignored with a
    /// warning when both are set.
    pub mountd_hmac_key_path: Option<std::path::PathBuf>,
    /// Ed25519 signing key file for rio-mountd Mount-admission tokens
    /// (ADR-022 mount-admission credentials, §P0590). Format:
    /// `rio-mountd-<n>:base64(64-byte ed25519 keypair)` (32-byte seed
    /// also accepted). The scheduler is the ONLY holder of this key;
    /// builder nodes hold public trust roots only, so node compromise
    /// yields no minting ability. When set, dispatch signs an
    /// `rmt2.<claims>.<signature>` token into
    /// `WorkAssignment.mountd_token` (same TTL as the assignment
    /// token) whose claims also name the target node — see
    /// `mountd_node_binding`. Takes precedence over
    /// `mountd_hmac_key_path`. Unset = no rmt2 minting (the legacy
    /// HMAC knob, if set, still mints; otherwise mountd admits by gid
    /// only). Env: `RIO_MOUNTD_SIGNING_KEY_PATH`. Helm:
    /// `mountdSigning.privateKeySecretName`.
    pub mountd_signing_key_path: Option<std::path::PathBuf>,
    /// What dispatch does when the target node for an rmt2
    /// Mount-admission token cannot be resolved (no controller-attested
    /// binding for the executor, no executor-reported node):
    /// `require` (default) defers that derivation one dispatch pass —
    /// strict mountds reject unbound tokens, so minting one would only
    /// convert a transient knowledge gap into a build failure;
    /// `prefer` mints the token without a node claim (operational
    /// escape hatch — node-checking mountds will reject it). Only
    /// consulted when `mountd_signing_key_path` is set. Env:
    /// `RIO_MOUNTD_NODE_BINDING`.
    pub mountd_node_binding: MountdNodeBinding,
    /// JWT verification. `key_path` → ConfigMap mount at
    /// `/etc/rio/jwt/ed25519_pubkey` (see helm jwt-pubkey-configmap.yaml).
    /// The gateway signs with the matching seed; scheduler verifies.
    /// Unset = interceptor inert (dev mode / pre-key-rotation-infra).
    /// SIGHUP reloads from the same path — kubelet remounts the
    /// ConfigMap on rotation, operator SIGHUPs the pod. Set via
    /// `RIO_JWT__KEY_PATH` (nested config key — double underscore).
    pub jwt: rio_common::config::JwtConfig,
    /// Kubernetes Lease name for leader election. `None` = non-K8s
    /// mode (single-scheduler; is_leader=true immediately, generation
    /// stays 1). Env: `RIO_LEASE_NAME`. See crate::lease.
    pub lease_name: Option<String>,
    /// Kubernetes namespace for the Lease. `None` = read from the
    /// in-cluster serviceaccount mount, fall back to "default".
    /// Env: `RIO_LEASE_NAMESPACE`. Ignored when `lease_name` is None.
    pub lease_namespace: Option<String>,
    /// Poison-detection thresholds. `[poison]` table in scheduler.toml.
    /// `r[sched.retry.per-executor-budget]` (scheduler.typ) specifies
    /// both this and `retry` below as TOML-configurable. P0219 shipped
    /// the structs + builders; this wires them. Default: 3 distinct
    /// workers must fail (matches the former `POISON_THRESHOLD` const).
    /// No CLI override — infrequently-tweaked deploy config.
    pub poison: crate::PoisonConfig,
    /// Per-worker retry backoff curve. `[retry]` table in scheduler.toml.
    /// Default: 2 retries, 5s→300s exponential with 20% jitter. No CLI
    /// override for the same reason as `poison`.
    pub retry: crate::RetryPolicy,
    /// In-flight detached substitute-fetch task bound
    /// (r[sched.substitute.detached+2]) — memory-safety only; per-replica
    /// throttling is `r[store.substitute.admission]`. Sizes
    /// `DagActor.substitute_sem`. Env: `RIO_SUBSTITUTE_MAX_CONCURRENT`
    /// (operator escape hatch — not chart-set). Default 256.
    #[serde(default = "default_substitute_concurrency")]
    pub substitute_max_concurrent: usize,
    /// gRPC-Web / CORS config for the dashboard SPA. `[dashboard]`
    /// table in scheduler.toml. Env: `RIO_DASHBOARD__*`.
    pub dashboard: DashboardConfig,
    /// ADR-023 SLA-driven sizing. `[sla]` table in scheduler.toml —
    /// mandatory (helm always renders it). No env override — structured
    /// config only. The defaults baseline (`Default for Config`) is
    /// [`crate::sla::config::SlaConfig::defaults_baseline`],
    /// which leaves `maxCores`/`maxMem`/`hwClasses` empty so a TOML
    /// that omits them is read as "unset" — the config layers merge
    /// per-key, so a populated baseline would mask the §13c-3 catalog
    /// derive.
    /// Validated via
    /// [`crate::sla::config::SlaConfig::validate_shape`] +
    /// [`crate::sla::config::SlaConfig::validate_resolved`].
    // Skipped from the docs schema: ADR-023 SLA config has its own
    // dedicated spec chapter and ~2.7KLoC of nested types (Tier,
    // ProbeShape, HwClassDef, Cell-keyed maps) that don't render
    // usefully into the per-key table that flows into configuration.typ.
    #[schemars(skip)]
    pub sla: crate::sla::config::SlaConfig,
    /// Permit a `[sla].reference_hw_class` change vs the value
    /// persisted in `sla_config_epoch` (M_058). DESTRUCTIVE — resets
    /// `build_samples`, `hw_perf_samples`, and this cluster's
    /// `sla_ema_state`. CLI-only (`--allow-reference-change`); never
    /// set from TOML/env so a stale flag can't survive a rollout.
    pub allow_reference_change: bool,
}

/// Dispatch behavior when the target node for an rmt2 Mount-admission
/// token cannot be resolved (ADR-022 §P0590 node-scoped claims).
/// `require` is the fail-closed default; `prefer` is the documented
/// operational escape hatch that mints unbound tokens strict mountds
/// reject.
// r[impl builder.mountd.token-node-scoped]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum MountdNodeBinding {
    /// Defer the derivation one dispatch pass until placement resolves.
    #[default]
    Require,
    /// Mint the token without a node claim.
    Prefer,
}

/// Dashboard browser-facing settings. The scheduler serves gRPC-Web
/// natively on its main port (D3) so the ingress is a plain HTTP
/// router — CORS therefore lives here, not in a proxy CRD.
// r[impl dash.envoy.grpc-web-translate+3]
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct DashboardConfig {
    /// Comma-separated CORS allowed origins for gRPC-Web requests.
    /// Env: `RIO_DASHBOARD__CORS_ALLOW_ORIGINS`. The dashboard nginx
    /// Service is the only legitimate browser origin in-cluster;
    /// external access (Ingress/LoadBalancer) appends its public
    /// hostname via helm `dashboard.cors.allowOrigins`. Comma-joined
    /// string (not `Vec<String>`) so the RIO_ env layer works
    /// without a custom split — helm renders `| join ","`.
    pub cors_allow_origins: String,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            cors_allow_origins: "http://rio-dashboard.rio-system.svc.cluster.local".into(),
        }
    }
}

fn default_substitute_concurrency() -> usize {
    crate::DEFAULT_SUBSTITUTE_CONCURRENCY
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
            log_retention_days: 30,
            soft_features: Vec::new(),
            hmac_key_path: None,
            assignment_token_ttl_cap_secs: crate::DEFAULT_ASSIGNMENT_TOKEN_TTL_CAP_SECS,
            service_hmac_key_path: None,
            mountd_hmac_key_path: None,
            mountd_signing_key_path: None,
            mountd_node_binding: MountdNodeBinding::default(),
            jwt: rio_common::config::JwtConfig::default(),
            lease_name: None,
            lease_namespace: None,
            poison: crate::PoisonConfig::default(),
            retry: crate::RetryPolicy::default(),
            substitute_max_concurrent: default_substitute_concurrency(),
            dashboard: DashboardConfig::default(),
            sla: crate::sla::config::SlaConfig::defaults_baseline(),
            allow_reference_change: false,
        }
    }
}

#[derive(Parser, Serialize, Default)]
#[command(
    name = "rio-scheduler",
    about = "DAG-aware build scheduler for rio-build"
)]
pub struct CliArgs {
    /// gRPC listen address for SchedulerService + ExecutorService
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen_addr: Option<std::net::SocketAddr>,

    /// PostgreSQL connection URL
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_url: Option<String>,

    /// Prometheus metrics listen address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics_addr: Option<std::net::SocketAddr>,

    /// Tick interval for housekeeping (seconds)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tick_interval_secs: Option<u64>,

    /// S3 bucket for build-log zstd flush (unset = flush disabled)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_s3_bucket: Option<String>,

    /// Permit an `[sla].reference_hw_class` change vs persisted —
    /// DESTRUCTIVE: resets build_samples / hw_perf_samples /
    /// sla_ema_state. See `crate::sla::check_reference_epoch`.
    #[arg(long)]
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub allow_reference_change: bool,
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
        // `LogFlusher::sweep_expired_logs` deletes `WHERE started_at <
        // now() - $days * interval '1 day'`. `days=0` collapses the
        // cutoff to `now()`, which is true for every row (`started_at`
        // is the dispatch instant, always in the past) — the sweep
        // would delete EVERY log on EVERY tick. That's a silent
        // data-loss footgun, not "GC disabled." Disable by setting it
        // high (3650 = ~10y), not zero.
        anyhow::ensure!(
            cfg.log_retention_days > 0,
            "log_retention_days must be positive, got 0 \
             (would delete every log on every GC sweep — to disable, \
             set it high, e.g. 3650)"
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
        // `actor/completion.rs` resets the per-executor `infra_count` when
        // `last.elapsed().as_secs_f64() > infra_retry_window_secs`. `as_secs_f64()`
        // is always >= 0.0, so a negative window (or 0.0) makes the comparison
        // true on EVERY non-floor-counted infra failure → `infra_count` never
        // accumulates → `max_infra_retries` cap never reached → the documented
        // 9748-dispatch hot-loop (state/executor.rs) re-enabled silently. NaN
        // makes the comparison false (also wrong: window-reset disabled).
        // Tests that want "window-reset disabled" should use a large finite
        // value (1e9 or `RetryPolicy::default().infra_retry_window_secs`), not 0.
        anyhow::ensure!(
            cfg.retry.infra_retry_window_secs.is_finite()
                && cfg.retry.infra_retry_window_secs > 0.0,
            "retry.infra_retry_window_secs must be finite and positive, got {} \
         (<=0 makes the elapsed-reset comparison always true → max_infra_retries \
         cap never reached → infra-failure hot-loop)",
            cfg.retry.infra_retry_window_secs
        );
        // r[impl common.hmac.expiry-cap]
        // Below the 4 h floor the dispatch-time clamp would silently
        // resolve every token TTL to the cap (the non-panicking
        // max/min ordering lets cap win), i.e. the operator's number
        // is ignored; 0 would mint already-expired tokens and every
        // castore read / upload would fail UNAUTHENTICATED at the
        // store. Reject at config load instead.
        anyhow::ensure!(
            cfg.assignment_token_ttl_cap_secs >= crate::ASSIGNMENT_TOKEN_TTL_FLOOR_SECS,
            "assignment_token_ttl_cap_secs must be >= {} (the 4h floor), got {} \
             (a smaller cap is silently ignored by the dispatch-time clamp; \
             0 would mint already-expired assignment tokens)",
            crate::ASSIGNMENT_TOKEN_TTL_FLOOR_SECS,
            cfg.assignment_token_ttl_cap_secs
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
        // §13c-3: pass-1 only — the catalog is fetched in main.rs
        // AFTER config load; `validate_resolved()` runs there.
        cfg.sla.validate_shape()?;
        Ok(())
    }
}

rio_common::impl_has_common_config!(Config);
