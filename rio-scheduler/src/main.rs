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
use rio_scheduler::grpc::SchedulerGrpc;

mod config;
use config::{CliArgs, Config, DashboardConfig};

/// Scheduler-specific lease transition hooks: emit `rio_scheduler_lease_*`
/// metrics and fire-and-forget `LeaderAcquired`/`LeaderLost` to the actor.
///
/// `tokio::spawn` for the actor send: the lease loop calls these hooks
/// synchronously from the renewal tick, and a blocked hook (>15s) would
/// stall the tick → lease expires → another replica acquires → dual-leader
/// (see `rio_lease::LeaseHooks` doc). `send_unchecked` bypasses
/// backpressure — control message, not work submission.
#[derive(Clone)]
struct SchedulerLeaseHooks {
    actor: ActorHandle,
}

impl rio_scheduler::lease::LeaseHooks for SchedulerLeaseHooks {
    fn on_acquire(&self) {
        // Counter for VM test observability: vm-phase3a's lease smoke
        // test polls this to confirm the lease loop actually acquired
        // (vs silently failing kube-client init and running standby
        // forever). The info! log has the same signal but metrics are
        // less brittle for VM grep.
        metrics::counter!("rio_scheduler_lease_acquired_total").increment(1);
        let actor = self.actor.clone();
        tokio::spawn(async move {
            if let Err(e) = actor
                .send_unchecked(rio_scheduler::actor::ActorCommand::LeaderAcquired)
                .await
            {
                // Actor dead. We're holding the lease but can't
                // dispatch. Not much to do — the process is probably
                // crashing.
                tracing::error!(error = %e, "failed to send LeaderAcquired (actor dead?)");
            }
        });
    }

