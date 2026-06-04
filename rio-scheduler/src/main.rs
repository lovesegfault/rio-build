//! rio-scheduler binary entry point.
//!
//! Starts the gRPC server, connects to PostgreSQL, and spawns the DAG actor.
//! `Config` parsing/validation lives in [`config`]; binary tests in `tests`.

use std::sync::Arc;

use clap::Parser;
use tracing::info;

use rio_proto::AdminServiceServer;
use rio_proto::ExecutorServiceServer;
use rio_proto::SchedulerServiceServer;
use rio_scheduler::actor::ActorHandle;
use rio_scheduler::admin::AdminServiceImpl;
use rio_scheduler::db::SchedulerDb;
use rio_scheduler::grpc::{OffActorProbe, SchedulerGrpc};

use rio_scheduler::config::{CliArgs, Config, DashboardConfig};

#[cfg(test)]
mod tests;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = CliArgs::parse();
    let rio_common::server::Bootstrap::<Config> {
        cfg,
        shutdown,
        serve_shutdown,
        otel_guard: _otel_guard,
        root_span: _root_span,
    } = rio_common::server::bootstrap(
        "scheduler",
        cli,
        rio_scheduler::describe_metrics,
        rio_scheduler::HISTOGRAM_BUCKETS,
    )?;

    // Shutdown chain for the actor: token cancels → actor's select!
    // loop sees it → drops all worker stream_tx → build-exec-bridge
    // tasks exit → ReceiverStream closes → serve_with_shutdown
    // returns → SchedulerGrpc + AdminService drop their ActorHandle
    // clones → tick-loop also breaks and drops its →
    // all mpsc::Sender clones drop → actor's rx.recv() returns None
    // → actor exits → guard root drains the lease loop's step_down()
    // PATCH bounded → guard_join.join() returns.

    // ---- Guard domain (health + skew sentinel; lease joins below) ----
    // r[impl sched.lease.guard-isolated]
    // BEFORE init_db_pool: the guard thread binds `/healthz`
    // immediately, so the chart's livenessProbe (httpGet:/healthz on
    // :9194) passes during PG cold-start — `init_db_pool` below can
    // retry without a CrashLoopBackOff. sh-002 Stage C (the §lifecycle
    // strike-3 close): liveness, lease renewal, and the skew sentinel
    // are KILL-WIRED surfaces and live on a dedicated current_thread
    // runtime that stays schedulable when this (main) runtime is not;
    // readiness stays SHED-WIRED to this working domain — `/readyz`
    // is 503 until `ready` flips after the gRPC server binds, and
    // 503 again whenever the main runtime cannot schedule the guard's
    // probe within budget (brownout = Endpoints removal, never a
    // kill). The chart's startupProbe stays tcpSocket:9001 (gRPC bind
    // ≈ post-init_db_pool/migrations) — the guard binds before
    // init_db_pool, so an httpGet startupProbe would be vacuous.
    // Cross-domain state is lock-free only — the census lives in the
    // guard module doc.
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (guard, guard_join) = rio_scheduler::guard::spawn(
        tokio::runtime::Handle::current(),
        ready.clone(),
        rio_scheduler::guard::GuardConfig::default(),
        shutdown.clone(),
    );

    // F1 funnel (the fb2f42b57 shape): every `?`/`return` between
    // guard::spawn and .join() MUST be inside this block — the
    // post-await epilogue is the single discharge for guard_join
    // (sys.epilogue.drain; r26 irony-check on bug_023). The funnel
    // wraps init_db_pool(..)? + check_reference_epoch(..)? + both
    // HmacSigner::load(..)? so their early-returns are caught by
    // GuardJoin's drop-panic instead of silently exiting. `async
    // move` so the body OWNS `cfg`/`serve_shutdown`/`ready`/`guard`
    // (fields are moved out of `cfg` below); `shutdown` is cloned for
    // the post-funnel `shutdown.cancel()`.
    let funnel_shutdown = shutdown.clone();
    let run: anyhow::Result<()> = async move {
        let shutdown = funnel_shutdown;
        let (pool, db) = init_db_pool(&cfg.database_url, cfg.pg_auth, &shutdown).await?;
        // M_058: reference_hw_class change guard. Runs before any
        // ref-second state is read (CostTable, SlaEstimator). On mismatch
        // without --allow-reference-change, abort here — the persisted
        // build_samples / EMA state are denominated in the OLD reference
        // and would corrupt every fit.
        rio_scheduler::sla::check_reference_epoch(&db, &cfg.sla, cfg.allow_reference_change)
            .await?;
        let store_client = connect_store_lazy(&cfg.store.addr);
        // sh-036.1: ONE breaker-open mirror, cloned to both the actor
        // (writes via CacheCheckBreaker state transitions) and the gRPC
        // handler (reads for the off-actor FMP conditional timeout).
        let breaker_open: Arc<std::sync::atomic::AtomicBool> = Arc::default();

        if !cfg.soft_features.is_empty() {
            info!(soft_features = ?cfg.soft_features, "soft-feature stripping enabled");
        }

        // ---- Leader election (gated on RIO_LEASE_NAME) ----
        // None → non-K8s mode: is_leader=true immediately, generation
        // stays at 1. VM tests and single-scheduler deployments hit
        // this path.
        //
        // Some → K8s mode: is_leader=false until the lease loop
        // acquires. Standby replicas merge DAGs (state warm) but
        // don't dispatch (dispatch_ready early-returns). On acquire,
        // the lease loop derives the generation from the Lease's
        // transition count and flips is_leader; the advertised generation
        // stays 0 until recovery completes (claim-before-advertise), then
        // carries the post-recovery value — the stale-leader fence on
        // assignments is transaction-side (the pull mint checks the
        // durable claims floor).
        //
        // The generation Arc is constructed HERE (not inside the
        // actor) so both the actor and the lease task share the same
        // instance. spawn injects it into the actor via DagActorPlumbing,
        // REPLACING the actor's default Arc(1) — same init value,
        // shared reference.
        // The leader pod label feeds the rio-scheduler-leader Service
        // (helm scheduler.yaml): present on the leader's own pod, removed
        // on lose, so that Service's endpoints are exactly the current
        // leader. ClusterIP consumers that cannot retry a Trailers-Only
        // Unavailable from the standby (the dashboard's nginx upstream)
        // use it instead of the balanced channel.
        let lease_cfg = rio_scheduler::lease::LeaseConfig::from_parts(
            cfg.lease_name.clone(),
            cfg.lease_namespace.clone(),
        )
        .map(|c| {
            c.with_leader_pod_label(
                rio_scheduler::lease::LEADER_ROLE_LABEL,
                rio_scheduler::lease::LEADER_ROLE_LEADER,
            )
        });
        // 1 is the generation FLOOR, not a base for an increment: every
        // writer (the lease loop's on_acquire, recovery's PG seed) is a
        // fetch_max that can only raise it. 0 is reserved as the proto
        // "field unset" sentinel.
        let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
        let leader = match &lease_cfg {
            Some(cfg) => {
                info!(
                    lease = %cfg.lease_name,
                    namespace = %cfg.namespace,
                    holder = %cfg.holder_id,
                    "lease-based leader election enabled"
                );
                rio_scheduler::lease::LeaderState::pending(Arc::clone(&generation))
            }
            None => {
                info!("lease_name unset; running as sole leader (non-K8s mode)");
                rio_scheduler::lease::LeaderState::always_leader(Arc::clone(&generation))
            }
        };
        // Clone for the health toggle loop + gRPC layer + AdminService.
        // The actor and lease task each get a `LeaderState` clone (cheap
        // multi-Arc).
        let is_leader_for_health = leader.is_leader_arc();
        let is_leader_for_grpc = leader.is_leader_arc();
        let leader_for_admin = leader.clone();

        // Load HMAC signer for assignment tokens. None path = disabled
        // (unsigned tokens, dev mode). Bad path / empty file = startup
        // error (operator configured it, failing silently = workers can
        // upload arbitrary paths = security surprise).
        let hmac_signer = rio_auth::hmac::HmacSigner::load(cfg.hmac_key_path.as_deref())
            .map_err(|e| anyhow::anyhow!("HMAC key load: {e}"))?
            .map(Arc::new);
        if hmac_signer.is_some() {
            info!("HMAC assignment token signing enabled");
        }
        // Same key, second handle: the gRPC layer verifies executor-identity
        // tokens (r[sec.executor.identity-token+3]) the actor signed per
        // SpawnIntent. `HmacKey` has both sign+verify; the role aliases
        // (`HmacSigner`/`HmacVerifier`) are documentation only.
        let hmac_for_grpc = hmac_signer.clone();
        // Service-identity signer (SEPARATE key). Lets the dispatch-time
        // FOD store-check assert tenant context via x-rio-probe-tenant-id
        // — see r[sched.dispatch.fod-substitute+3].
        let service_signer = rio_auth::hmac::HmacSigner::load(cfg.service_hmac_key_path.as_deref())
            .map_err(|e| anyhow::anyhow!("service HMAC key load: {e}"))?;
        if service_signer.is_some() {
            info!("service-token signing enabled (dispatch-time substitution probe)");
        }
        // Same key, verifier role: AdminService gates the controller-only
        // mutating RPC (AppendInterruptSample) on x-rio-service-token
        // (the removed DrainExecutor rode the same gate in its day). HmacSigner == HmacVerifier (type alias);
        // re-load to get an independently-owned Arc for AdminServiceImpl.
        let service_verifier =
            rio_auth::hmac::HmacVerifier::load(cfg.service_hmac_key_path.as_deref())
                .map_err(|e| anyhow::anyhow!("service HMAC key load: {e}"))?
                .map(std::sync::Arc::new);
        if service_verifier.is_some() {
            info!("service-token verification enabled (controller-only AdminService RPCs)");
        }

        // ADR-023 phase-13: hw-band cost table. PG-backed (sla_ema_state)
        // so a restart doesn't re-warm; lease-gated poller below keeps it
        // fresh on the leader.
        let hw_cost_source = cfg.sla.hw_cost_source;
        let sla_cluster = cfg.sla.cluster.clone();
        // r[sched.sla.threat.corpus-clamp+3]: AdminServiceImpl needs the
        // [sla] block for ImportSlaCorpus param-range validation. Cloned
        // before cfg.sla is moved into DagActorConfig below.
        let sla_for_admin = std::sync::Arc::new(cfg.sla.clone());
        let cost_table = std::sync::Arc::new(parking_lot::RwLock::new(
            rio_scheduler::sla::cost::CostTable::load(
                &SchedulerDb::new(pool.clone()),
                &sla_cluster,
                hw_cost_source,
            )
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "cost-table load failed; starting from seeds");
                rio_scheduler::sla::cost::CostTable::seeded(&sla_cluster, hw_cost_source)
            }),
        ));
        // §13c-2: catalog-derived per-hwClass ceilings, fetched once at boot.
        // Spot-only — Static (vmtest) has no AWS API; ceilings fall to
        // `cfg.unwrap_or(global)`. Time-bounded so a misconfigured IRSA
        // doesn't hang boot — on timeout/error the ceilings fall to global
        // and the `_class_ceiling_uncatalogued` gauge fires per class.
        // r[impl scheduler.sla.ceiling.catalog-derived+4]
        if matches!(hw_cost_source, rio_scheduler::sla::cost::HwCostSource::Spot) {
            let ec2 = aws_sdk_ec2::Client::new(&aws_config::from_env().load().await);
            let catalog = match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                rio_scheduler::sla::catalog::fetch_catalog(&ec2),
            )
            .await
            {
                Ok(c) => c,
                Err(_) => {
                    tracing::warn!(
                        "§13c-2 instance-type catalog fetch timed out (30s); per-class \
                    ceilings fall to sla.maxCores/maxMem"
                    );
                    Vec::new()
                }
            };
            let ceilings = rio_scheduler::sla::catalog::derive_ceilings(
                &catalog,
                &cfg.sla.hw_classes,
                &cfg.sla.metal_sizes,
                &cfg.sla.unlaunchable_sizes,
            );
            info!(
                classes = ceilings.len(),
                total = cfg.sla.hw_classes.len(),
                "§13c-2 catalog ceilings derived"
            );
            cost_table.write().set_catalog_ceilings(ceilings);
        }
        // §13c-3: derive the effective global ceiling and run pass-2
        // validation. UNCONDITIONAL — runs after the (Spot-only)
        // `set_catalog_ceilings` block so the catalog is available; under
        // Static, `validate_shape()` already required `Some(maxCores)` so
        // this is a no-op resolve. The chicken-and-egg (Spot + transient
        // AWS hiccup → empty catalog → boot fail / CrashLoopBackOff) is
        // the explicit contract: an operator who left `maxCores=None`
        // opted into auto-derived globals; if derivation can't happen,
        // that's a config error, not a fallback.
        // r[impl scheduler.sla.global.derive+2]
        let (resolved_global, source) = {
            let ct = cost_table.read();
            cfg.sla.resolve_globals(ct.catalog_ceilings())?
        };
        {
            let ct = cost_table.read();
            cfg.sla
                .validate_resolved(resolved_global, ct.catalog_ceilings(), source)?;
        }
        cost_table.write().set_resolved_global(resolved_global);
        info!(
            max_cores = resolved_global.0,
            max_mem = resolved_global.1,
            source,
            "§13c-3 global ceiling resolved"
        );
        // λ refresh + sweep + persist run regardless of `hw_cost_source`
        // (the controller appends `interrupt_samples` even under Static).
        // `inputs_gen` is derived from the table at poll time — pollers
        // just write; nobody bumps. `cost_was_leader` is shared between both
        // pollers and the actor; its writer set is the
        // observability::LEADER_EDGES cost-latch cells (lose + rebound
        // deliveries) plus poller_tick_prelude's steady-state edges — see
        // the registry, never a prose list. The spot poller reads it to
        // skip one body on the false→true edge so its first fold lands
        // post-reload.
        let cost_was_leader = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cost_reload_notify = std::sync::Arc::new(tokio::sync::Notify::new());
        rio_common::task::spawn_monitored(
            "sla-interrupt-housekeeping",
            rio_scheduler::sla::cost::interrupt_housekeeping(
                SchedulerDb::new(pool.clone()),
                leader.clone(),
                std::sync::Arc::clone(&cost_table),
                std::sync::Arc::clone(&cost_was_leader),
                std::sync::Arc::clone(&cost_reload_notify),
                shutdown.clone(),
            ),
        );
        // Spot-price poller (and the staleness gauge / clamp it owns) are
        // Spot-only — under Static there is no live source to be "stale
        // relative to".
        if matches!(hw_cost_source, rio_scheduler::sla::cost::HwCostSource::Spot) {
            rio_common::task::spawn_monitored(
                "sla-cost-poller",
                rio_scheduler::sla::cost::spot_price_poller(
                    leader.clone(),
                    std::sync::Arc::clone(&cost_table),
                    std::sync::Arc::clone(&cost_was_leader),
                    shutdown.clone(),
                ),
            );
        }
        // sh-018b: SLA-estimator refresh runs OFF the actor turn (it
        // was the surviving 15-30s phase-00 stall under completion
        // bursts). The actor and this task share `sla_estimator` /
        // `solve_cache` via Arc; the actor only reads. The poller-
        // liveness gauge is emitted by the ACTOR (so it climbs on a
        // poller panic), not here.
        let sla_estimator = std::sync::Arc::new(rio_scheduler::sla::SlaEstimator::new(&cfg.sla));
        let solve_cache: std::sync::Arc<rio_scheduler::sla::solve::SolveCache> =
            std::sync::Arc::default();
        rio_common::task::spawn_monitored(
            "sla-estimator-refresh",
            rio_scheduler::sla::estimator_poller(
                SchedulerDb::new(pool.clone()),
                leader.clone(),
                std::sync::Arc::clone(&sla_estimator),
                std::sync::Arc::clone(&solve_cache),
                cfg.sla.solve_tiers(),
                // Same cadence as the on-actor path it replaced
                // (`ESTIMATOR_REFRESH_EVERY` × tick): 60s in
                // production; the sla-sizing VM fixture's
                // `tick_interval_secs=2` → 12s.
                cfg.tick_interval * 6,
                shutdown.clone(),
            ),
        );

        // Spawn the DAG actor with the shared leader state. Poison +
        // retry come from scheduler.toml (or `#[serde(default)]` if
        // absent — same behavior unless the operator writes a `[poison]`
        // / `[retry]` table).
        let actor = ActorHandle::spawn(
            db.clone(),
            rio_scheduler::actor::DagActorConfig {
                soft_features: cfg.soft_features,
                poison: cfg.poison,
                retry_policy: cfg.retry,
                sla: cfg.sla,
                establishment_report_slack: cfg.establishment_report_slack,
                exec_retention_days: cfg.exec_retention_days,
                materialization: cfg.materialization,
                ..Default::default()
            },
            rio_scheduler::actor::DagActorPlumbing {
                store_client: store_client.clone(),
                breaker_open: Arc::clone(&breaker_open),
                hmac_signer,
                service_signer: service_signer.map(Arc::new),
                leader: leader.clone(),
                // The pod identity recorded on (and compared against)
                // leader_generation_claims rows — the same-epoch re-claim
                // discriminator. Empty in non-K8s mode, and unused there
                // too: with no lease loop, LeaderAcquired never fires, so
                // recovery (and with it the generation claim) never runs.
                holder_id: lease_cfg
                    .as_ref()
                    .map(|c| c.holder_id.clone())
                    .unwrap_or_default(),
                cost_table: std::sync::Arc::clone(&cost_table),
                cost_was_leader,
                cost_reload_notify,
                sla_estimator: Some(sla_estimator),
                solve_cache: Some(solve_cache),
                shutdown: shutdown.clone(),
            },
        );
        info!("DAG actor spawned");

        // Spawn the lease loop ON THE GUARD RUNTIME (sh-002C). AFTER
        // actor spawn so the actor's generation is already the shared
        // Arc — when the lease acquires and applies the lease-derived
        // generation (fetch_max), the actor sees it. Kill-wired (D1):
        // renewal keeps its 5s cadence during main-domain stalls (the
        // 16.35s Tick, sh-002 Stage B), so the lease's fence-check
        // premise survives admitted-load starvation. The loop builds its
        // own kube client on the guard runtime — no main-domain pool
        // sharing. The loop owes a shutdown epilogue (the
        // graceful-release PATCH): its DrainHandle is adopted into the
        // guard root, which drains it bounded before the runtime drops
        // (sys.epilogue.drain; bug_118).
        //
        // SchedulerLeaseHooks::new(&actor) STAYS ON THIS (main) RUNTIME:
        // the `lease-hook-forwarder` task it spawns
        // (lease_hooks.rs:72, ambient `tokio::spawn` via
        // `spawn_monitored`) `.await`s the actor's bounded mailbox and
        // must not land on the guard `current_thread` runtime — keeps
        // the guard's cross-domain census intact (nothing the guard
        // polls depends on main-domain mailbox drain). Only the
        // constructed `hooks` value (an `UnboundedSender`, sync
        // `.send()`) crosses into the guard. `serving_generation` needs
        // no new plumbing: the actor stamps it itself inside
        // `handle_leader_acquired` (recovery.rs, from PG `claim_target`,
        // main runtime); the guard-hosted lease loop reaches the actor
        // via the EXISTING shared `Arc<AtomicU64>` inside `LeaderState`
        // + the hooks' `UnboundedSender` — both runtime-agnostic.
        if let Some(lease_cfg) = lease_cfg {
            let hooks = rio_scheduler::lease_hooks::SchedulerLeaseHooks::new(&actor);
            guard.adopt_epilogue(guard.spawn_lease(lease_cfg, leader, hooks, shutdown.clone()));
        }

        // grpc.health.v1.Health. SERVING iff is_leader. K8s Service
        // routes only to SERVING pods → only to the leader. Standby
        // replicas stay live (liveness probe passes) but not ready.
        //
        // Toggle loop tracks is_leader every 1s. In non-K8s mode
        // is_leader=true immediately → first iteration sets SERVING.
        // In K8s standby mode: stays NOT_SERVING until lease acquire.
        //
        // r[impl ctrl.probe.named-service]
        // The CLIENT-SIDE balancer (rio-proto/src/client/balance.rs) probes
        // the NAMED service `rio.scheduler.SchedulerService` to find the
        // leader — set_not_serving only affects named services, empty-string
        // stays SERVING forever after first set_serving. A balancer probing
        // "" would route to standby.
        //
        // CRITICAL — K8S PROBES ARE A DIFFERENT LAYER: scheduler.yaml's
        // readiness/liveness use httpGet on the GUARD-DOMAIN axum server
        // (`/readyz` + `/healthz` on :9194 — sched.lease.guard-isolated);
        // the startupProbe stays tcpSocket:9001 (gRPC bind ≈ post-migrations
        // — the guard binds before init_db_pool, so an httpGet startupProbe
        // would be vacuous). DO NOT "fix" the manifest to grpc probes —
        // that crash-loops the standby (gRPC health reports NOT_SERVING
        // until lease acquire; if liveness goes grpc, standby gets SIGKILLed
        // → restart → still standby → loop). The guard's `/healthz` is an
        // unconditional 200 (process alive + the dedicated runtime
        // schedulable); `/readyz` is 503 until `ready` flips below AND the
        // main runtime answers a probe within budget — leader-election is
        // NOT a readiness gate (both replicas Ready; clients route via the
        // health-aware balancer which DOES check is_leader).
        let (health_reporter, health_service) = tonic_health::server::health_reporter();

        // Two-stage shutdown — see rio_common::server::spawn_drain_task
        // for the INDEPENDENT-token rationale. The closure flips the
        // NAMED SchedulerService: BalancedChannel probes that name to
        // find the leader (empty-string stays SERVING forever after
        // first set_serving — probing "" would route to standby).
        //
        // The health-toggle loop below breaks on the SAME parent token
        // and its break arm does NOT call set_serving — so it cannot
        // un-flip us here. Last write wins.
        let reporter = health_reporter.clone();
        rio_common::server::spawn_drain_task(
            shutdown.clone(),
            serve_shutdown.clone(),
            cfg.common.drain_grace,
            move || async move {
                reporter
                    .set_not_serving::<SchedulerServiceServer<SchedulerGrpc>>()
                    .await;
            },
        );

        spawn_health_toggle(
            health_reporter.clone(),
            is_leader_for_health,
            shutdown.clone(),
        );

        // Create gRPC services.
        let grpc_service = SchedulerGrpc::new(
            actor.clone(),
            db,
            Arc::clone(&is_leader_for_grpc),
            // jwt_mode from config (not from `jwt_pubkey.is_some()` —
            // that's loaded below the server-builder for hot-reload
            // wiring; the path being set is what determines mode).
            cfg.jwt.key_path.is_some(),
            hmac_for_grpc,
            // Substitution-replacement: the same service-HMAC verifier the
            // AdminService uses, here verifying the store's kind-attested
            // materialization credential (ServiceClaims caller="rio-store")
            // on the materialization-only ExecutorService operations.
            service_verifier.clone(),
            OffActorProbe {
                store_client,
                breaker_open,
            },
        );

        // Background refresh for ClusterStatus.store_size_bytes — 60s PG poll
        // on the shared DB. Keeps ClusterStatus fast (autoscaler's 30s path).
        let store_size_bytes =
            rio_scheduler::admin::spawn_store_size_refresh(pool.clone(), shutdown.clone());

        // build_samples retention: delete rows older than 30 days, hourly.
        // 30d bounds the SLA estimator's sample set (ADR-023) with margin
        // for cold-restart refresh + operator forensics.
        //
        // Fresh SchedulerDb from pool.clone() — `db` was moved into the
        // actor at ActorHandle::spawn above. PgPool is
        // Arc-backed; SchedulerDb::new is just { pool }, so this is a
        // 1-pointer clone. Placed before AdminServiceImpl::new which
        // terminally moves `pool`.
        {
            let db = SchedulerDb::new(pool.clone());
            rio_common::task::spawn_periodic(
                "build-samples-retention",
                std::time::Duration::from_secs(3600),
                shutdown.clone(),
                move || {
                    let db = db.clone();
                    async move {
                        match db.delete_samples_older_than(30).await {
                            Ok(0) => {}
                            Ok(n) => info!(rows_deleted = n, "build_samples retention sweep"),
                            Err(e) => tracing::warn!(?e, "build_samples retention failed"),
                        }
                    }
                },
            );
        }

        let admin_service = AdminServiceImpl::new(
            pool,
            actor.clone(),
            cfg.store.addr.clone(),
            store_size_bytes,
            leader_for_admin,
            shutdown.clone(),
            sla_cluster,
            sla_for_admin,
            service_verifier,
            cost_table,
        );

        // Start periodic tick task. Actor-dead handling: try_send fails
        // silently once the channel closes; the shutdown token (cancelled
        // by the actor's drop path) stops the loop shortly after. No
        // early-break needed — spawn_periodic's biased; shutdown wins.
        let tick_actor = actor.clone();
        rio_common::task::spawn_periodic(
            "tick-loop",
            cfg.tick_interval,
            shutdown.clone(),
            move || {
                let tick_actor = tick_actor.clone();
                async move {
                    if tick_actor
                        .try_send(rio_scheduler::actor::ActorCommand::Tick)
                        .is_err()
                        && !tick_actor.is_alive()
                    {
                        tracing::warn!("actor channel closed; tick dropped");
                    }
                }
            },
        );

        // Start gRPC server
        let listen_addr = cfg.listen_addr;
        let max_message_size = rio_common::grpc::max_message_size();

        // r[impl sec.jwt.pubkey-mount+2]
        // JWT pubkey from ConfigMap mount (if configured) + SIGHUP reload
        // loop. kubelet remounts the ConfigMap on rotation; operator
        // SIGHUPs the pod; the spawned reload task re-reads + swaps the
        // Arc<RwLock> the interceptor closure captured below.
        //
        // cfg.jwt.key_path is set via RIO_JWT__KEY_PATH env, itself set by
        // helm _helpers.tpl (`rio.mounts` with want=jwtVerify) when
        // .Values.jwt.enabled. Without the mount → key_path stays None →
        // interceptor inert → silent fail-open. The helm triplet is the
        // real impl; this marker is the Rust-side anchor tracey can see.
        //
        // Parent shutdown token: reload loop stops on SIGTERM instantly,
        // not after the drain window. See load_and_wire_jwt docstring for
        // the None→inert / Some→fail-fast semantics.
        let jwt_pubkey = rio_auth::jwt_interceptor::load_and_wire_jwt(
            cfg.jwt.key_path.as_deref(),
            shutdown.clone(),
        )?;

        info!(
            listen_addr = %listen_addr,
            store_addr = %cfg.store.addr,
            max_message_size,
            jwt = jwt_pubkey.is_some(),
            "starting gRPC server"
        );

        // Working domain ready: every dependency-connect `?` above has
        // returned, the actor is spawned, and the gRPC server is about
        // to bind. The guard's `/readyz` flips to 200 the moment the
        // main runtime answers a probe within budget; the chart's
        // startupProbe (tcpSocket:9001) gates on the bind itself.
        ready.store(true, std::sync::atomic::Ordering::Relaxed);

        // r[impl dash.envoy.grpc-web-translate+3]
        // accept_http1: gRPC-Web arrives as HTTP/1.1 POST from browser
        // fetch(); GrpcWebLayer needs the h1 codec enabled. Native gRPC
        // clients keep negotiating h2 — both protocols on one port.
        // r[impl dash.stream.idle-timeout+3]
        // Server-initiated h2 PINGs keep long-lived server streams
        // (WatchBuild, TriggerGC) alive through any proxy's idle-timeout
        // (replacing the Envoy Gateway ClientTrafficPolicy
        // `streamIdleTimeout: 1h`) — supplied by `tonic_builder()` itself
        // since the keepalive hoist; the hand-chained override that used
        // to live here is exactly what the `h2-keepalive-single-source`
        // check now forbids.
        rio_common::server::tonic_builder()
            .accept_http1(true)
            // Layer order: first .layer() = outermost. CORS must see the
            // OPTIONS preflight before GrpcWebLayer (which would reject
            // a non-grpc-web content-type). GrpcWebLayer translates
            // application/grpc-web+proto → application/grpc before the
            // JWT interceptor and the tonic services see the request.
            .layer(build_cors_layer(&cfg.dashboard))
            .layer(tonic_web::GrpcWebLayer::new())
            // JWT tenant-token verify layer. jwt_pubkey computed above —
            // None (dev/unset) → inert pass-through; Some → verify every
            // x-rio-tenant-token header the gateway sets.
            //
            // Installed unconditionally (not `if jwt_pubkey.is_some()`) so
            // the builder type stays stable across the None/Some branch —
            // no `InterceptedService<_, F>` vs plain server type divergence.
            //
            // Permissive-on-absent-header: health/worker/admin callers don't
            // set x-rio-tenant-token → pass-through. Only the gateway sets
            // it; only gateway-originated calls get verified. See the
            // module docs in rio-common for the coexistence table.
            .layer(tonic::service::InterceptorLayer::new(
                rio_auth::jwt_interceptor::jwt_interceptor(jwt_pubkey),
            ))
            .add_service(health_service)
            .add_service(
                SchedulerServiceServer::new(grpc_service.clone())
                    .max_decoding_message_size(max_message_size)
                    .max_encoding_message_size(max_message_size),
            )
            .add_service(
                ExecutorServiceServer::new(grpc_service)
                    .max_decoding_message_size(max_message_size)
                    .max_encoding_message_size(max_message_size),
            )
            .add_service(
                AdminServiceServer::new(admin_service)
                    .max_decoding_message_size(max_message_size)
                    .max_encoding_message_size(max_message_size),
            )
            .serve_with_shutdown(listen_addr, serve_shutdown.cancelled_owned())
            .await?;

        info!("scheduler shutting down");
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
    info!("scheduler shut down cleanly");
    run
}

