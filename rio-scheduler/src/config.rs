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
    /// Retention for terminal `drv_executions` lifecycle rows, in days.
    /// The scheduler's `gc_exec_rows` pass deletes a row only when it is
    /// terminal, has no active assignment, is referenced by no
    /// `drv_attempts` ledger row, AND is older than this — the
    /// kernel-documented conjunction (`exec_row_sweep_eligible`). The
    /// store's log TTL sweep deletes the row's log artifacts on its own
    /// `log_retention_days` clock but never the row itself
    /// (`store.log.sweep-ownership`). Validate: ≥ 1.
    pub exec_retention_days: u32,
    /// How long past an open pull-mode attempt's intent deadline the
    /// establishment sweep waits for a terminal report before
    /// establishing the attempt as an unreported executor crash
    /// (pull-mode dispatch only; stream-mode attempts keep the as-built
    /// correlation machinery). Provisional default 120 s (about two
    /// controller ticks plus Job terminal-observation margin);
    /// re-baselined at deployment time against the controller's
    /// terminal-report latency histogram.
    /// Env: `RIO_ESTABLISHMENT_REPORT_SLACK_SECS`.
    #[serde(
        rename = "establishment_report_slack_secs",
        with = "rio_common::config::secs"
    )]
    #[schemars(with = "u64")]
    pub establishment_report_slack: std::time::Duration,
    /// I-204: `requiredSystemFeatures` values that are capability HINTS,
    /// not hardware gates. Stripped from each derivation at DAG-insert so
    /// they don't drive pool spawn or block dispatch. nixpkgs convention:
    /// `big-parallel`, `benchmark`. Helm sets via `scheduler.softFeatures`.
    pub soft_features: Vec<String>,
    /// HMAC key file for signing assignment tokens. The store
    /// verifies on PutPath with the SAME key. Unset = unsigned
    /// tokens (dev mode). Generate: `openssl rand -out /path 32`.
    pub hmac_key_path: Option<std::path::PathBuf>,
    /// HMAC key file for signing `x-rio-service-token` (SEPARATE from
    /// `hmac_key_path`). The scheduler mints `ServiceClaims { caller:
    /// "rio-scheduler" }` so the store honours `x-rio-probe-tenant-id`
    /// on dispatch-time `FindMissingPaths`/`QueryPathInfo` —
    /// `r[sched.dispatch.fod-substitute]`. Unset = dispatch-time
    /// substitution probe disabled (falls back to local-presence-only).
    pub service_hmac_key_path: Option<std::path::PathBuf>,
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
    /// Substitution-replacement (design §8): store-owned
    /// materialization jobs — the only substitution path since the
    /// Phase D′ flag collapse retired the walk-era machinery.
    /// `[materialization]` table in scheduler.toml. Env:
    /// `RIO_MATERIALIZATION__*` (nested keys — double underscore).
    pub materialization: MaterializationConfig,
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

/// Substitution-replacement campaign (design §8): store-owned
/// materialization jobs — THE substitution mechanism (unconditional
/// since the substitution-replacement cutover; the coexistence flag
/// and the chart AND-guard died with it).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct MaterializationConfig {
    /// Budget: `materialization_infra` rows (worker-reported AND
    /// establishment-written — OQ1 amendment 1) per job before the job
    /// parks (design §2.5). Never causes a fail-fast.
    pub max_attempts: u32,
    /// Park backoff base (seconds) after budget exhaustion.
    pub park_backoff_base_secs: u64,
    /// Park backoff cap (seconds).
    pub park_backoff_cap_secs: u64,
    /// Item T conversion-strictness, worker-charge half (harden-store
    /// reconciliation memo §6.2(b)): `false` = off (the shipped
    /// default; walk-equivalence preserved). When ON, the PD-20 park
    /// re-evaluation converts a parked Vouched/Pending job from-source
    /// ONLY when worker-reported `materialization_infra` charges alone
    /// exhaust `max_attempts`; Scheduler-party establishment
    /// ("unreported") charges still count toward PARKING (OQ1
    /// amendment 1 — unchanged; since the 2026-06-03 establishment-park
    /// reversal they park party-blind at the budget) but no longer
    /// authorize conversion. The job stays parked (armed: park-expiry
    /// re-claim; further worker charges accrue across cycles) until
    /// they do. KNOB-ON POPULATION NOTE (the reversal's consequence):
    /// an establishment-only-parked job — zero worker charges, the
    /// never-reporting-replica crash-loop — can NEVER satisfy this
    /// gate; with the knob ON such jobs stay parked until a worker
    /// charge lands or the job is cancelled (deliberate: a from-source
    /// conversion on zero worker evidence is exactly what the knob
    /// exists to forbid; the population is visible as parked jobs
    /// whose ledger rows are all establishment-written). Flipping
    /// the default ON later is an operational act gated on
    /// `RioSchedulerMaterializationConversions` alert evidence.
    pub conversion_requires_worker_charge: bool,
    /// Item T conversion-strictness, dwell half: minimum seconds since
    /// the job's MOST RECENT park began (re-park restarts the clock;
    /// durable `materialization_jobs.park_began_at`, failover-exact)
    /// before PD-20 may convert. `0` = off (knob disabled — the
    /// shipped default). The re-evaluation only runs inside the park
    /// window, so the largest satisfiable dwell is
    /// [`MaterializationConfig::max_satisfiable_dwell_secs`]
    /// (`park_backoff_cap_secs - 1`): a visited job has
    /// `parked_until > now`, hence `elapsed < window <= cap`, hence
    /// `elapsed.as_secs() <= cap - 1` — dwell == cap (bug_088's
    /// off-by-one) or beyond can never be met and is rejected at
    /// config validation.
    pub conversion_min_park_dwell_secs: u64,
    /// The deadline a MATERIALIZATION attempt is minted under
    /// (seconds): the establishment sweep establishes an unreported
    /// store claim this long (plus `establishment_report_slack_secs`)
    /// after the mint. Materialization never runs under a build pod's
    /// `activeDeadlineSeconds`, so the build solve is the wrong anchor
    /// (bug_075's secondary effect: establishment waited
    /// build-deadline+slack). Large closures on slow upstreams may
    /// need this raised per-deployment. Must be ≥ 1 — `0` would
    /// establish every claim on its first sweep tick.
    pub attempt_deadline_secs: u64,
}

