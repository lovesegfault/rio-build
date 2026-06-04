use std::sync::Arc;

use clap::Parser;
use sqlx::postgres::PgPoolOptions;
use tracing::{error, info};

use rio_proto::ChunkServiceServer;
use rio_proto::StoreAdminServiceServer;
use rio_proto::StoreServiceServer;
use rio_store::backend::ChunkBackend;
use rio_store::grpc::{ChunkServiceImpl, StoreAdminServiceImpl, StoreServiceImpl};
use rio_store::signing::{Signer, TenantSigner};
use rio_store::substitute::Substituter;

use rio_store::config::{
    CliArgs, Config, StoreCommand, derive_substitute_admission_cap, init_chunk_backend,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
    let admission_cap = cfg
        .substitute_admission_permits
        .unwrap_or_else(|| derive_substitute_admission_cap(cfg.pg_max_connections));
    info!(admission_cap, "substitute admission gate");
    let substitute_admission = rio_store::admission::AdmissionGate::new(admission_cap);

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
    if let Some(v) = hmac_verifier {
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
            .with_nar_bytes_budget(store_service.nar_bytes_budget().clone())
            .with_admission_gate(substitute_admission.clone());
        if let Some(signer) = store_service.signer() {
            s = s.with_signer(Arc::clone(signer));
        }
        Arc::new(s)
    };
    let store_service = store_service.with_substituter(substituter);

    // r[impl store.shutdown.drain-getpath]
    // Three-stage shutdown — see rio_common::server::spawn_drain_task
    // for the INDEPENDENT-token rationale + proof test. Closure flips
    // the NAMED StoreService (BalancedChannel probe target). The
    // after-grace hook waits for in-flight GetPath body streams to
    // complete (or `stream_drain` to elapse) BEFORE serve_shutdown
    // tears down the listener — ComponentScaler scale-down assumes
    // SIGTERM drains in-flight work (decide.rs MAX_SCALE_DOWN_STEP).
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
    // ARE no chunks to get).
    let chunk_service = ChunkServiceImpl::new(chunk_cache.clone());

    // StoreAdminServiceImpl: TriggerGC + VerifyChunks + upstream CRUD
    // + GetLoad. Gets the chunk backend directly (for key_for in
    // sweep's pending_s3_deletes enqueue + VerifyChunks HeadObject).
    // None for inline-only stores — sweep does CASCADE delete only.
    //
    // Also spawn GC background tasks (orphan scanner + orphan-chunk
    // sweep + drain). All periodic (15min / 1h / 30s).
    // spawn_monitored: if one panics, logged; store keeps serving
    // (degraded GC, not down).
    let chunk_backend_for_gc: Option<Arc<dyn ChunkBackend>> =
        chunk_cache.as_ref().map(|c| c.backend());
    let admin_service = StoreAdminServiceImpl::new(pool.clone(), chunk_backend_for_gc.clone())
        .with_shutdown(shutdown.clone())
        .with_service_verifier(service_verifier)
        .with_substitute_admission(substitute_admission);
    rio_store::gc::orphan::spawn_scanner(
        pool.clone(),
        chunk_backend_for_gc.clone(),
        shutdown.clone(),
    );
    rio_store::gc::sweep::spawn_orphan_chunk_sweep(
        pool.clone(),
        chunk_backend_for_gc.clone(),
        shutdown.clone(),
    );
    if let Some(backend) = chunk_backend_for_gc {
        rio_store::gc::drain::spawn_drain_task(pool.clone(), backend, shutdown.clone());
    }

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

    info!(
        addr = %addr,
        max_msg_size,
        jwt = jwt_pubkey.is_some(),
        "starting gRPC server"
    );

    rio_common::server::tonic_builder()
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
            rio_auth::jwt_interceptor::jwt_interceptor(jwt_pubkey),
        ))
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
            StoreAdminServiceServer::new(admin_service)
                .max_decoding_message_size(max_msg_size)
                .max_encoding_message_size(max_msg_size),
        )
        .serve_with_shutdown(addr, serve_shutdown.cancelled_owned())
        .await?;

    info!("store shut down cleanly");
    Ok(())
}

// ── bootstrap helpers (extracted from main) ──────────────────────────

/// `rio-store migrate`: one-shot migration runner. This is the
/// container entrypoint of the helm `rio-migrate` Job and the
/// ExecStart of the NixOS `rio-migrate` systemd oneshot — migrations
/// run out-of-band, BEFORE any app pod/service starts, and always as
/// the database master — schema DDL never depends on the credentials
/// or privileges of whatever auth mode the app pods use.
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
                // initdb burned a backoffLimit pod with an
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

    tokio::select! {
        biased;
        _ = shutdown.cancelled() => {
            anyhow::bail!(
                "migration interrupted by shutdown signal; the advisory lock was \
                 released (lock connection closed) and the rescheduled Job re-runs \
                 idempotently"
            );
        }
        r = rio_migrations::migrate::run(&pool, rio_migrations::migrator()) => {
            r.inspect_err(|e| error!(error = %e, "database migrations failed"))?;
        }
    }
    info!("database migrations applied");
    Ok(())
}

/// Connect to PostgreSQL and verify the schema is current. URL is
/// logged with password redacted.
///
// r[impl store.db.pool-idle-timeout]
/// Aurora Serverless v2 scales `max_connections` with ACU; at
/// `min_capacity=0.5` (infra/eks/rds.tf) that's ~105 usable slots. The
/// sqlx default 10-minute idle reap means a burst-grown pool holds
/// `max_connections` long after the burst — so two store replicas at
/// 50 + two scheduler at 10 = 120 idle conns against a 105-slot
/// server, and ad-hoc psql gets `FATAL: remaining connection slots
/// are reserved`. Setting `idle_timeout=60s` + `min_connections=2`
/// shrinks the pool back to baseline within a minute of burst end
/// (I-171).
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