/// CORS for browser gRPC-Web: the single-sourced dashboard contract
/// (`rio_common::cors` — shared with rio-store, bug_355). Replaces
/// the Envoy Gateway `SecurityPolicy` CRD (D3 cascade); origins come
/// from `RIO_DASHBOARD__CORS_ALLOW_ORIGINS` (comma-separated).
fn build_cors_layer(cfg: &DashboardConfig) -> tower_http::cors::CorsLayer {
    rio_common::cors::dashboard_cors_layer(&cfg.cors_allow_origins)
}

// ── bootstrap helpers (extracted from main) ──────────────────────────

/// Connect to PostgreSQL and run migrations. Separate from the rest of
/// main() so the migration call site (`rio_migrations::migrator()` →
/// `rio_migrations::migrate::run`) is obvious in the boot sequence.
///
/// Bounded retry (8 tries, exponential 1→16s): when systemd starts PG
/// and the scheduler near-simultaneously, or after a PG restart within
/// the `RestartSec` window, a one-shot connect crash-loops the
/// scheduler. (The store connect uses [`connect_store_lazy`] instead —
/// lazy never fails at creation; first-RPC-connects-or-fails.)
/// Unlike the store connect, exhaustion here IS fatal — scheduler
/// without PG can't recover state, can't persist, can't serve.
/// Retryable = `pg_error::is_transient` only: a PERMANENT error (bad
/// password 28P01, undefined object) crashes immediately instead of
/// burning ~5 minutes of exhaustion first — "crash-on-permanent" is
/// the scheduler's contract, and `connect_with_retry`'s retry-all
/// predicate would make it false. Each attempt re-mints via
/// `TokenSource::fresh_options` (in IAM mode a retry loop can outlive
/// a token; mint failures classify transient and ride the backoff).
async fn init_db_pool(
    database_url: &str,
    pg_auth: rio_common::config::PgAuthMode,
    shutdown: &rio_common::signal::Token,
) -> anyhow::Result<(sqlx::PgPool, SchedulerDb)> {
    use rio_common::backoff::{Backoff, Jitter, RetryError};
    const MAX_TRIES: u32 = 8;
    // Same curve as rio_proto::client::connect_with_retry (1s→16s,
    // ±25% jitter) — only the retryable-predicate differs.
    const CONNECT_BACKOFF: Backoff = Backoff {
        base: std::time::Duration::from_secs(1),
        mult: 2.0,
        cap: std::time::Duration::from_secs(16),
        jitter: Jitter::Proportional(0.25),
    };
    // Config preflight (bad URL, weak TLS, missing rootcert, missing
    // AWS region) fails HERE, before any retry — crash-loops visibly.
    let tokens =
        std::sync::Arc::new(rio_common::pg_iam::TokenSource::new(database_url, pg_auth).await?);
    let tokens_for_retry = std::sync::Arc::clone(&tokens);
    let pool = match rio_common::backoff::retry(
        &CONNECT_BACKOFF,
        MAX_TRIES,
        shutdown,
        rio_common::pg_error::is_transient_anyhow,
        |n, e| {
            tracing::warn!(
                error = format!("{e:#}"),
                tries = n,
                "PG connect failed; retrying"
            )
        },
        || {
            let tokens = std::sync::Arc::clone(&tokens_for_retry);
            async move {
                // r[impl store.db.pool-idle-timeout+2]
                // Aurora's max_connections is runtime-constant (derived
                // from MAX capacity, 2,000-capped at min_capacity ≤ 0.5 —
                // see infra/eks/rds.tf). idle_timeout=60s +
                // min_connections=2 shrinks a burst-grown pool back to
                // baseline so idle conns don't count against the fixed
                // budget (I-171). See rio-store init_db_pool for the full
                // budget and the IAM connect-rate watch item.
                let opts = tokens.fresh_options().await?;
                sqlx::postgres::PgPoolOptions::new()
                    .max_connections(10)
                    .min_connections(2)
                    .idle_timeout(std::time::Duration::from_secs(60))
                    .connect_with(opts)
                    .await
                    .map_err(anyhow::Error::from)
            }
        },
    )
    .await
    {
        Ok(pool) => pool,
        Err(RetryError::Cancelled) => {
            anyhow::bail!("shutdown during PostgreSQL connect")
        }
        Err(RetryError::Exhausted { last, attempts }) => {
            anyhow::bail!("PostgreSQL connect failed after {attempts} tries: {last:#}")
        }
    };
    info!("connected to PostgreSQL");
    tokens.spawn_refresher(pool.clone());

    // r[impl store.db.migrate-try-lock] — same try-then-wait advisory
    // lock as rio-store. Both services run the SAME migration set
    // against the SAME database; sqlx's default blocking
    // `pg_advisory_lock` deadlocks against migrations 011/022's CREATE
    // INDEX CONCURRENTLY when ≥2 replicas (of either service) start
    // together (I-194). Raw `Migrator::run` here would also lock on a
    // DIFFERENT key (sqlx hashes the DB name) than rio-store's
    // `MIGRATE_LOCK_ID`, so a scheduler and a store starting together
    // would not mutually exclude. See rio_migrations::migrate::run.
    rio_migrations::migrate::run(&pool, rio_migrations::migrator()).await?;
    info!("database migrations applied");

    let db = SchedulerDb::new(pool.clone());
    Ok((pool, db))
}