impl MaterializationConfig {
    /// The largest `conversion_min_park_dwell_secs` a PD-20
    /// re-evaluation can ever satisfy (bug_088). Derivation: the
    /// re-evaluation only visits jobs whose park is still open
    /// (`parked_until > now`), so the dwell clock reads
    /// `elapsed < window <= park_backoff_cap_secs`, and
    /// `elapsed.as_secs()` (truncating) is at most `cap - 1`. The
    /// validator and the gate site both consume THIS helper — one
    /// boundary, two readers, no drift.
    pub fn max_satisfiable_dwell_secs(&self) -> u64 {
        self.park_backoff_cap_secs.saturating_sub(1)
    }
}

impl Default for MaterializationConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            park_backoff_base_secs: 30,
            park_backoff_cap_secs: 900,
            conversion_requires_worker_charge: false,
            conversion_min_park_dwell_secs: 0,
            attempt_deadline_secs: 3600,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: rio_common::default_addr(9001),
            store: rio_common::config::UpstreamAddrs::with_port(9002),
            database_url: String::new(),
            common: rio_common::config::CommonConfig::new(9091),
            tick_interval: std::time::Duration::from_secs(10),
            exec_retention_days: 30,
            establishment_report_slack: std::time::Duration::from_secs(120),
            soft_features: Vec::new(),
            hmac_key_path: None,
            service_hmac_key_path: None,
            jwt: rio_common::config::JwtConfig::default(),
            lease_name: None,
            lease_namespace: None,
            poison: crate::PoisonConfig::default(),
            retry: crate::RetryPolicy::default(),
            materialization: MaterializationConfig::default(),
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
        // 0 would make every terminal+unreferenced execution row
        // instantly collectable — deleting kind/attribution context
        // out from under operators inspecting same-day builds, and
        // racing the controller's terminal-observation margin.
        anyhow::ensure!(
            cfg.exec_retention_days >= 1,
            "exec_retention_days must be >= 1 (got {})",
            cfg.exec_retention_days
        );
        // r[impl sched.config.slack-floor]
        // Cross-component timing contract (C2/077 gap 3): the
        // controller's wedge clustering needs expired attempts
        // observable in the open view for its grace + two ticks before
        // the establishment sweep removes them. The controller
        // const-asserts its side against the same shared constant.
        anyhow::ensure!(
            cfg.establishment_report_slack.as_secs()
                >= rio_common::limits::MIN_ESTABLISHMENT_REPORT_SLACK_SECS,
            "establishment_report_slack_secs must be >= {} \
             (rio_common::limits::MIN_ESTABLISHMENT_REPORT_SLACK_SECS — the controller's \
             wedge observation grace + two reconcile ticks must fit inside the slack, \
             or node-wedge clustering goes blind)",
            rio_common::limits::MIN_ESTABLISHMENT_REPORT_SLACK_SECS
        );
        // r[impl sched.retry.per-executor-budget+4]
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
        // The PD-20 park re-evaluation only visits jobs whose park
        // window is still open (`park_until` in the future), so a
        // conversion dwell longer than the largest possible park
        // backoff can NEVER be satisfied at a re-evaluation — the
        // dwell knob would be a silent conversion-never, which is not
        // a posture the Item T record orders (the record's dwell is a
        // MINIMUM, implying eventual conversion). Fail fast at load.
        // bug_088: the boundary is cap - 1, not cap — a visited job's
        // dwell clock truncates below the cap (see
        // MaterializationConfig::max_satisfiable_dwell_secs, the one
        // shared boundary).
        anyhow::ensure!(
            cfg.materialization.conversion_min_park_dwell_secs
                <= cfg.materialization.max_satisfiable_dwell_secs(),
            "materialization.conversion_min_park_dwell_secs must be <= \
         park_backoff_cap_secs - 1 (got dwell={}, max satisfiable={}); a \
         dwell at or beyond the park cap can never be met at a PD-20 \
         re-evaluation and silently disables conversion entirely",
            cfg.materialization.conversion_min_park_dwell_secs,
            cfg.materialization.max_satisfiable_dwell_secs()
        );
        // attempt_deadline_secs = 0 would establish every store claim
        // on its first sweep visit (deadline already past at mint) —
        // the unvalidated-degenerate-knob class (082's sibling).
        anyhow::ensure!(
            cfg.materialization.attempt_deadline_secs >= 1,
            "materialization.attempt_deadline_secs must be >= 1, got 0 (a zero deadline establishes every claim on its first sweep tick)"
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
        // Substitution-replacement materialization budget/backoff
        // bounds. `max_attempts = 0` would park every job before its
        // first attempt (the budget check is `count >= max`), and a
        // zero backoff base makes a parked job instantly re-claimable
        // (park becomes a hot-loop). The cap must not sit below the
        // base — backoff_duration-style curves degenerate otherwise.
        // All three checked at config load so a bad value fails at
        // startup, not mid-flight (a discipline kept from the
        // Phase A/B staged rollout, which validated even while the
        // since-removed cutover flag was off).
        anyhow::ensure!(
            cfg.materialization.max_attempts >= 1,
            "materialization.max_attempts must be >= 1, got {} \
         (0 would park every job before its first attempt)",
            cfg.materialization.max_attempts
        );
        anyhow::ensure!(
            cfg.materialization.park_backoff_base_secs >= 1,
            "materialization.park_backoff_base_secs must be >= 1, got {} \
         (0 makes a parked job instantly re-claimable — park becomes a hot-loop)",
            cfg.materialization.park_backoff_base_secs
        );
        anyhow::ensure!(
            cfg.materialization.park_backoff_cap_secs >= cfg.materialization.park_backoff_base_secs,
            "materialization.park_backoff_cap_secs must be >= park_backoff_base_secs \
         (got cap={}, base={})",
            cfg.materialization.park_backoff_cap_secs,
            cfg.materialization.park_backoff_base_secs
        );
        // §13c-3: pass-1 only — the catalog is fetched in main.rs
        // AFTER config load; `validate_resolved()` runs there.
        cfg.sla.validate_shape()?;
        Ok(())
    }
}