    fn on_lose(&self) {
        metrics::counter!("rio_scheduler_lease_lost_total").increment(1);
        let actor = self.actor.clone();
        tokio::spawn(async move {
            if let Err(e) = actor
                .send_unchecked(rio_scheduler::actor::ActorCommand::LeaderLost)
                .await
            {
                tracing::error!(error = %e, "failed to send LeaderLost (actor dead?)");
            }
        });
    }
}

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
    // clones → tick-loop + lease-loop also break and drop theirs →
    // all mpsc::Sender clones drop → actor's rx.recv() returns None
    // → actor exits → drops event_persist_tx → event-persister also
    // exits (channel-close). event_log::spawn doesn't need a token.

    let (pool, db) = init_db_pool(&cfg.database_url, &shutdown).await?;
    // M_058: reference_hw_class change guard. Runs before any
    // ref-second state is read (CostTable, SlaEstimator). On mismatch
    // without --allow-reference-change, abort here — the persisted
    // build_samples / EMA state are denominated in the OLD reference
    // and would corrupt every fit.
    rio_scheduler::sla::check_reference_epoch(&db, &cfg.sla, cfg.allow_reference_change).await?;
    let store_client = connect_store_lazy(&cfg.store.addr);

    // Shared log ring buffers. Written by the BuildExecution recv task
    // (inside SchedulerGrpc), drained by the flusher, read by AdminService.
    let log_buffers = std::sync::Arc::new(rio_scheduler::logs::LogBuffers::new());
    let (log_flush_tx, admin_s3) = init_log_pipeline(
        cfg.log_s3_bucket.as_deref(),
        pool.clone(),
        Arc::clone(&log_buffers),
    )
    .await;

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
    // the lease loop increments generation and flips is_leader;
    // workers see the new gen in their next heartbeat and reject
    // stale-gen assignments from the old leader.
    //
    // The generation Arc is constructed HERE (not inside the
    // actor) so both the actor and the lease task share the same
    // instance. spawn injects it into the actor via DagActorPlumbing,
    // REPLACING the actor's default Arc(1) — same init value,
    // shared reference.
    let lease_cfg = rio_scheduler::lease::LeaseConfig::from_parts(
        cfg.lease_name.clone(),
        cfg.lease_namespace.clone(),
    );
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

    // Spawn the event-log persister. Bounded mpsc + single drain
    // task → FIFO write ordering (fire-and-forget spawns would
    // race on the PG pool). emit_build_event try_sends here; if
    // backed up, the broadcast still carries the event — only a
    // mid-backlog gateway reconnect loses it.
    let event_persist_tx = rio_scheduler::event_log::spawn(pool.clone());

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
    // tokens (r[sec.executor.identity-token+2]) the actor signed per
    // SpawnIntent. `HmacKey` has both sign+verify; the role aliases
    // (`HmacSigner`/`HmacVerifier`) are documentation only.
    let hmac_for_grpc = hmac_signer.clone();
    // Service-identity signer (SEPARATE key). Lets the dispatch-time
    // FOD store-check assert tenant context via x-rio-probe-tenant-id
    // — see r[sched.dispatch.fod-substitute+2].
    let service_signer = rio_auth::hmac::HmacSigner::load(cfg.service_hmac_key_path.as_deref())
        .map_err(|e| anyhow::anyhow!("service HMAC key load: {e}"))?;
    if service_signer.is_some() {
        info!("service-token signing enabled (dispatch-time substitution probe)");
    }
    // Same key, verifier role: AdminService gates controller-only
    // mutating RPCs (AppendInterruptSample, DrainExecutor) on
    // x-rio-service-token. HmacSigner == HmacVerifier (type alias);
    // re-load to get an independently-owned Arc for AdminServiceImpl.
    let service_verifier = rio_auth::hmac::HmacVerifier::load(cfg.service_hmac_key_path.as_deref())
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
    // r[impl scheduler.sla.ceiling.catalog-derived]
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
    // r[impl scheduler.sla.global.derive]
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
    // pollers and written ONLY by interrupt_housekeeping (the
    // edge-reload owner); the spot poller reads it to skip one body on
    // the false→true edge so its first fold lands post-reload.
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
            substitute_max_concurrent: cfg.substitute_max_concurrent,
            sla: cfg.sla,
            ..Default::default()
        },
        rio_scheduler::actor::DagActorPlumbing {
            store_client,
            log_flush_tx,
            log_buffers: Some(Arc::clone(&log_buffers)),
            event_persist_tx: Some(event_persist_tx),
            hmac_signer,
            service_signer: service_signer.map(Arc::new),
            leader: leader.clone(),
            cost_table: std::sync::Arc::clone(&cost_table),
            cost_was_leader,
            cost_reload_notify,
            shutdown: shutdown.clone(),
        },
    );
    info!("DAG actor spawned");

    // Spawn the lease loop (if configured). AFTER actor spawn so
    // the actor's generation is already the shared Arc — when the
    // lease acquires and increments, the actor sees it.
    // Capture the handle: the lease loop calls step_down() on
    // shutdown (graceful release, saves ~15s on rollouts). That's
    // an async K8s API call that needs time to complete — if we
    // drop the handle and let main() race to exit, the process
    // dies before the PATCH lands and we're back to TTL expiry.
    let lease_loop = lease_cfg.map(|lease_cfg| {
        // Hooks fire-and-forget LeaderAcquired/LeaderLost. The lease
        // loop does NOT block on recovery — it keeps renewing while
        // the actor handles LeaderAcquired.
        rio_common::task::spawn_monitored(
            "lease-loop",
            rio_scheduler::lease::run_lease_loop(
                lease_cfg,
                leader,
                SchedulerLeaseHooks {
                    actor: actor.clone(),
                },
                shutdown.clone(),
            ),
        )
    });

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
    // CRITICAL — K8S PROBES ARE A DIFFERENT LAYER: scheduler.yaml uses
    // tcpSocket, NOT grpc. DO NOT "fix" the manifest to grpc probes —
    // that crash-loops the standby (gRPC health reports NOT_SERVING
    // until lease acquire; if liveness goes grpc, standby gets SIGKILLed
    // → restart → still standby → loop). TCP-accept succeeding is the
    // correct readiness/liveness signal for standby: the process is
    // live, the port is bound, leader-election is the ONLY thing
    // blocking serve.
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

    // Create gRPC services. All three get the SAME Arc<LogBuffers>:
    // SchedulerGrpc writes, AdminService reads (live), LogFlusher drains
    // (on completion). The test-only new_for_tests() constructor makes a
    // SEPARATE buffer — it's cfg(test) gated so prod can't accidentally
    // use it and silently break the pipeline.
    let grpc_service = SchedulerGrpc::with_log_buffers(
        actor.clone(),
        Arc::clone(&log_buffers),
        db,
        Arc::clone(&is_leader_for_grpc),
        Arc::clone(&generation),
        // jwt_mode from config (not from `jwt_pubkey.is_some()` —
        // that's loaded below the server-builder for hot-reload
        // wiring; the path being set is what determines mode).
        cfg.jwt.key_path.is_some(),
        hmac_for_grpc,
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
        log_buffers,
        admin_s3,
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

    // r[impl sec.jwt.pubkey-mount]
    // JWT pubkey from ConfigMap mount (if configured) + SIGHUP reload
    // loop. kubelet remounts the ConfigMap on rotation; operator
    // SIGHUPs the pod; the spawned reload task re-reads + swaps the
    // Arc<RwLock> the interceptor closure captured below.
    //
    // cfg.jwt.key_path is set via RIO_JWT__KEY_PATH env, itself set by
    // helm _helpers.tpl (rio.jwtVerifyEnv/VolumeMount/Volume) when
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
        log_s3_bucket = ?cfg.log_s3_bucket,
        jwt = jwt_pubkey.is_some(),
        "starting gRPC server"
    );

    // r[impl dash.envoy.grpc-web-translate+3]
    // accept_http1: gRPC-Web arrives as HTTP/1.1 POST from browser
    // fetch(); GrpcWebLayer needs the h1 codec enabled. Native gRPC
    // clients keep negotiating h2 — both protocols on one port.
    // r[impl dash.stream.idle-timeout]
    // http2_keep_alive_interval: 30s server-initiated PING keeps
    // long-lived server streams (GetBuildLogs, WatchBuild) alive
    // through any proxy's idle-timeout. Replaces the Envoy Gateway
    // ClientTrafficPolicy `streamIdleTimeout: 1h` — the stream is
    // never idle from the proxy's view.
    rio_common::server::tonic_builder()
        .accept_http1(true)
        .http2_keepalive_interval(Some(std::time::Duration::from_secs(30)))
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

    // Wait for step_down() to complete. serve_with_shutdown has
    // already returned (cancel token fired), so the lease loop has
    // seen the same signal and is on its way out — this join is
    // bounded by step_down's ~3s timeout (RENEW_INTERVAL -
    // RENEW_SLOP). Ignore the JoinError: it's only set if the lease
    // task panicked, which spawn_monitored already logged, and we're
    // shutting down regardless.
    if let Some(h) = lease_loop {
        let _ = h.await;
    }

    info!("scheduler shut down cleanly");
    Ok(())
}