/// Lazy-connect store client for scheduler-side cache checks + CA-cutoff
/// verification.
///
/// Lazy, not eager: the scheduler holds this client for its entire
/// lifetime. An eager connect caches the pod IP that DNS resolved to AT
/// STARTUP. When the store Deployment rolls (helm upgrade, config
/// change), the old pod terminates, kube-dns re-resolves `rio-store` to
/// the new pod IP, but the eager Channel still points at the old IP —
/// RPCs fail with connection-refused and the scheduler never recovers
/// without a restart. Observed during P0473 rsb testing: substitution
/// RPCs silently went dark after a store rollout.
///
/// [`connect_store_lazy`](rio_proto::client::connect_store_lazy)
/// builds the Endpoint with `connect_lazy()` (re-resolves DNS on
/// each reconnect) + the hoisted h2 keepalive consts (30 s interval /
/// 10 s timeout, while-idle; `rio_common::grpc`, pinned by the
/// `h2-keepalive-single-source` check — detects half-open connections
/// within ~40 s). The channel
/// transparently reconnects to the new pod. The Endpoint
/// building lives in rio-proto so it can reuse the process-global
/// the other `connect_*` helpers.
///
/// No retry loop: lazy never fails at creation time (only on malformed
/// addr, which is a config bug → fatal). First RPC connects; if the
/// store isn't up yet (systemd near-simultaneous start, PG migration
/// deadlock per the old doc-comment), that RPC gets `Unavailable` and
/// the cache-check circuit breaker opens — the NEXT RPC after the
/// breaker's half-open interval retries and succeeds once store is up.
/// Cache-check degrades gracefully instead of being permanently
/// disabled.
// r[impl sched.store-client.reconnect]
fn connect_store_lazy(
    store_addr: &str,
) -> Option<rio_proto::StoreServiceClient<tonic::transport::Channel>> {
    match rio_proto::client::connect_store_lazy(store_addr) {
        Ok(client) => {
            info!(%store_addr, "store channel created (lazy; connects on first RPC)");
            Some(client)
        }
        Err(e) => {
            // Only malformed addr / bad TLS config reach here — config
            // bugs, not transient. Still non-fatal: cache-check-disabled
            // is a degraded mode, not a crash.
            tracing::warn!(
                %store_addr, error = %e,
                "store channel creation failed (malformed addr?); \
                 scheduler-side cache check + CA cutoff disabled"
            );
            None
        }
    }
}