rio_common::impl_has_common_config!(Config);

#[cfg(test)]
mod tests {
    /// C2/077 gap 3: the controller's wedge observation grace + two
    /// reconcile ticks must fit inside the establishment report slack —
    /// otherwise the sweep removes expired attempts from the open view
    /// before the wedge clustering can observe them. The floor is the
    /// shared constant; a config below it must fail load (and the
    /// boundary value must not).
    // r[verify sched.config.slack-floor]
    #[test]
    fn establishment_slack_below_shared_floor_fails_validation() {
        use rio_common::config::ValidateConfig;
        // Satisfy the ensures ordered before the slack floor so the
        // first failure (if any) is the one under test.
        let mut cfg = super::Config {
            database_url: "postgres://test".into(),
            ..Default::default()
        };
        cfg.store.addr = "store:9002".into();
        cfg.establishment_report_slack = std::time::Duration::from_secs(30);
        let err = cfg
            .validate()
            .expect_err("slack below the shared floor must fail fast at load")
            .to_string();
        assert!(
            err.contains("establishment_report_slack_secs must be >="),
            "the failure must be the slack floor, got: {err}"
        );
        // Boundary: exactly the floor passes the slack check (later,
        // unrelated ensures may still reject the skeletal fixture).
        cfg.establishment_report_slack =
            std::time::Duration::from_secs(rio_common::limits::MIN_ESTABLISHMENT_REPORT_SLACK_SECS);
        if let Err(e) = cfg.validate() {
            assert!(
                !e.to_string().contains("establishment_report_slack_secs"),
                "the floor boundary must pass the slack check, got: {e:#}"
            );
        }
    }
}