/// CORS layer for browser gRPC-Web. Replaces the Envoy Gateway
/// `SecurityPolicy` CRD (D3 cascade). Origins come from
/// `RIO_DASHBOARD__CORS_ALLOW_ORIGINS` (comma-separated); the three
/// `expose_headers` are what connect-web reads to surface
/// `Status.code`/`.message` to the SPA — without them the browser
/// blocks the trailer headers and every RPC error renders as
/// `Code.Unknown`.
fn build_cors_layer(cfg: &DashboardConfig) -> tower_http::cors::CorsLayer {
    use http::{HeaderName, Method};
    use tower_http::cors::{AllowOrigin, CorsLayer};

    CorsLayer::new()
        .allow_origin(AllowOrigin::list(parse_cors_origins(
            &cfg.cors_allow_origins,
        )))
        .allow_methods([Method::POST, Method::OPTIONS])
        .allow_headers([
            HeaderName::from_static("content-type"),
            HeaderName::from_static("x-grpc-web"),
            HeaderName::from_static("x-user-agent"),
        ])
        .expose_headers([
            HeaderName::from_static("grpc-status"),
            HeaderName::from_static("grpc-message"),
            HeaderName::from_static("grpc-status-details-bin"),
        ])
}

/// Parse a comma-separated CORS origin list (helm renders
/// `| join ","`): split, trim, drop empties, drop unparseable.
/// Extracted from `build_cors_layer` so the split/trim/filter chain
/// is directly assertable — `CorsLayer`'s internal origin list isn't
/// inspectable, so a constructibility check alone is vacuous.
pub(crate) fn parse_cors_origins(raw: &str) -> Vec<http::HeaderValue> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|o| {
            http::HeaderValue::from_str(o)
                .inspect_err(|e| tracing::warn!(origin = o, error = %e, "invalid CORS origin"))
                .ok()
        })
        .collect()
}

// ── bootstrap helpers (extracted from main) ──────────────────────────

