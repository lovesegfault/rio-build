use std::sync::Arc;

use clap::Parser;
use sqlx::postgres::PgPoolOptions;
use tracing::{error, info};

use rio_proto::ChunkServiceServer;
use rio_proto::LogServiceServer;
use rio_proto::StoreAdminServiceServer;
use rio_proto::StoreServiceServer;
use rio_store::backend::ChunkBackend;
use rio_store::grpc::{ChunkServiceImpl, StoreAdminServiceImpl, StoreServiceImpl};
use rio_store::logs::LogServiceImpl;
use rio_store::logs::chunks::{
    FilesystemLogChunkStore, LogChunkStore, MemoryLogChunkStore, S3LogChunkStore,
};
use rio_store::logs::ingest::IngestConfig;
use rio_store::signing::{Signer, TenantSigner};
use rio_store::substitute::Substituter;

use rio_store::config::{ChunkBackendKind, CliArgs, Config, StoreCommand, init_chunk_backend};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // One-shot diagnostic mode for `xtask deploy`'s pg preflight: a
    // throwaway pod runs `rio-store pg-preflight` to MEASURE the live
    // server's max_connections (the value the rds.tf model predicts —
    // see infra/eks/rds.tf locals). Intercepted before CliArgs::parse
    // so the positional mode word never reaches the config layering.
    if std::env::args().nth(1).as_deref() == Some("pg-preflight") {
        return pg_preflight().await;
    }
    let cli = CliArgs::parse();
    if let Some(StoreCommand::Migrate) = cli.command {
        return run_migrate().await;
    }
    let rio_common::server::Bootstrap::<Config> {
        cfg,
        shutdown,
        serve_shutdown,
        otel_guard: _otel_guard,
        root_span: _root_span,
    } = rio_common::server::bootstrap(
        "store",
        cli,
        rio_store::describe_metrics,
        rio_store::HISTOGRAM_BUCKETS,
    )?;

    let pool = init_db_pool(&cfg.database_url, cfg.pg_auth, cfg.pg_max_connections).await?;

    // grpc.health.v1.Health. The NAMED `rio.store.StoreService` is unset
    // (probe → NotFound, which kubelet treats as failure) until the
    // `set_serving::<StoreServiceServer>` call below flips it. K8s
    // readinessProbe (store.yaml `grpc:{service: rio.store.StoreService}`)
    // hits the named service — readiness fails until the schema check
    // passes means the Service doesn't route to a half-booted pod. NOTE:
    // tonic-health defaults the EMPTY-string service to SERVING; the
    // store's probe doesn't check "" so that default is harmless here,
    // but DON'T copy this pattern to a binary that probes "" (see
    // `rio_common::server::health_reporter_not_serving`).
    //
    // Ordering: health_reporter() → build services → set_serving() →
    // serve(). The set_serving happens BEFORE serve() blocks, which means
    // the very first health check after listen returns SERVING. That's
    // correct: by the time we're listening, the schema check passed. If
    // it failed (migrate Job hasn't run yet), the `?` above already
    // bailed and the pod restarts until the schema is current.
    let (health_reporter, health_service) = tonic_health::server::health_reporter();

    let chunk_cache = init_chunk_backend(
        &cfg.chunk_backend,
        cfg.chunk_cache_capacity_bytes,
        cfg.s3_max_attempts,
        cfg.chunk_upload_global_permits,
    )
    .await?;

    // Load the narinfo signing key. `None` path → `None` signer (not
    // an error — signing is optional). Bad path / bad format → `?`
    // (operator configured a key; failing silently = unsigned paths
    // = security surprise).
    let signer = Signer::load(cfg.signing_key_path.as_deref())
        .map_err(|e| anyhow::anyhow!("signing key load failed: {e}"))?;
    if signer.is_some() {
        info!(path = ?cfg.signing_key_path, "narinfo signing enabled");
    }

    // HMAC verifier for assignment tokens. Same key file as the
    // scheduler's signer. None → accept all PutPath (dev mode).
    let hmac_verifier = rio_auth::hmac::HmacVerifier::load(cfg.hmac_key_path.as_deref())
        .map_err(|e| anyhow::anyhow!("HMAC key load: {e}"))?;
    if hmac_verifier.is_some() {
        info!("HMAC assignment token verification enabled on PutPath");
    }
    let service_verifier = rio_auth::hmac::HmacVerifier::load(cfg.service_hmac_key_path.as_deref())
        .map_err(|e| anyhow::anyhow!("service-HMAC key load: {e}"))?
        .map(Arc::new);
    if service_verifier.is_some() {
        info!("x-rio-service-token verification enabled on PutPath + StoreAdminService");
    }
    let service_verifier_for_authz = service_verifier.clone();

    // Tenant-aware signer (with prior-cluster-key history). Computed
    // before the StoreServiceImpl chain so the side-effecty PG load +
    // log don't break up the builder calls below. Wrap the cluster
    // Signer in a TenantSigner — per-tenant key lookup hits
    // `tenant_keys` on the same PG pool. Paths without tenant
    // attribution (service-token caller, dev mode — see
    // `StoreServiceImpl::verify_assignment_token`) fall through to the
    // cluster
    // key inside resolve_once (via maybe_sign).
    let tenant_signer = match signer {
        Some(s) => {
            // Prior cluster keys for sig_visibility_gate after rotation.
            // Loaded once at startup — rotation is a human-driven op that
            // restarts the store anyway (new Signer = new process).
            let prior = TenantSigner::load_prior_cluster(&pool, s.key_name())
                .await
                .map_err(|e| anyhow::anyhow!("cluster_key_history load: {e}"))?;
            if !prior.is_empty() {
                info!(
                    count = prior.len(),
                    "loaded prior cluster keys for sig-gate"
                );
            }
            Some(TenantSigner::new(s, pool.clone()).with_prior_cluster(prior))
        }
        None => None,
    };

    // Substitute admission gate: ONE instance, cloned into both
    // StoreServiceImpl (acquires) and StoreAdminServiceImpl (reports
    // via GetLoad). Constructed before either builder chain so the
    // share is explicit — two `AdmissionGate::new()` calls would
    // compile but GetLoad would always read 0.0.
    // r[impl store.materialize.gate-share+1]
    // ONE effective value feeds the gate AND the executor path-slot
    // pool (live_047/R-C): the pool tracks the override, so the
    // documented `substituteAdmissionPermits` lever can never hand
    // the executor the whole gate.
    let admission_cap = rio_store::config::effective_substitute_admission_cap(
        cfg.substitute_admission_permits,
        cfg.pg_max_connections,
    );
    info!(admission_cap, "substitute admission gate");
    let substitute_admission = rio_store::admission::AdmissionGate::new(admission_cap);
    let executor_path_slots = rio_store::materialize::executor::PathSlotPool::new(
        rio_store::config::derive_executor_path_slots(substitute_admission.capacity()),
    );

    // StoreServiceImpl: one constructor + builder chain. Unconditional
    // builders chain directly; Option<T> from config applies via
    // `if let` so each builder keeps its concrete-argument signature
    // for tests. Config-is-single-source-of-truth fields
    // always replace the constructor default; nar_buffer_budget only
    // overrides when explicitly set.
    let mut store_service = StoreServiceImpl::new(pool.clone())
        .with_chunk_upload_max_concurrent(cfg.chunk_upload_max_concurrent)
        .with_max_batch_paths(cfg.max_batch_paths)
        .with_chunk_prefetch_k(cfg.chunk_prefetch_k);
    if let Some(cache) = &chunk_cache {
        store_service = store_service.with_chunk_cache(Arc::clone(cache));
    }
    if let Some(ts) = tenant_signer {
        store_service = store_service.with_signer(ts);
    }
    // Same verifier shared with DirectoryServiceImpl below — both gate
    // on `x-rio-assignment-token`.
    let hmac_verifier_arc = hmac_verifier.map(Arc::new);
    if let Some(v) = hmac_verifier_arc.clone() {
        store_service = store_service.with_hmac_verifier(v);
    }
    store_service = store_service.with_service_bypass_callers(cfg.service_bypass_callers);
    if let Some(v) = service_verifier.clone() {
        store_service = store_service.with_service_hmac_verifier(v);
    }
    if let Some(budget) = cfg.nar_buffer_budget_bytes {
        info!(
            budget_bytes = budget,
            "NAR buffer budget overridden from config"
        );
        // `as usize`: lossless on 64-bit; on 32-bit (not a supported
        // target) it would truncate, but so would DEFAULT_NAR_BUDGET.
        store_service = store_service.with_nar_budget(budget as usize);
    }

    // Substituter: upstream binary-cache fetch-on-miss. Shares the same
    // chunk backend as PutPath (NAR chunks go to the same S3 bucket)
    // and the same TenantSigner (for sig_mode=add|replace). Always
    // enabled — a tenant with zero `tenant_upstreams` rows makes
    // `list_for_tenant` return [], which is a fast no-op.
    let chunk_backend: Option<Arc<dyn ChunkBackend>> = chunk_cache.as_ref().map(|c| c.backend());
    let substituter = {
        let mut s = Substituter::new(pool.clone(), chunk_backend)
            .with_chunk_upload_max_concurrent(cfg.chunk_upload_max_concurrent)
            .with_nar_budget(store_service.nar_budget().clone())
            .with_admission_gate(substitute_admission.clone())
            .with_stall_window(cfg.substitute_stall);
        if let Some(signer) = store_service.signer() {
            s = s.with_signer(Arc::clone(signer));
        }
        Arc::new(s)
    };
    // Clone (not move) into the StoreService: the materialization
    // executor spawner below shares the same Substituter instance —
    // one admission gate, one singleflight, one HTTP pool per replica.
    let store_service = store_service.with_substituter(Arc::clone(&substituter));

    // r[impl store.shutdown.drain-getpath]
    // Three-stage shutdown — see rio_common::server::spawn_drain_task
    // for the INDEPENDENT-token rationale + proof test. Closure flips
    // the NAMED StoreService (BalancedChannel probe target). The
    // after-grace hook waits for in-flight GetPath body streams to
    // complete (or `stream_drain` to elapse) BEFORE serve_shutdown
    // tears down the listener — KEDA scale-down (the store
    // ScaledObject's damped -1 pod / 600s policy) assumes SIGTERM
    // drains in-flight work.
    // Spawned here (not at the top of main) because the active-stream
    // counter lives on store_service.
    let reporter = health_reporter.clone();
    let active_streams = store_service.active_get_path_streams_handle();
    let stream_drain = cfg.stream_drain;
    rio_common::server::spawn_drain_task_ext(
        shutdown.clone(),
        serve_shutdown.clone(),
        cfg.common.drain_grace,
        move || async move {
            reporter
                .set_not_serving::<StoreServiceServer<StoreServiceImpl>>()
                .await;
        },
        move || async move {
            rio_common::server::wait_for_active_drain(&active_streams, stream_drain).await;
        },
    );

    // ChunkServiceImpl: same cache Arc. None → FAILED_PRECONDITION
    // on GetChunk, which is correct for an inline-only store (there
    // ARE no chunks to get). The pool is for HasChunks' durable-
    // presence probe — a pure PG read that works without a cache. The
    // verifier is HasChunks' caller-identity gate.
    let chunk_service =
        ChunkServiceImpl::new(pool.clone(), chunk_cache.clone(), hmac_verifier_arc.clone());

    // Tenant-scoped via JWT or HMAC assignment-token claim — see
    // grpc/directory.rs. ReadBlob shares the chunk cache; the signer
    // feeds the sig-visibility fallback's trusted-key set (same
    // TenantSigner as the StoreService, so the validity gate and the
    // castore reads derive identical trust).
    let directory_service = rio_store::grpc::DirectoryServiceImpl::new(
        pool.clone(),
        hmac_verifier_arc.clone(),
        chunk_cache.clone(),
        store_service.signer().cloned(),
    );

    // LogService: build-log ingest + tailing. The log chunk store
    // follows the NAR chunk backend's kind: S3 → the same bucket under
    // the `logs/` prefix (a separate client so log PUTs never contend
    // with NAR chunk uploads for the SDK's connection pool); filesystem
    // → a `logs/` subdirectory of the same base dir (chunks survive a
    // process restart — the property the standalone VM scenario
    // exercises); inline → in-memory (build logs survive only as long
    // as the process, loudly logged).
    let log_chunk_store: Arc<dyn LogChunkStore> = match &cfg.chunk_backend {
        ChunkBackendKind::S3 { bucket, .. } => {
            let client = rio_common::s3::default_client(cfg.s3_max_attempts).await;
            Arc::new(S3LogChunkStore::new(client, bucket.clone()))
        }
        ChunkBackendKind::Filesystem { base_dir } => Arc::new(
            FilesystemLogChunkStore::new(base_dir.join("logs"))
                .map_err(|e| anyhow::anyhow!("creating the log chunk directory: {e}"))?,
        ),
        ChunkBackendKind::Inline => {
            tracing::warn!(
                "no chunk backend configured: build-log chunks are stored \
                 in process memory and will NOT survive a store restart"
            );
            Arc::new(MemoryLogChunkStore::default())
        }
    };
    // The replica identity routes cross-replica TailLog readers to the
    // replica holding an execution's live ingest buffer: it is written
    // into `log_ingest_sessions.replica_pod` and substituted into
    // `log_peer_url_template`'s `{pod}` by the reader's replica, so it
    // must be something the template turns into a DIALABLE URL.
    //
    // RIO_STORE_REPLICA_ID is preferred: the helm chart injects
    // `status.podIP` via the downward API and pairs it with an
    // IP-based template (`http://{pod}:9002`; the store brackets IPv6
    // identities itself at dial time) — a Deployment's pods
    // get no per-pod DNS A records (no `hostname`/`subdomain` in the
    // pod spec), so the HOSTNAME pod name is registrable but not
    // resolvable. Fallback to HOSTNAME (kubelet sets it to the pod
    // name) keeps single-replica and dev deployments working: with one
    // replica the proxy path never fires, so resolvability doesn't
    // matter. NOT a Config field: this is pod identity injected by the
    // orchestrator, not operator-tunable configuration.
    let replica_pod = std::env::var("RIO_STORE_REPLICA_ID")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "rio-store-dev".to_string());
    let mut log_service = LogServiceImpl::new(
        pool.clone(),
        Arc::clone(&log_chunk_store),
        replica_pod.clone(),
    )
    .with_ingest_config(IngestConfig {
        per_exec_byte_cap: cfg.log_ingest_byte_cap,
        cut_threshold_bytes: cfg.log_cut_threshold_bytes,
        cut_interval: cfg.log_cut_interval,
    })
    .with_max_streams(cfg.log_max_streams)
    .with_byte_budget(cfg.log_bytes_budget)
    .with_max_chunks_per_exec(cfg.log_max_chunks_per_exec)
    .with_peer_url_template(cfg.log_peer_url_template.clone());
    if cfg.log_peer_url_template.is_empty() {
        // Fail-closed posture: without a template this replica cannot
        // dial peers, so a reader landing on a non-owning replica gets
        // the history-only view (laggy but correct). One warn at boot;
        // rio_store_log_tail_proxy_failures_total never fires because
        // the proxy is never attempted.
        tracing::warn!(
            "log_peer_url_template is empty: the cross-replica live-tail proxy is \
             disabled; set RIO_LOG_PEER_URL_TEMPLATE (helm: store.logPeerUrlTemplate) \
             to enable live tails across replicas"
        );
    }
    // Same HMAC key as PutPath: the assignment token authorizes both
    // the output upload and the log stream for one build attempt.
    let log_hmac = rio_auth::hmac::HmacVerifier::load(cfg.hmac_key_path.as_deref())
        .map_err(|e| anyhow::anyhow!("HMAC key load (LogService): {e}"))?;
    let log_hmac_configured = log_hmac.is_some();
    if let Some(v) = log_hmac {
        log_service = log_service.with_hmac_verifier(Arc::new(v));
    } else {
        info!("HMAC verification disabled on AppendLog (dev mode)");
    }
    info!(%replica_pod, "LogService enabled");
    // live062-R3: detach the shutdown release obligation before the
    // service moves into the router below.
    let ingest_shutdown = log_service.ingest_shutdown_handle();

    // StoreAdminServiceImpl: TriggerGC + VerifyChunks + upstream CRUD
    // + GetLoad. Gets the chunk backend directly (for key_for in
    // sweep's pending_s3_deletes enqueue + VerifyChunks HeadObject).
    // None for inline-only stores — sweep does CASCADE delete only.
    //
    // Also spawn GC background tasks (orphan placeholder scanner +
    // chunk-collect backstop + drain). All periodic
    // (15min / daily / 30s). The hourly orphan-chunk sweep is gone:
    // never-referenced chunks past grace are ordinary collect-cycle
    // victims now (run_gc phase 3 / the daily backstop).
    // spawn_monitored: if one panics, logged; store keeps serving
    // (degraded GC, not down).
    let chunk_backend_for_gc: Option<Arc<dyn ChunkBackend>> =
        chunk_cache.as_ref().map(|c| c.backend());
    let admin_service = StoreAdminServiceImpl::new(pool.clone(), chunk_backend_for_gc.clone())
        .with_shutdown(shutdown.clone())
        .with_service_verifier(service_verifier)
        .with_substitute_admission(substitute_admission.clone());
    rio_store::gc::orphan::spawn_scanner(pool.clone(), shutdown.clone());
    // Store gauge self-publication (30s in-process tick): the PG-pool
    // AND substitute-admission utilization gauges, from their owning
    // data sources. No gauge may depend on RPC traffic for freshness:
    // with the store scaled by KEDA there is no periodic GetLoad
    // caller, and a frozen gauge blanks the store dashboard panels +
    // `xtask k8s status` (obs.metric.store-gauge-ownership).
    rio_store::grpc::spawn_store_gauge_tick(pool.clone(), substitute_admission, shutdown.clone());
    // Daily chunk-collect backstop (live arm): covers stores that
    // never trigger GC, so bounded garbage retention has a worst-case
    // cadence (24h + grace + drain lag). The first tick fires one
    // full interval after boot: pod boot/scale-up/crash-loops never
    // trigger the cycle (the heaviest query pattern in the system).
    // Takes GC_LOCK_ID non-blocking and skips when a GC run (which
    // already runs the cycle as phase 3) or another replica's backstop
    // is in flight. The backend is needed for the collect batches'
    // pending_s3_deletes enqueue (None on inline-only stores — the
    // soft-delete still happens, there is just no S3 key to enqueue).
    rio_store::gc::collect::spawn_collect_backstop(
        pool.clone(),
        chunk_backend_for_gc.clone(),
        shutdown.clone(),
    );
    // Every replica publishes the cluster GC gauges from the durable
    // gc_collect_state row (merged_bug_211): the cycle winner is no
    // longer the only pod whose gauges move — replicas converge on
    // the row value within one 60s period. Replicated-fact semantics:
    // aggregate with max(), never sum() (owner decision Q6).
    rio_store::gc::state::spawn_gc_gauge_publisher(pool.clone(), shutdown.clone());
    if let Some(backend) = chunk_backend_for_gc {
        rio_store::gc::drain::spawn_drain_task(pool.clone(), backend, shutdown.clone());
    }
    // Build-log TTL sweep: hourly, deletes executions (and their chunk
    // objects) older than log_retention_days. The store owns log
    // retention end to end — the scheduler never touches log storage.
    rio_store::logs::sweep::spawn_log_sweep(
        pool.clone(),
        Arc::clone(&log_chunk_store),
        std::time::Duration::from_secs(u64::from(cfg.log_retention_days) * 86_400),
        shutdown.clone(),
    );

    // Substitution-replacement Phase A: the materialization-job
    // executor (design §2.2). The spawner carries the dormancy gate —
    // enabled=false (the default) spawns NOTHING and this call is a
    // no-op; enabled=true runs cfg.materialization.executor_concurrency
    // claim loops against the scheduler's ExecutorService, presenting
    // the kind-attested store-service credential minted from the same
    // service-HMAC key file the verifier side mounts.
    let materialization_signer =
        rio_auth::hmac::HmacSigner::load(cfg.service_hmac_key_path.as_deref())
            .map_err(|e| anyhow::anyhow!("service-HMAC key load (materialization): {e}"))?
            .map(Arc::new);
    rio_store::materialize::spawn_materialization_executor(
        cfg.materialization.clone(),
        pool.clone(),
        Arc::clone(&substituter),
        materialization_signer,
        executor_path_slots,
        shutdown.clone(),
    );

    let max_msg_size = rio_common::grpc::max_message_size();

    let addr = cfg.listen_addr;

    // PG is connected, migrations applied, services constructed.
    // Everything that can fail-fast has. SERVING.
    //
    // The type param is the service struct, not the generated Server
    // wrapper. tonic-health uses it for the per-service name (clients
    // can check "rio.store.StoreService" specifically). We only
    // register one — the empty-string "whole server" check falls through
    // to this when no specific service is named.
    health_reporter
        .set_serving::<StoreServiceServer<StoreServiceImpl>>()
        .await;

    // JWT pubkey from ConfigMap mount + SIGHUP reload loop. One
    // gateway signing key → one pubkey across all verifier services →
    // same ConfigMap mount path, same SIGHUP rotation story as
    // scheduler. See load_and_wire_jwt docstring for None→inert /
    // Some→fail-fast. Parent shutdown token: reload loop stops on
    // SIGTERM instantly, same disposition as orphan-scanner/GC-drain.
    let jwt_pubkey = rio_auth::jwt_interceptor::load_and_wire_jwt(
        cfg.jwt.key_path.as_deref(),
        shutdown.clone(),
    )?;

    // Boot key-coherence: jwt => (service && hmac). The half-keyed
    // states are refused HERE, naming the missing knob — the authz
    // layer's per-class dual-mode never has to reason about them
    // (rio-authz-kernel::key_coherence; bughunt2 bug_237).
    // r[impl store.authz.key-coherence]
    rio_store::authz::validate_key_coherence(rio_store::authz::VerifierConfig {
        jwt: jwt_pubkey.is_some(),
        service: service_verifier_for_authz.is_some(),
        hmac: log_hmac_configured,
    })
    .map_err(|e| anyhow::anyhow!(e))?;

    info!(
        addr = %addr,
        max_msg_size,
        jwt = jwt_pubkey.is_some(),
        "starting gRPC server"
    );

    // r[impl dash.envoy.grpc-web-translate+3]
    // accept_http1 + CORS + GrpcWebLayer: the dashboard SPA calls
    // LogService.TailLog from browser fetch() as gRPC-Web over
    // HTTP/1.1. Same layer stack and ordering as the scheduler's admin
    // server (CORS outermost so it sees the OPTIONS preflight before
    // GrpcWebLayer rejects the non-grpc content type; GrpcWebLayer
    // translates to native gRPC before the JWT interceptor and the
    // services see the request). The layers wrap EVERY service on the
    // port — that is harmless: gRPC-Web is a transport encoding, not an
    // auth change, and native h2 callers (builders, the gateway) are
    // untouched. The browser still cannot call PutPath/AppendLog
    // usefully without the HMAC tokens those handlers demand.
    rio_common::server::tonic_builder()
        .accept_http1(true)
        .layer(rio_common::cors::dashboard_cors_layer(
            &cfg.log_cors_allow_origins,
        ))
        .layer(tonic_web::GrpcWebLayer::new())
        // JWT tenant-token verify layer. jwt_pubkey computed above.
        // Installed unconditionally for type stability (see
        // scheduler/main.rs for the full note).
        //
        // Permissive-on-absent matters MORE for store than scheduler:
        // workers call StoreService.PutPath with HMAC assignment tokens
        // (no JWT). If absent-header were a rejection, worker uploads
        // would break the moment the pubkey is configured. Same layer
        // wrapping ChunkService/StoreAdminService is harmless — those
        // callers never set x-rio-tenant-token either.
        .layer(tonic::service::InterceptorLayer::new(
            rio_auth::jwt_interceptor::jwt_interceptor(jwt_pubkey.clone()),
        ))
        // Per-method credential-class enforcement, AFTER the JWT
        // interceptor (it consumes the claims the interceptor
        // attaches). Fails closed on undeclared methods; per-class
        // enforcement is enforce-when-configured. See authz.rs.
        // r[impl store.log.method-credential+2]
        .layer(rio_store::authz::AuthzLayer {
            jwt_configured: jwt_pubkey.is_some(),
            hmac_configured: log_hmac_configured,
            service_verifier: service_verifier_for_authz,
        })
        .add_service(health_service)
        .add_service(
            StoreServiceServer::new(store_service)
                .max_decoding_message_size(max_msg_size)
                .max_encoding_message_size(max_msg_size),
        )
        .add_service(
            ChunkServiceServer::new(chunk_service)
                .max_decoding_message_size(max_msg_size)
                .max_encoding_message_size(max_msg_size),
        )
        .add_service(
            rio_proto::DirectoryServiceServer::new(directory_service)
                .max_decoding_message_size(max_msg_size)
                .max_encoding_message_size(max_msg_size),
        )
        .add_service(
            StoreAdminServiceServer::new(admin_service)
                .max_decoding_message_size(max_msg_size)
                .max_encoding_message_size(max_msg_size),
        )
        .add_service(
            // The log plane's decode cap is the KERNEL chunk bound, not
            // the store-wide max_msg_size knob (bug_298, §5-Q22: 16 MiB
            // const, no knob): an AppendLog message larger than one
            // chunk's payload ceiling has no legitimate producer — the
            // builder flushes at 64 lines / 64 KiB-truncated each — and
            // every byte tonic decodes here is allocation the admission
            // gate cannot refuse retroactively. Encoding stays on the
            // shared knob (TailLog responses are re-chunked to <=256
            // lines, far below either bound).
            LogServiceServer::new(log_service)
                .max_decoding_message_size(rio_log_kernel::MAX_CHUNK_PAYLOAD_BYTES as usize)
                .max_encoding_message_size(max_msg_size),
        )
        .serve_with_shutdown(addr, serve_shutdown.cancelled_owned())
        .await?;

    // live062-R3: discharge the graceful-shutdown lease-release
    // obligation BEFORE process exit — the runtime drop is about to
    // kill any detached ingest drivers still tearing down, and their
    // per-driver releases die with them. Without this, an evicted
    // replica's session rows linger for the full staleness window and
    // every reconnecting builder burns it in 1 Hz refused retries.
    ingest_shutdown.release_live_sessions().await;

    info!("store shut down cleanly");
    Ok(())
}