/// Edge-triggered health-toggle loop: tracks `is_leader` every 1s and
/// flips the gRPC HealthReporter's SchedulerService status.
///
/// Checks every second — short enough that leadership transitions
/// surface quickly (K8s readiness probe period is typically 5-10s, so
/// we update before it checks).
///
/// Why not watch the AtomicBool directly: there's no async
/// wake-on-change for atomics. A `tokio::sync::watch` channel would
/// give that, but then the lease task and `dispatch_ready` both need to
/// be adapted to use watch instead of AtomicBool. Polling at 1Hz is
/// simpler and the 1s lag is imperceptible (K8s probes poll slower).
///
/// Edge-triggered: only call set_serving/set_not_serving on a
/// TRANSITION, not every iteration. tonic-health `set_*` is an async
/// RwLock write + broadcast to Watch subscribers — not expensive, but
/// calling it 1Hz for no reason wakes any grpc Health.Watch clients
/// (K8s probes don't use Watch, but other tooling might).
///
/// Stateful: `prev` is cross-tick mutable state, so not
/// `spawn_periodic` (FnMut can't lend `&mut` across `.await`).
/// `biased;` inlined per `r[common.task.periodic-biased]`.
fn spawn_health_toggle(
    reporter: tonic_health::server::HealthReporter,
    is_leader: Arc<std::sync::atomic::AtomicBool>,
    shutdown: rio_common::signal::Token,
) {
    rio_common::task::spawn_monitored("health-toggle-loop", async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        // `prev`: what we LAST set the reporter to. Starts None so
        // the first iteration unconditionally sets (either SERVING
        // or NOT_SERVING depending on is_leader at that moment).
        // Option<bool> not bool: "haven't set anything yet" is
        // distinct from both true and false.
        let mut prev: Option<bool> = None;
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    tracing::debug!("health-toggle-loop shutting down");
                    break;
                }
                _ = interval.tick() => {}
            }
            let now = is_leader.load(std::sync::atomic::Ordering::Relaxed);
            if prev != Some(now) {
                if now {
                    reporter
                        .set_serving::<SchedulerServiceServer<SchedulerGrpc>>()
                        .await;
                    tracing::debug!("health: SERVING (is_leader=true)");
                } else {
                    reporter
                        .set_not_serving::<SchedulerServiceServer<SchedulerGrpc>>()
                        .await;
                    tracing::debug!("health: NOT_SERVING (is_leader=false, standby)");
                }
                prev = Some(now);
            }
        }
    });
}
