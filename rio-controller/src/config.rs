//! `rio-controller` binary configuration: layered-config-loaded `Config`
//! struct, clap `CliArgs` overlay, and the `ValidateConfig` bounds
//! checks. Extracted from `main.rs` so `tests/config_schema.rs` can
//! snapshot `schema_for!(Config)` into the committed
//! `tests/fixtures/config-schema.json` that `xtask regen docs-data`
//! reads.

use clap::Parser;
use serde::{Deserialize, Serialize};

use crate::reconcilers::nodeclaim_pool::NodeClaimPoolConfig;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Config {
    /// rio-scheduler upstream. Env: `RIO_SCHEDULER__ADDR` /
    /// `__BALANCE_HOST` / `__BALANCE_PORT`. `balance_host` used two
    /// ways: (1) injected into worker pods as
    /// `RIO_SCHEDULER__BALANCE_HOST`; (2) THIS process's autoscaler
    /// uses it for leader-aware ClusterStatus polling. `None` →
    /// single-channel via `addr` (ClusterIP — round-robins to the
    /// standby ~50% of the time with replicas=2).
    pub scheduler: rio_common::config::UpstreamAddrs,
    /// rio-store upstream. Env: `RIO_STORE__ADDR` / `__BALANCE_HOST`
    /// / `__BALANCE_PORT`. Injected into worker pod containers by
    /// the Pool reconciler. I-077: balance host needed so
    /// scaling rio-store 1→4 actually spreads load.
    pub store: rio_common::config::UpstreamAddrs,
    #[serde(flatten)]
    pub common: rio_common::config::CommonConfig,
    /// HTTP /healthz listen address. K8s livenessProbe hits this.
    pub health_addr: std::net::SocketAddr,
    /// GC cron interval (hours). 0 = disabled (reconciler not
    /// spawned). The cron calls StoreAdminService.TriggerGC with
    /// default params (dry_run=false, force=false, store's
    /// `DEFAULT_GC_GRACE_HOURS` grace). `store_addr` is the connect
    /// target — StoreAdminService
    /// is hosted on the store's gRPC port alongside StoreService.
    pub gc_interval_hours: u64,
    /// ADR-023 §13b NodeClaim pool reconciler. `enabled = false` =
    /// reconciler not spawned (legacy 12-NodePool mode). Env:
    /// `RIO_NODECLAIM_POOL__ENABLED` / `__DATABASE_URL` / `__LEASE_NAME`
    /// / `__NODE_CLASS_REF` / `__MAX_FLEET_CORES` / etc.
    pub nodeclaim_pool: NodeClaimPoolConfig,
    /// HMAC key for minting `x-rio-service-token` on AdminService
    /// calls. SAME file as the gateway/scheduler/store
    /// `service_hmac_key_path` (one shared `rio-service-hmac` Secret).
    /// `None` = dev mode (no header attached; scheduler's verifier is
    /// also `None` and passes through). Env:
    /// `RIO_SERVICE_HMAC_KEY_PATH`. See `r[sec.authz.service-token]`.
    pub service_hmac_key_path: Option<std::path::PathBuf>,
    /// ADR-023 §13a: pod `requests.memory` floor for the
    /// `rio.build/hw-bench-needed` gate (STREAM triad working-set
    /// safety). MUST match the scheduler's `[sla].hw_bench_mem_floor`;
    /// helm renders both from `sla.hwBenchMemFloor`. Env:
    /// `RIO_HW_BENCH_MEM_FLOOR`.
    pub hw_bench_mem_floor: u64,
    /// Cluster identity axis for the node-informer's exposure uids
    /// (merged_bug_001). `interrupt_samples` lives in the shared-PG
    /// (global-DB) topology of ADR-023 §2.13 and M_047's partial
    /// unique index on `event_uid` is table-GLOBAL, so every exposure
    /// idempotency key MUST carry the cluster
    /// (`exposure:{cluster}:{hw}:{window-slot}`) or two clusters'
    /// informers silently absorb each other's λ-denominator windows.
    /// MIRRORS the scheduler's `[sla].cluster`: helm renders BOTH from
    /// the one values expression (`scheduler.sla.cluster`, falling
    /// back to `karpenter.clusterName`, then `""`) into the two TOMLs
    /// — never set them apart by hand. Empty = single-cluster default
    /// (matches the scheduler's `DEFAULT ''` column). bug_022: the
    /// default is safe ONLY while this deployment's PG is private to
    /// it — two deployments both at `""` on one PG mint identical
    /// uids and silently absorb each other's λ evidence; the chart
    /// refuses to render an empty id when the external-secrets PG
    /// path (the shared-capable topology) is enabled, and the
    /// informer warns at activation on the empty default.
    pub cluster: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scheduler: rio_common::config::UpstreamAddrs::with_port(9001),
            store: rio_common::config::UpstreamAddrs::with_port(9002),
            // 9094: gateway=9090, scheduler=9091, store=9092,
            // worker=9093. Controller is next.
            common: rio_common::config::CommonConfig::new(9094),
            // Same +100 pattern as gateway/worker.
            health_addr: rio_common::default_addr(9194),
            // 24h: typical store growth between sweeps is a few
            // thousand paths. Lower values are fine for VM tests.
            gc_interval_hours: 24,
            nodeclaim_pool: NodeClaimPoolConfig::default(),
            service_hmac_key_path: None,
            // 8 GiB: matches `rio_scheduler::sla::config::
            // default_hw_bench_mem_floor`. STREAM triad's 3×4×LLC
            // working set tops out ~4.6 GiB on c7a.48xlarge.
            hw_bench_mem_floor: 8 * (1 << 30),
            // Single-cluster default — mirrors the scheduler's
            // `[sla].cluster` `DEFAULT ''`.
            cluster: String::new(),
        }
    }
}

#[derive(Parser, Serialize, Default)]
#[command(name = "rio-controller", about = "Kubernetes operator for rio-build")]
pub struct CliArgs {
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics_addr: Option<std::net::SocketAddr>,

    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    health_addr: Option<std::net::SocketAddr>,
}

impl rio_common::config::ValidateConfig for Config {
    /// Bounds checks on operator-settable fields. Extracted from
    /// `main()` so the checks are unit-testable without spinning up
    /// the full controller (kube-client connect, reconciler spawn).
    /// Every `ensure!` documents a specific crash or silent-wrong
    /// that occurs AFTER startup if the bad value gets through.
    fn validate(&self) -> anyhow::Result<()> {
        self.scheduler
            .ensure_required("scheduler.addr", "controller")?;
        rio_common::config::ensure_required(
            &self.nodeclaim_pool.database_url,
            "nodeclaim_pool.database_url",
            "controller",
        )?;
        anyhow::ensure!(
            self.nodeclaim_pool.max_fleet_cores > 0,
            "nodeclaim_pool.max_fleet_cores must be > 0"
        );
        Ok(())
    }
}

rio_common::impl_has_common_config!(Config);