/// `rio-store pg-preflight`: connect via `RIO_DATABASE_URL`, print the
/// server's `max_connections` in `key=value` form, exit. The output
/// contract (`max_connections=N` on stdout) is parsed by
/// `xtask::k8s::eks::deploy`'s pg-preflight step — change both
/// together. Uses a single connection and no migrations: this mode
/// must work against a fully-booted production database without side
/// effects.
async fn pg_preflight() -> anyhow::Result<()> {
    use anyhow::Context;
    let url = std::env::var("RIO_DATABASE_URL")
        .context("pg-preflight requires RIO_DATABASE_URL (the rio-postgres secret's url key)")?;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .context("pg-preflight: connect failed")?;
    let max_connections: i32 =
        sqlx::query_scalar("SELECT setting::int FROM pg_settings WHERE name = 'max_connections'")
            .fetch_one(&pool)
            .await
            .context("pg-preflight: SELECT max_connections failed")?;
    println!("max_connections={max_connections}");
    Ok(())
}

// ── bootstrap helpers (extracted from main) ──────────────────────────

/// `rio-store migrate`: one-shot migration runner. This is the
/// container entrypoint of the helm `rio-migrate` Job and the
/// ExecStart of the NixOS `rio-migrate` systemd oneshot — migrations
/// run out-of-band, BEFORE any app pod/service starts, and always as
/// the database master — schema DDL never depends on the credentials
/// or privileges of whatever auth mode the app pods use. The same
/// run reconciles the `rio_app` role and its grants
/// (`rio_migrations::ensure_roles`), so a fresh cluster deploys
/// directly in `postgres.authMode=iam`: the role exists before any
/// pod connects as it.
///
/// Reads `RIO_DATABASE_URL` directly instead of loading the full
/// store `Config` — the Job sets exactly this one variable, and the
/// full config would drag in store-only concerns (chunk backend,
/// listen addrs) a migration run never touches. Always password-mode:
/// the runners hand it the master URL.
///
/// Connect retry: on fresh k3s installs the bitnami PG StatefulSet
/// lands in the same helm release as this Job, so PG can trail by
/// minutes (image pull + initdb). A flat 5s poll for up to 10
/// minutes rides that out inside ONE Job pod — no CrashLoop backoff
/// amplification; the Job `backoffLimit` stays a backstop for real
/// failures (which surface fast: auth errors and bad SQL don't
/// retry here).
async fn run_migrate() -> anyhow::Result<()> {
    use anyhow::Context as _;

    // Same single-provider guard as rio_common::server::bootstrap —
    // this subcommand skips bootstrap entirely, and a future
    // transitive dep re-enabling `ring` would otherwise re-create the
    // rustls dual-provider can't-auto-select panic on the first
    // verify-full handshake to Aurora. Defense-in-depth; the live
    // path works today.
    rio_common::server::install_crypto_provider();

    let _otel_guard = rio_common::observability::init_tracing("store")?;
    let url = std::env::var("RIO_DATABASE_URL")
        .context("`rio-store migrate` requires RIO_DATABASE_URL")?;
    info!(
        url = %rio_common::config::redact_db_url(&url),
        "running database migrations"
    );

    // SIGTERM/SIGINT → cancel (this subcommand skips
    // rio_common::server::bootstrap and with it the usual signal
    // wiring). On node drain, kubelet SIGTERMs the Job pod: the
    // select! below drops the in-flight migrate future — the detached
    // lock connection closes, PG aborts the running statement
    // server-side and releases the advisory lock — and main returns
    // (atexit profraw flush included) instead of dying mid-statement
    // at SIGKILL. The Job controller reschedules the pod; the re-run
    // is idempotent under the lock.
    let shutdown = rio_common::signal::shutdown_signal();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    let pool = loop {
        match PgPoolOptions::new().max_connections(2).connect(&url).await {
            Ok(pool) => break pool,
            Err(e)
                if rio_common::pg_error::is_transient_bounded(&e)
                    && std::time::Instant::now() < deadline =>
            {
                // Shared classifier, BOUNDED variant: reachability
                // errors, PG lifecycle FATALs (57P03 during bitnami
                // initdb burned a backoffLimit pod with the old
                // Io|PoolTimedOut-only filter), resource pressure,
                // and 3D000 (database not created yet — bitnami init
                // window). Permanent errors (auth, bad SQL) still
                // fail fast so the Job log shows them; a bad
                // sslrootcert path now burns the deadline visibly
                // (warn every 5s) instead of failing fast — accepted
                // for the Aurora-resume RST case, see pg_error docs.
                tracing::warn!(error = %e, "PostgreSQL not ready; retrying in 5s");
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => {
                        anyhow::bail!("shutdown signal during PostgreSQL connect poll");
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                }
            }
            Err(e) => return Err(e).context("PostgreSQL connect failed"),
        }
    };

    // Migrations + the rio_app role/grant reconciliation, both under
    // one advisory-lock hold (see migrate::run_with_roles for why the
    // role pass must not run unserialized).
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => {
            anyhow::bail!(
                "migration interrupted by shutdown signal; the advisory lock was \
                 released (lock connection closed) and the rescheduled Job re-runs \
                 idempotently"
            );
        }
        r = rio_migrations::migrate::run_with_roles(&pool, rio_migrations::migrator()) => {
            r.inspect_err(|e| error!(error = format!("{e:#}"), "database migrations failed"))?;
        }
    }
    info!("database migrations applied");
    Ok(())
}