/// Connect to PostgreSQL and run migrations. Separate from the rest of
/// main() so the sqlx::migrate!() macro expansion (compile-time SQL
/// checksum validation) has an obvious call site.
///
/// Bounded retry (8 tries, exponential 1→16s): when systemd starts PG
/// and the scheduler near-simultaneously, or after a PG restart within
/// the `RestartSec` window, a one-shot connect crash-loops the
/// scheduler. (The store connect uses [`connect_store_lazy`] instead —
/// lazy never fails at creation; first-RPC-connects-or-fails.)
/// Unlike the store connect, exhaustion here IS fatal — scheduler
/// without PG can't recover state, can't persist, can't serve.
async fn init_db_pool(
    database_url: &str,
    shutdown: &rio_common::signal::Token,
) -> anyhow::Result<(sqlx::PgPool, SchedulerDb)> {
    use rio_proto::client::RetryError;
    const MAX_TRIES: u32 = 8;
    let pool = match rio_proto::client::connect_with_retry(
        shutdown,
        || {
            // r[impl store.db.pool-idle-timeout]
            // Aurora Serverless v2 scales max_connections with ACU; at
            // min_capacity=0.5 that's ~105 usable slots. idle_timeout=60s
            // + min_connections=2 shrinks a burst-grown pool back to
            // baseline so idle conns don't count against Aurora's limit
            // (I-171). See rio-store init_db_pool for the full budget.
            sqlx::postgres::PgPoolOptions::new()
                .max_connections(10)
                .min_connections(2)
                .idle_timeout(std::time::Duration::from_secs(60))
                .connect(database_url)
        },
        Some(MAX_TRIES),
    )
    .await
    {
        Ok(pool) => pool,
        Err(RetryError::Cancelled) => {
            anyhow::bail!("shutdown during PostgreSQL connect")
        }
        Err(RetryError::Exhausted { last, attempts }) => {
            anyhow::bail!("PostgreSQL connect failed after {attempts} tries: {last}")
        }
    };
    info!("connected to PostgreSQL");

    // r[impl store.db.migrate-try-lock] — same try-then-wait advisory
    // lock as rio-store. Both services run the SAME migration set
    // against the SAME database; sqlx's default blocking
    // `pg_advisory_lock` deadlocks against migrations 011/022's CREATE
    // INDEX CONCURRENTLY when ≥2 replicas (of either service) start
    // together (I-194). Raw `Migrator::run` here would also lock on a
    // DIFFERENT key (sqlx hashes the DB name) than rio-store's
    // `MIGRATE_LOCK_ID`, so a scheduler and a store starting together
    // would not mutually exclude. See rio_common::migrate::run.
    rio_common::migrate::run(&pool, sqlx::migrate!("../migrations")).await?;
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
/// each reconnect) + `http2_keep_alive_interval(30s)` /
/// `keep_alive_timeout(10s)` / `keep_alive_while_idle(true)`
/// (detects half-open connections within ~40s). The channel
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

type LogFlushTx = tokio::sync::mpsc::Sender<rio_scheduler::logs::FlushRequest>;
type AdminS3 = (aws_sdk_s3::Client, String);

/// Log flusher + AdminService S3 setup.
///
/// Both need the same S3 client (if configured) — build it once, clone
/// where needed. Without `RIO_LOG_S3_BUCKET`, logs are ring-buffer-only
/// (lost on restart, still live-servable while running) and
/// AdminService can only serve active-derivation logs.
async fn init_log_pipeline(
    bucket: Option<&str>,
    pool: sqlx::PgPool,
    log_buffers: Arc<rio_scheduler::logs::LogBuffers>,
) -> (Option<LogFlushTx>, Option<AdminS3>) {
    let Some(bucket) = bucket else {
        tracing::warn!(
            "RIO_LOG_S3_BUCKET not set; build logs will be ring-buffer-only \
             (lost on scheduler restart, AdminService can't serve completed logs)"
        );
        return (None, None);
    };
    // Same client builder as rio-store's chunk backend (raised retry
    // attempts, stalled-stream protection OFF — compressed log batches
    // are small pre-buffered bodies, exactly the case that trips the
    // sdk's stall monitor on S3-compatible servers). One config home
    // for both services. See rio_common::s3::default_client.
    let s3 = rio_common::s3::default_client(rio_common::s3::DEFAULT_S3_MAX_ATTEMPTS).await;
    let (flush_tx, flush_rx) = tokio::sync::mpsc::channel(1000);
    let flusher =
        rio_scheduler::logs::LogFlusher::new(s3.clone(), bucket.to_owned(), pool, log_buffers);
    flusher.spawn(flush_rx);
    info!(%bucket, "log flusher spawned");
    (Some(flush_tx), Some((s3, bucket.to_owned())))
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
