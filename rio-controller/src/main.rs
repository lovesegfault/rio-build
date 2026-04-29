//! rio-controller binary.
//!
//! Runs one Controller::run loop per CRD (Pool, ComponentScaler)
//! plus the disruption watcher and
//! GC schedule. All terminate on SIGTERM via graceful_shutdown_on.
//!
// r[impl sec.psa.control-plane-restricted]
//! rio-controller (and scheduler/gateway/store) run under PSA
//! `restricted` — runAsNonRoot (UID 65532), drop-ALL, seccomp:
//! RuntimeDefault, readOnlyRootFilesystem. The securityContext
//! lives in `infra/helm/rio-build/templates/_helpers.tpl`
//! (`rio.podSecurityContext` / `rio.containerSecurityContext`);
//! image-level `config.User` in `nix/docker.nix`. No CAP_SYS_ADMIN,
//! no FUSE, no raw sockets — plain gRPC + kube-apiserver client.

use std::sync::Arc;

use clap::Parser;
use k8s_openapi::api::batch::v1::Job;
use kube::Client;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use rio_controller::reconcilers::node_informer::NodeLabelCache;
use rio_controller::reconcilers::nodeclaim_pool::{
    self, ControllerLeaseHooks, NodeClaimPoolConfig, NodeClaimPoolReconciler,
};
use rio_controller::reconcilers::nodepoolbudget::NodePoolBudgetConfig;
use rio_controller::reconcilers::{
    AdminClient, Ctx, componentscaler, node_informer, nodepoolbudget, pool,
};
use rio_controller::spawn_controller;
use rio_crds::componentscaler::ComponentScaler;
use rio_crds::pool::Pool;

// ----- config (figment two-struct) --------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    /// rio-scheduler upstream. Env: `RIO_SCHEDULER__ADDR` /
    /// `__BALANCE_HOST` / `__BALANCE_PORT`. `balance_host` used two
    /// ways: (1) injected into worker pods as
    /// `RIO_SCHEDULER__BALANCE_HOST`; (2) THIS process's autoscaler
    /// uses it for leader-aware ClusterStatus polling. `None` →
    /// single-channel via `addr` (ClusterIP — round-robins to the
    /// standby ~50% of the time with replicas=2).
    scheduler: rio_common::config::UpstreamAddrs,
    /// rio-store upstream. Env: `RIO_STORE__ADDR` / `__BALANCE_HOST`
    /// / `__BALANCE_PORT`. Injected into worker pod containers by
    /// the Pool reconciler. I-077: balance host needed so
    /// scaling rio-store 1→4 actually spreads load.
    store: rio_common::config::UpstreamAddrs,
    #[serde(flatten)]
    common: rio_common::config::CommonConfig,
    /// HTTP /healthz listen address. K8s livenessProbe hits this.
    health_addr: std::net::SocketAddr,
    /// GC cron interval (hours). 0 = disabled (reconciler not
    /// spawned). The cron calls StoreAdminService.TriggerGC with
    /// default params (dry_run=false, force=false, store's 2h
    /// grace). `store_addr` is the connect target — StoreAdminService
    /// is hosted on the store's gRPC port alongside StoreService.
    gc_interval_hours: u64,
    /// Shared Karpenter NodePool vCPU budget. `cpu_millicores = 0` =
    /// reconciler not spawned. Env: `RIO_NODEPOOL_BUDGET__CPU_MILLICORES`
    /// / `__SELECTOR`.
    nodepool_budget: NodePoolBudgetConfig,
    /// ADR-023 §13b NodeClaim pool reconciler. `enabled = false` =
    /// reconciler not spawned (legacy 12-NodePool mode). Env:
    /// `RIO_NODECLAIM_POOL__ENABLED` / `__DATABASE_URL` / `__LEASE_NAME`
    /// / `__NODE_CLASS_REF` / `__MAX_FLEET_CORES` / etc.
    nodeclaim_pool: NodeClaimPoolConfig,
    /// HMAC key for minting `x-rio-service-token` on AdminService
    /// calls. SAME file as the gateway/scheduler/store
    /// `service_hmac_key_path` (one shared `rio-service-hmac` Secret).
    /// `None` = dev mode (no header attached; scheduler's verifier is
    /// also `None` and passes through). Env:
    /// `RIO_SERVICE_HMAC_KEY_PATH`. See `r[sec.authz.service-token]`.
    service_hmac_key_path: Option<std::path::PathBuf>,
    /// ADR-023 §13a: pod `requests.memory` floor for the
    /// `rio.build/hw-bench-needed` gate (STREAM triad working-set
    /// safety). MUST match the scheduler's `[sla].hw_bench_mem_floor`;
    /// helm renders both from `sla.hwBenchMemFloor`. Env:
    /// `RIO_HW_BENCH_MEM_FLOOR`.
    hw_bench_mem_floor: u64,
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
            nodepool_budget: NodePoolBudgetConfig::default(),
            nodeclaim_pool: NodeClaimPoolConfig::default(),
            service_hmac_key_path: None,
            // 8 GiB: matches `rio_scheduler::sla::config::
            // default_hw_bench_mem_floor`. STREAM triad's 3×4×LLC
            // working set tops out ~4.6 GiB on c7a.48xlarge.
            hw_bench_mem_floor: 8 * (1 << 30),
        }
    }
}

