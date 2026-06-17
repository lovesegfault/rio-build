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
use tracing::{info, warn};

use rio_controller::config::{CliArgs, Config};
use rio_controller::reconcilers::nodeclaim_pool::{self, ControllerLeaseHooks};
use rio_controller::reconcilers::{AdminClient, Ctx, componentscaler, node_informer, pool};
use rio_controller::spawn_controller;
use rio_crds::componentscaler::ComponentScaler;
use rio_crds::pool::Pool;

// ----- main --------------------------------------------------------------------

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

    // ---- Guard domain (health + skew sentinel; lease joins below) ----
    // r[impl ctrl.health.ready-gates-connect+1]
    // BEFORE dependency connect: the guard thread binds `/healthz`
    // immediately, so the chart's livenessProbe (periodSeconds:10,
    // timeoutSeconds:10, failureThreshold:6, startupProbe 2s×30)
    // passes during scheduler cold-start — `connect_forever` below
    // can retry without a CrashLoopBackOff. Round-9 Banner B (the
    // live_054 close): liveness, lease renewal, and the skew sentinel
    // are KILL-WIRED surfaces and live on a dedicated current_thread
    // runtime that stays schedulable when this (main) runtime is not;
    // readiness stays SHED-WIRED to this working domain — `/readyz`
    // is 503 until `ready` flips after `connect_forever` returns, and
    // 503 again whenever the main runtime cannot schedule the guard's
    // probe within budget (brownout = Endpoints removal, never a
    // kill). Cross-domain state is lock-free only — the census lives
    // in the guard module doc.
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (guard, guard_join) = rio_controller::guard::spawn(
        tokio::runtime::Handle::current(),
        ready.clone(),
        rio_controller::guard::GuardConfig {
            health_addr: cfg.health_addr,
            ..Default::default()
        },
        shutdown.clone(),
    );

    // Every `?`/`return` between guard::spawn and .join() MUST be inside
    // this block — the post-await epilogue is the single discharge for
    // guard_join (sys.epilogue.drain; r26 irony-check on bug_023).
    let run: anyhow::Result<()> = async {
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
        let Some((admin_ch, _balance_guard)) =
            rio_proto::client::connect_forever(&shutdown, || {
                rio_proto::client::connect_raw::<rio_proto::AdminServiceClient<_>>(&cfg.scheduler)
            })
            .await
        else {
            return Ok(());
        };
        // Wrap the balanced channel with a service-token interceptor: every
        // AdminService RPC carries `x-rio-service-token` so the scheduler's
        // controller-only gates (AppendInterruptSample, ReportAttemptOutcome,
        // AckSpawnedIntents, MintExecutorTokens) pass.
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
        // The lease-generation atomic, hoisted ABOVE the admin client so
        // the generation-stamp interceptor (D4, fence module) can read it
        // on every RPC. 1 is the generation FLOOR (the value the lease
        // loop's fetch_max can only raise) — also the steady state in
        // non-K8s mode; pre-acquire RPCs therefore stamp generation 1,
        // which the scheduler's watermark treats like any other value.
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let admin: AdminClient = rio_proto::AdminServiceClient::with_interceptor(
            admin_ch,
            rio_controller::reconcilers::fence::GenerationStamp::new(
                service_interceptor.clone(),
                Arc::clone(&generation),
            ),
        )
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
        // `NodeClaimPoolReconciler::new` below. The Option<tx> keeps the
        // sender alive while connect_pg retries (in spawn_monitored below)
        // so the gate stays unarmed (not closed); `placeable_tx.take()`
        // hands it to the reconciler. `Ctx.placeable = None` ⇔ NodeClaim CRD
        // absent (k3s VM tests without Karpenter) — the gate is a
        // pass-through and the nodeclaim_pool reconciler is not spawned.
        let nodeclaim_crd = nodeclaim_pool::nodeclaim_crd_present(&client).await;
        let (mut placeable_tx, placeable) = if nodeclaim_crd {
            let (tx, rx) = nodeclaim_pool::placeable_channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        // ---- HwClass config ----
        // Loaded BEFORE `Ctx` so `pool/jobs::apply_intent_resources` and
        // `pool/pod::wants_metal` derive tolerations from the same
        // `[sla.hw_classes.$h]` map the scheduler routed against (r31
        // bug_020). Also feeds the node-informer / annotator below and
        // the nodeclaim_pool reconciler. `connect_forever` already
        // established the channel; `load` retries 5× with backoff for
        // leader-election transients then degrades to empty (annotator/λ
        // skip; `wants_metal` falls back to the literal `kvm` feature).
        let hw_config = node_informer::HwClassConfig::default();
        hw_config.load(&mut admin.clone()).await;
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
            kube_build_scheduler_enabled: cfg.nodeclaim_pool.kube_build_scheduler_enabled,
            hw_config: hw_config.clone(),
            terminal_report_sampled: Default::default(),
            exhausted_streak: Default::default(),
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
        let cs_controller =
            spawn_controller!(client, shutdown, ctx, ComponentScaler, componentscaler);

        // ---- DisruptionTarget watcher ----
        // Pod watcher: K8s sets DisruptionTarget=True on a pod BEFORE
        // eviction (node drain, spot interrupt). We synthesize the
        // preempted report and foreground-delete the owning Job → the
        // pod's SIGTERM-abort cgroup-kills the build and the drv requeues
        // in seconds instead of burning the grace period. The pod's own
        // SIGTERM abort is the fallback if this task misses the window.
        //
        // spawn_monitored: if the watcher panics, logged; controller
        // keeps reconciling. Loses fast-preemption but not correctness
        // (SIGTERM drain still runs).
        rio_common::task::spawn_monitored(
            "disruption-watcher",
            pool::disruption::run(client.clone(), admin.clone(), shutdown.clone()),
        );

        // ---- Node informer ----
        // λ[h] spot-exposure flush (60s Node LIST) + 300s HwClassConfig
        // refresh (ADR-023). Node labels are NOT cached: the per-need
        // consumers below GET the node when they actually need a label
        // join (§4(a)2 — the NodeLabelCache deletion). `hw_class` is the
        // operator's `[sla.hw_classes.$h]` key matched against Node
        // labels (NOT a hardcoded reconstruction; bug_061). Reuses
        // `hw_config` loaded above the `Ctx` block.
        // bug_363: the exposure flush maintains a name→hw_class fallback
        // the interrupt watcher consults when the interrupted node is
        // already gone (the common reclaim case).
        // merged_bug_001: AT MOST ONE informer per cluster — exposure
        // uids are keyed (cluster, class, window-slot), so a co-running
        // twin of the SAME cluster converges on identical uids (the
        // absorb dedups it), but the residual partial-window seam and
        // interrupt-watcher duplication are closed by the chart's
        // `strategy: Recreate` (controller.yaml) — the informer is
        // deliberately NOT lease-gated. `cluster` is Config-borne
        // (controller.toml), single-sourced by helm with the scheduler's
        // `[sla].cluster`; this is the ONE read site.
        let cluster = node_informer::ClusterId::new(&cfg.cluster);
        let hw_fallback: node_informer::HwClassFallback = Default::default();
        rio_common::task::spawn_monitored(
            "node-informer",
            node_informer::run(
                client.clone(),
                hw_config.clone(),
                admin.clone(),
                hw_fallback.clone(),
                cluster,
                shutdown.clone(),
            ),
        );
        // ADR-023 phase-10: stamp `rio.build/hw-class` on each builder
        // pod once `spec.nodeName` resolves (per-need Node GET). Builder
        // reads it via downward-API to key its `hw_perf_samples`
        // microbench insert.
        rio_common::task::spawn_monitored(
            "hw-class-annotator",
            node_informer::run_pod_annotator(client.clone(), hw_config.clone(), shutdown.clone()),
        );
        // sh-028: stamp `pod-deletion-cost` on each gateway pod with
        // its scraped `rio_gateway_connections_active` so KEDA
        // scale-down evicts the least-loaded replica. Lives HERE (not
        // in the gateway) so the gateway keeps `automountServiceAccountToken:
        // false` and gains no kube client / `pods` RBAC — the
        // controller already holds `[get, list, patch]` on pods. Gated
        // on `gateway_namespace` non-empty (helm sets it via downward
        // API; non-k8s leaves it empty → annotator not spawned).
        if !cfg.gateway_namespace.is_empty() {
            rio_common::task::spawn_monitored(
                "gateway-cost-annotator",
                rio_controller::reconcilers::gateway_cost::run(
                    client.clone(),
                    cfg.gateway_namespace.clone(),
                    shutdown.clone(),
                ),
            );
        } else {
            info!(
                "gateway deletion-cost annotator disabled \
                 (RIO_GATEWAY_NAMESPACE unset — non-k8s mode)"
            );
        }
        // ADR-023 phase-13: SpotInterrupted Event → interrupt_samples
        // (λ\[h\] numerator). The informer's periodic flush above writes
        // the exposure denominator.
        rio_common::task::spawn_monitored(
            "spot-interrupt-watcher",
            node_informer::run_spot_interrupt_watcher(
                client.clone(),
                hw_config.clone(),
                admin.clone(),
                hw_fallback,
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
        // No leader-gate: controller is single-replica (only the
        // nodeclaim_pool reconciler is lease-gated, for rolling-upgrade
        // surge safety; the node-informer additionally relies on the
        // chart's `strategy: Recreate` so a rollout never co-runs two
        // informers — merged_bug_001, see the informer-spawn comment
        // above). If replicas>1 by misconfig, the store's
        // GC_LOCK_ID advisory lock serializes
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
        // Lease-elected: only the leader replica reconciles. Lease + PG
        // connect run AFTER the scheduler `connect_forever` above so the
        // table is migrated by the time `CellSketches::load_seeded` reads it
        // (scheduler/store own the migrator).
        // r[impl ctrl.nodeclaim.shim-nodepool]
        if nodeclaim_crd {
            let lease_cfg = rio_lease::LeaseConfig::from_parts(
                cfg.nodeclaim_pool.lease_name.clone(),
                cfg.nodeclaim_pool.lease_namespace.clone(),
            );
            // The controller's generation is CONSUMED (D4; the fence
            // module): the nodeclaim_pool mutation writers (create /
            // unhealthy-reap delete / consolidate delete) check a per-pass
            // MutationFence against it, every AdminService RPC stamps it
            // as request metadata via GenerationStamp (wired above), and
            // the scheduler's AckSpawnedIntents refuses generations below
            // its watermark. The consumer census is pinned by the fence
            // module tests + the scheduler's generation_fence_tests. The
            // atomic itself is hoisted above the admin-client construction
            // so the interceptor can read it; LeaderState below shares the
            // SAME Arc, so acquire/rebound updates reach every stamp.
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
            // Controller has no recovery step — record completion for the
            // startup acquire-epoch so `leader_for()` consumers (none yet)
            // see a coherent state. There is nothing to re-complete after
            // later lease transitions; nothing in the controller reads the
            // predicate.
            leader.set_recovery_complete(leader.acquired_transitions());

            let hooks = ControllerLeaseHooks::default();
            if let Some(lease_cfg) = lease_cfg {
                // Kill-wired (D1): renewal keeps its 5s cadence during
                // main-domain stalls, so the lease's fence-check premise
                // survives admitted-load starvation (the 054 violation was
                // 2.5-3x). The loop builds its own kube client on the
                // guard runtime — no main-domain pool sharing. The loop
                // owes a shutdown epilogue (the graceful-release PATCH):
                // its DrainHandle is adopted into the guard root, which
                // drains it bounded before the runtime drops
                // (sys.epilogue.drain; bug_118).
                guard.adopt_epilogue(guard.spawn_lease(
                    lease_cfg,
                    leader.clone(),
                    hooks.clone(),
                    shutdown.clone(),
                ));
            }

            // `connect_pg` wraps `connect_forever` (returns `None` only on
            // shutdown). `spawn_monitored` so a PG outage at boot doesn't
            // block `tokio::join!` below — the Pool/ComponentScaler
            // reconcilers run with the gate UNARMED
            // (`PlaceableGate::snapshot` reads None → fail-closed) until
            // PG connects and the reconciler publishes. Named `async fn`
            // (not an `async move` block) because of rustc's HRTB Send
            // check on nested `async ||` borrows — see `run_nodeclaim_pool`
            // doc.
            rio_common::task::spawn_monitored(
                "nodeclaim-pool",
                nodeclaim_pool::run_nodeclaim_pool(
                    client.clone(),
                    admin.clone(),
                    leader,
                    hooks,
                    cfg.nodeclaim_pool.clone(),
                    hw_config.clone(),
                    placeable_tx.take().expect("placeable_tx not yet taken"),
                    shutdown.clone(),
                ),
            );
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
    .await;

    // Process↔thread join (sys.epilogue.drain; bug_023): the guard
    // root drains the lease step_down() bounded by
    // SHUTDOWN_EPILOGUE_BUDGET, so this join is itself bounded —
    // bug_118 one lifecycle level up. spawn_blocking so the
    // multi_thread runtime keeps polling (the otel guard's flush etc.)
    // while the rio-guard thread drains.
    shutdown.cancel();
    tokio::task::spawn_blocking(move || guard_join.join())
        .await
        .expect("the rio-guard thread panicked during the epilogue drain");
    run
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
        assert_eq!(d.nodeclaim_pool.max_fleet_cores, 10_000);
    }

    #[test]
    fn cli_args_parse_help() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert();
    }

    // Jailed standing-guard tests — see rio-test-support/src/config.rs.
    // When you add Config.newfield: ADD IT to both assert blocks below.

    rio_test_support::jail_roundtrip!(
        "controller",
        r#"
        gc_interval_hours = 0
        cluster = "prod-east"

        [nodeclaim_pool]
        max_fleet_cores = 64
        kube_build_scheduler_enabled = false

        max_node_disk = 25769803776
        metal_sizes = ["metal", "metal-24xl"]

        [nodeclaim_pool.lead_time_seed]
        "vmtest:spot" = 5.0
        "#,
        |cfg: Config| {
            assert_eq!(cfg.gc_interval_hours, 0);
            // merged_bug_001: the exposure-uid cluster axis loads from
            // the helm-rendered TOML (same `cluster = …` key shape as
            // the scheduler's `[sla].cluster` — one values expression
            // feeds both binaries).
            assert_eq!(cfg.cluster, "prod-east");
            // B16: nested map/seq fields load from TOML (NOT env — the
            // RIO_ env layer yields bare strings). This is the same shape
            // helm's rio-controller-config ConfigMap renders.
            assert_eq!(cfg.nodeclaim_pool.max_fleet_cores, 64);
            // r40 bug_018: `kube_build_scheduler_enabled` is the gate
            // between "NodeClaim CRD present" and "stamp `schedulerName=
            // kube-build-scheduler`". Helm renders it from `buildScheduler.
            // enabled`; this is the only test proving the rendered TOML
            // key actually deserializes into `NodeClaimPoolConfig` (the
            // `pool/jobs::build_job` AND-gate is not unit-testable, and
            // compiled-defaults baselines silently leak defaults — a config
            // field that adds a deploy-hazard gate gets the strongest test).
            assert!(!cfg.nodeclaim_pool.kube_build_scheduler_enabled);

            assert_eq!(cfg.nodeclaim_pool.max_node_disk, 25769803776);
            assert_eq!(cfg.nodeclaim_pool.metal_sizes, vec!["metal", "metal-24xl"]);
            assert_eq!(cfg.nodeclaim_pool.default_lead_time_seed, 30.0);
            assert_eq!(
                cfg.nodeclaim_pool.lead_time_seed.get("vmtest:spot"),
                Some(&5.0)
            );
        }
    );

    rio_test_support::jail_defaults!("controller", "gc_interval_hours = 24", |cfg: Config| {
        assert!(cfg.scheduler.balance_host.is_none());
        assert_eq!(cfg.gc_interval_hours, 24);
        // merged_bug_001: empty = the single-cluster default, matching
        // the scheduler's `[sla].cluster` `DEFAULT ''`.
        assert_eq!(cfg.cluster, "");
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
        cfg.nodeclaim_pool.database_url = "postgres://localhost/rio".into();
        cfg
    }

    #[test]
    fn config_rejects_nodeclaim_pool_without_db() {
        let mut cfg = test_valid_config();
        cfg.nodeclaim_pool.database_url = String::new();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("nodeclaim_pool.database_url"), "{err}");
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