/// Connect to PostgreSQL and verify the schema is current. URL is
/// logged with password redacted.
///
// r[impl store.db.pool-idle-timeout+2]
/// Aurora Serverless v2's `max_connections` is RUNTIME-CONSTANT —
/// derived from the configured MAXIMUM capacity (the AWS PG table in
/// infra/eks/rds.tf) and capped at 2,000 when `min_capacity` ≤ 0.5 —
/// it does not scale with the live ACU. The sqlx default 10-minute
/// idle reap means a burst-grown pool holds `max_connections` long
/// after the burst, so N replicas at their pool maxima can exhaust
/// the fixed budget and ad-hoc psql gets `FATAL: remaining connection
/// slots are reserved`. Setting `idle_timeout=60s` +
/// `min_connections=2` shrinks the pool back to baseline within a
/// minute of burst end (I-171). The fleet-level budget is enforced at
/// deploy time: the pg preflight (this binary's `pg-preflight` mode)
/// measures the parameter and derives the store ceiling.
///
/// IAM-mode watch item: RDS caps NEW IAM-authenticated connections at
/// ~200/s cluster-wide. With idle_timeout=60s the pool sheds and
/// regrows around bursts; at current scale (ComponentScaler max 14
/// store replicas x 20 conns) full-fleet regrowth stays well under
/// the ceiling, so no tuning now — the tripwire is
/// rio_pg_iam_mint_failures_total plus connect-error logs. Revisit
/// (iam-mode min_connections/idle_timeout, jittered regrowth) when
/// componentScaler.store.max or pgMaxConnections rises; if the
/// tripwire fires, the escalation is to front Aurora with RDS Proxy —
/// the app side keeps IAM auth while the proxy holds the warm
/// connection pool, making the per-connection ceiling irrelevant.
async fn init_db_pool(
    database_url: &str,
    pg_auth: rio_common::config::PgAuthMode,
    max_connections: u32,
) -> anyhow::Result<sqlx::PgPool> {
    info!(
        url = %rio_common::config::redact_db_url(database_url),
        ?pg_auth,
        max_connections,
        "connecting to PostgreSQL"
    );
    // TokenSource::new is the config preflight: bad URL / weak TLS /
    // missing rootcert / unresolvable AWS region all fail HERE (and
    // crash-loop the pod visibly) before any retry machinery runs.
    let tokens =
        std::sync::Arc::new(rio_common::pg_iam::TokenSource::new(database_url, pg_auth).await?);
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .min_connections(2)
        .idle_timeout(std::time::Duration::from_secs(60))
        // bug_114 defense-in-depth: a server-side statement ceiling so
        // runaway/abandoned queries from dead client connections cannot
        // pile up server-side (the CLIENT-side hold bound is the typed
        // NAR-hold envelope — statement_timeout cannot defend the
        // client against a black-holed network, only the server
        // against zombie statements). Priced off the statement census:
        // claim/heartbeat/complete are ms-class; log sweeps s-class;
        // the long pole is the GC mark CTE / batched sweeps over a
        // ~155K-path store (minutes-class, observed in the live_054
        // forensics era). 600s sits above every lawful statement with
        // headroom and below the kernel TCP-keepalive eternity it
        // replaces.
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                use sqlx::Executor as _;
                conn.execute("SET statement_timeout = '600s'").await?;
                Ok(())
            })
        })
        .connect_with(tokens.fresh_options().await?)
        .await?;
    info!("PostgreSQL connection established");
    tokens.spawn_refresher(pool.clone());

    // r[impl store.db.schema-current+2] — startup does NOT migrate;
    // `rio-store migrate` (helm rio-migrate Job / NixOS oneshot)
    // already did.
    rio_migrations::migrate::assert_current(&pool)
        .await
        .inspect_err(|e| error!(error = format!("{e:#}"), "database schema check failed"))?;
    info!("database schema is current");

    Ok(pool)
}