#[derive(Parser, Serialize, Default)]
#[command(name = "rio-controller", about = "Kubernetes operator for rio-build")]
struct CliArgs {
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics_addr: Option<std::net::SocketAddr>,

    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    health_addr: Option<std::net::SocketAddr>,
}

// ----- main --------------------------------------------------------------------

impl rio_common::config::ValidateConfig for Config {
    /// Bounds checks on operator-settable fields. Extracted from
    /// `main()` so the checks are unit-testable without spinning up
    /// the full controller (kube-client connect, reconciler spawn).
    /// Every `ensure!` documents a specific crash or silent-wrong
    /// that occurs AFTER startup if the bad value gets through.
    fn validate(&self) -> anyhow::Result<()> {
        self.scheduler
            .ensure_required("scheduler.addr", "controller")?;
        if self.nodeclaim_pool.enabled {
            rio_common::config::ensure_required(
                &self.nodeclaim_pool.database_url,
                "nodeclaim_pool.database_url",
                "controller",
            )?;
            anyhow::ensure!(
                self.nodeclaim_pool.max_fleet_cores > 0,
                "nodeclaim_pool.max_fleet_cores must be > 0 when enabled"
            );
        }
        Ok(())
    }
}

rio_common::impl_has_common_config!(Config);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = CliArgs::parse();
    let rio_common::server::Bootstrap::<Config> {
        cfg,
        shutdown,
        serve_shutdown: _,
        otel_guard: _otel_guard,
        root_span: _root_span,
    } = rio_common::server::bootstrap(
        "controller",
        cli,
        rio_controller::describe_metrics,
        rio_controller::HISTOGRAM_BUCKETS,
    )?;

    // store.addr is injected into worker pod containers as
    // RIO_STORE__ADDR. Workers with an empty store addr fail their
    // first PutPath with a tonic malformed-URI error — deep inside
    // a spawned task, easy to miss. Warn loudly at startup.
    if cfg.store.addr.is_empty() {
        warn!(
            "RIO_STORE__ADDR not set; worker pods will get empty RIO_STORE__ADDR \
             env (PutPath will fail with malformed URI)."
        );
    }

    // ---- K8s client ----
    // try_default reads in-cluster config (service account token
    // at /var/run/secrets/kubernetes.io/serviceaccount/) or
    // KUBECONFIG for local dev. `?` — no kube client = useless
    // controller, fail loud.
    let client = Client::try_default().await?;
    info!("kubernetes client connected");

    // ---- Health server ----
    // r[impl ctrl.health.ready-gates-connect]
    // BEFORE dependency connect: liveness = process alive + runtime
    // responsive (server.rs `/healthz` is unconditional 200), so it
    // must answer immediately or the chart's livenessProbe
    // (periodSeconds:10, failureThreshold:3, no startupProbe) kills
    // the pod at ~20-30s — re-introducing the CrashLoopBackOff that
    // `connect_forever` exists to avoid. Readiness = dependencies
    // reachable: `/readyz` is 503 until `ready` flips after
    // `connect_forever` returns.
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    rio_common::server::spawn_axum(
        "health-server",
        cfg.health_addr,
        rio_common::server::health_router(ready.clone()),
        shutdown.clone(),
    );

    // ---- Scheduler clients (autoscaler + reconcilers) ----
    // Retry until connected via connect_forever (shutdown-aware,
    // exponential backoff). All rio-* pods start in parallel via
    // helm; this process can reach here before the scheduler Service
    // has endpoints. Pod stays NotReady (`/readyz` → 503) while
    // retrying; `/healthz` already serves 200 so livenessProbe
    // passes. Observed 2/2 coverage-full failures 2026-03-16 before
    // retry was added (CrashLoopBackOff ate the 180s test budget).
    //
    // Once connected: hold the channel for process lifetime. Balanced
    // when scheduler_balance_host is set — the standby returns
    // UNAVAILABLE on all RPCs, so ClusterIP round-robin fails ~50% of
    // ticks; balanced channel health-probes pod IPs and routes only
    // to the leader. Guard held in _balance_guard (dropping it stops
    // the probe loop). Single-channel mode: dev/test only.
    let Some((admin_ch, _balance_guard)) = rio_proto::client::connect_forever(&shutdown, || {
        rio_proto::client::connect_raw::<rio_proto::AdminServiceClient<_>>(&cfg.scheduler)
    })
    .await
    else {
        return Ok(());
    };
    // Wrap the balanced channel with a service-token interceptor: every
    // AdminService RPC carries `x-rio-service-token` so the scheduler's
    // controller-only gates (AppendInterruptSample, DrainExecutor) pass.
    // r[impl sec.authz.service-token]
    let service_signer = rio_auth::hmac::HmacSigner::load(cfg.service_hmac_key_path.as_deref())
        .map_err(|e| anyhow::anyhow!("service HMAC key load: {e}"))?
        .map(std::sync::Arc::new);
    if service_signer.is_some() {
        info!("x-rio-service-token minting enabled on AdminService + StoreAdminService RPCs");
    }
    // One interceptor instance, cloned for every controller→service
    // client (scheduler `AdminService` + store `StoreAdminService`).
    // Both gates check `caller="rio-controller"` against per-RPC
    // allowlists. r[store.admin.service-gate].
    let service_interceptor =
        rio_auth::hmac::ServiceTokenInterceptor::new(service_signer, "rio-controller");
    let admin: AdminClient =
        rio_proto::AdminServiceClient::with_interceptor(admin_ch, service_interceptor.clone())
            .max_decoding_message_size(rio_common::grpc::max_message_size())
            .max_encoding_message_size(rio_common::grpc::max_message_size());

    ready.store(true, std::sync::atomic::Ordering::Relaxed);

    // ---- Events Recorder ----
    // Reporter identifies US (the controller) in emitted events.
    // `kubectl get events` shows `rio-controller` in the SOURCE
    // column. instance=None → K8s uses pod name (from metadata.
    // name downward API if set, else hostname).
    let recorder = kube::runtime::events::Recorder::new(
        client.clone(),
        kube::runtime::events::Reporter {
            controller: "rio-controller".into(),
            instance: None,
        },
    );

    // ---- Context ----
    // Placeable-gate channel: created here (before Ctx) so the receiver
    // is in `Ctx` for the Pool reconciler and the sender is passed to
    // `NodeClaimPoolReconciler::new` below. When disabled, no channel —
    // `PlaceableGate::disabled()` makes `jobs.rs` fall back to the §13a
    // `ready` gate. The `_tx` binding keeps the sender alive across the
    // disabled/PG-connect-failed paths so the gate stays unarmed (not
    // closed); `placeable_tx.take()` hands it to the reconciler when
    // enabled.
    let (mut placeable_tx, placeable) = if cfg.nodeclaim_pool.enabled {
        let (tx, gate) = nodeclaim_pool::placeable_channel();
        (Some(tx), gate)
    } else {
        (None, nodeclaim_pool::PlaceableGate::disabled())
    };
    let ctx = Arc::new(Ctx {
        client: client.clone(),
        admin: admin.clone(),
        scheduler: cfg.scheduler.clone(),
        store: cfg.store.clone(),
        recorder: recorder.clone(),
        service_interceptor: service_interceptor.clone(),
        error_counts: Default::default(),
        scaler: Default::default(),
        hw_bench_mem_floor: cfg.hw_bench_mem_floor,
        placeable,
    });

    // ---- Reconcilers ----
    // `spawn_controller!` expands to `Controller::new().owns()
    // .graceful_shutdown_on().run().for_each()`. Each yields a
    // future; `tokio::join!` below polls both concurrently.
    //
    // `owns:` — kube-runtime watches that child kind and re-enqueues
    // the parent on child status change (e.g. Job complete → re-spawn
    // in <1s instead of waiting for the 10s poll). ComponentScaler
    // owns nothing: it patches `/scale` on a helm-owned Deployment.
    //
    // graceful_shutdown_on: SIGTERM cancels the token (registered
    // eagerly at top of main()), which drains in-flight reconciles.
    let pool_controller = spawn_controller!(client, shutdown, ctx, Pool, pool, owns: Job);
    let cs_controller = spawn_controller!(client, shutdown, ctx, ComponentScaler, componentscaler);

    // ---- DisruptionTarget watcher ----
    // Pod watcher: K8s sets DisruptionTarget=True on a pod BEFORE
    // eviction (node drain, spot interrupt). We fire DrainExecutor
    // {force:true} → scheduler preempts in-flight builds (cgroup.
    // kill + reassign) in seconds instead of burning the 2h
    // terminationGracePeriodSeconds. SIGTERM self-drain (force=
    // false) is the fallback if this task misses the window.
    //
    // spawn_monitored: if the watcher panics, logged; controller
    // keeps reconciling. Loses fast-preemption but not correctness
    // (SIGTERM drain still runs).
    rio_common::task::spawn_monitored(
        "disruption-watcher",
        pool::disruption::run(client.clone(), admin.clone(), shutdown.clone()),
    );

    // ---- Node informer ----
    // Caches Node labels → hw_class string for completion-ingest
    // join (ADR-023). Builders report spec.nodeName (downward API);
    // controller joins server-side because builders are air-gapped
    // from apiserver. `hw_class` is the operator's `[sla.hw_classes.$h]`
    // key matched against Node labels (NOT a hardcoded reconstruction;
    // bug_061), so fetch the scheduler's config once before spawning
    // the informer/annotator. `connect_forever` already established
    // the channel; `load` retries 5× with backoff for leader-election
    // transients then degrades to empty (annotator/λ skip).
    // TODO: clone node_cache into the completion-ingest consumer
    // once that lands; until then this populates but nobody reads.
    let hw_config = node_informer::HwClassConfig::default();
    hw_config.load(&mut admin.clone()).await;
    let node_cache = NodeLabelCache::with_config(hw_config.clone());
    rio_common::task::spawn_monitored(
        "node-informer",
        node_informer::run(
            client.clone(),
            node_cache.clone(),
            admin.clone(),
            shutdown.clone(),
        ),
    );
    // ADR-023 phase-10: stamp `rio.build/hw-class` on each builder
    // pod once `spec.nodeName` resolves. Builder reads it via
    // downward-API to key its `hw_perf_samples` microbench insert.
    rio_common::task::spawn_monitored(
        "hw-class-annotator",
        node_informer::run_pod_annotator(client.clone(), node_cache.clone(), shutdown.clone()),
    );
    // ADR-023 phase-13: SpotInterrupted Event → interrupt_samples
    // (λ\[h\] numerator). Node-DELETE in the informer above writes the
    // exposure denominator.
    rio_common::task::spawn_monitored(
        "spot-interrupt-watcher",
        node_informer::run_spot_interrupt_watcher(
            client.clone(),
            node_cache,
            admin.clone(),
            shutdown.clone(),
        ),
    );

    // ---- GC cron ----
    // Gated on gc_interval_hours > 0. 0 = disabled (operators who
    // want manual-only GC via rio-cli). Also gated on store_addr
    // non-empty — we already warned above if it's empty (workers
    // will break too); don't also spawn a cron that will never
    // connect. Both gates log so the absence is diagnosable.
    //
    // No leader-gate: controller is single-replica by design
    // (controller.md — no leader election). If replicas>1 by
    // misconfig, the store's GC_LOCK_ID advisory lock serializes
    // concurrent TriggerGC calls (see gc_schedule module doc).
    if cfg.gc_interval_hours > 0 && !cfg.store.addr.is_empty() {
        let gc_tick = std::time::Duration::from_secs(cfg.gc_interval_hours * 3600);
        rio_common::task::spawn_monitored(
            "gc-cron",
            rio_controller::reconcilers::gc_schedule::run(
                cfg.store.addr.clone(),
                service_interceptor.clone(),
                gc_tick,
                shutdown.clone(),
            ),
        );
    } else {
        info!(
            gc_interval_hours = cfg.gc_interval_hours,
            store_addr_set = !cfg.store.addr.is_empty(),
            "GC cron disabled"
        );
    }

    // ---- NodeClaim pool (ADR-023 §13b) ----
    // Gated on `nodeclaim_pool.enabled`. Lease-elected: only the leader
    // replica reconciles. Lease + PG connect run AFTER the scheduler
    // `connect_forever` above so the table is migrated by the time
    // `CellSketches::load` reads it (scheduler/store own the migrator).
    // r[impl ctrl.nodeclaim.shim-nodepool]
    if cfg.nodeclaim_pool.enabled {
        let lease_cfg = rio_lease::LeaseConfig::from_parts(
            cfg.nodeclaim_pool.lease_name.clone(),
            cfg.nodeclaim_pool.lease_namespace.clone(),
        );
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let leader = match &lease_cfg {
            Some(lc) => {
                info!(
                    lease = %lc.lease_name, namespace = %lc.namespace, holder = %lc.holder_id,
                    "nodeclaim_pool lease election enabled"
                );
                rio_lease::LeaderState::pending(Arc::clone(&generation))
            }
            None => {
                info!("nodeclaim_pool lease_name unset; running as sole leader (non-K8s mode)");
                rio_lease::LeaderState::always_leader(Arc::clone(&generation))
            }
        };
        // Controller has no recovery step — flip recovery_complete now
        // so `leader_for()` consumers (none yet) see a coherent state.
        leader.set_recovery_complete();

        if let Some(lease_cfg) = lease_cfg {
            rio_common::task::spawn_monitored(
                "nodeclaim-pool-lease",
                rio_lease::run_lease_loop(
                    lease_cfg,
                    leader.clone(),
                    ControllerLeaseHooks,
                    shutdown.clone(),
                ),
            );
        }

        if let Some(pg) =
            nodeclaim_pool::connect_pg(&cfg.nodeclaim_pool.database_url, &shutdown).await
        {
            let reconciler = NodeClaimPoolReconciler::new(
                client.clone(),
                admin.clone(),
                pg,
                leader,
                cfg.nodeclaim_pool.clone(),
                hw_config.clone(),
                placeable_tx
                    .take()
                    .expect("placeable_tx Some iff nodeclaim_pool.enabled"),
            )
            .await;
            rio_common::task::spawn_monitored("nodeclaim-pool", reconciler.run(shutdown.clone()));
        }
    } else {
        info!("nodeclaim_pool reconciler disabled (legacy NodePool mode)");
    }

    // ---- NodePool budget ----
    // Gated on cpu_millicores > 0 (same opt-in pattern as gc-cron).
    // Disabled = NodePool limits stay helm-managed.
    if cfg.nodepool_budget.cpu_millicores > 0 {
        rio_common::task::spawn_monitored(
            "nodepool-budget",
            nodepoolbudget::run(client.clone(), cfg.nodepool_budget, shutdown.clone()),
        );
    } else {
        info!("NodePool budget reconciler disabled (cpu_millicores=0)");
    }

    info!("controller running");
    // Both controllers run until SIGTERM (graceful_shutdown_on
    // drains in-flight reconciles). tokio::join! polls both
    // concurrently on THIS task — no separate spawn. Semantics:
    //   - Ok(()) from ONE: join! continues polling the OTHER until
    //     it also completes (graceful-shutdown waits for both drains).
    //   - Panic in ONE: unwinds through join! immediately — the OTHER
    //     is NOT polled to completion (process exits via unwind).
    // This is the intended behavior: panics propagate (no JoinHandle
    // silent-swallow), Ok-exits wait for sibling (no half-drained
    // state on shutdown).
    tokio::join!(pool_controller, cs_controller);

    info!("controller shutting down");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_common::config::ValidateConfig as _;

    #[test]
    fn config_defaults_are_stable() {
        let d = Config::default();
        assert!(d.scheduler.addr.is_empty(), "required, no default");
        assert_eq!(d.common.metrics_addr.to_string(), "[::]:9094");
        assert_eq!(d.health_addr.to_string(), "[::]:9194");
        assert_eq!(d.gc_interval_hours, 24, "GC cron defaults to daily");
        assert_eq!(
            d.nodepool_budget.cpu_millicores, 0,
            "NodePool budget disabled by default"
        );
        assert!(
            !d.nodeclaim_pool.enabled,
            "nodeclaim_pool disabled by default"
        );
    }

    #[test]
    fn cli_args_parse_help() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert();
    }

    // figment::Jail standing-guard tests — see rio-test-support/src/config.rs.
    // When you add Config.newfield: ADD IT to both assert blocks below.

    rio_test_support::jail_roundtrip!(
        "controller",
        r#"
        gc_interval_hours = 0
        "#,
        |cfg: Config| {
            assert_eq!(cfg.gc_interval_hours, 0);
        }
    );

    rio_test_support::jail_defaults!("controller", "gc_interval_hours = 24", |cfg: Config| {
        assert!(cfg.scheduler.balance_host.is_none());
        assert_eq!(cfg.gc_interval_hours, 24);
    });

    // -----------------------------------------------------------------------
    // validate_config rejection tests — spreads the P0409 pattern
    // (rio-scheduler/src/main.rs) to the controller.
    // -----------------------------------------------------------------------

    /// All required fields filled with valid values — so rejection
    /// tests can patch ONE field and prove that specific check fires.
    /// `Config::default()` leaves `scheduler_addr` empty, which
    /// validate_config rejects BEFORE reaching the bounds checks we
    /// want to test.
    fn test_valid_config() -> Config {
        let mut cfg = Config::default();
        cfg.scheduler.addr = "http://localhost:9000".into();
        cfg.store.addr = "http://localhost:9001".into();
        cfg
    }

    #[test]
    fn config_rejects_nodeclaim_pool_without_db() {
        let mut cfg = test_valid_config();
        cfg.nodeclaim_pool.enabled = true;
        cfg.nodeclaim_pool.database_url = String::new();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("nodeclaim_pool.database_url"), "{err}");
    }

    #[test]
    fn config_accepts_nodeclaim_pool_with_db() {
        let mut cfg = test_valid_config();
        cfg.nodeclaim_pool.enabled = true;
        cfg.nodeclaim_pool.database_url = "postgres://localhost/rio".into();
        cfg.validate().expect("enabled + db url passes");
    }

    #[test]
    fn config_rejects_empty_scheduler_addr() {
        let mut cfg = test_valid_config();
        cfg.scheduler.addr = String::new();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("scheduler.addr"), "{err}");
    }

    /// Baseline: `test_valid_config()` itself passes — proves the
    /// rejection tests above are testing ONLY their mutation.
    #[test]
    fn config_accepts_valid() {
        test_valid_config()
            .validate()
            .expect("valid config should pass");
    }
}
