//! rio-scheduler binary entry point.
//!
//! Starts the gRPC server, connects to PostgreSQL, and spawns the DAG actor.

use std::sync::Arc;

use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::info;

use rio_proto::AdminServiceServer;
use rio_proto::ExecutorServiceServer;
use rio_proto::SchedulerServiceServer;
use rio_scheduler::actor::ActorHandle;
use rio_scheduler::admin::AdminServiceImpl;
use rio_scheduler::db::SchedulerDb;
use rio_scheduler::grpc::SchedulerGrpc;

// Two-struct config split — see rio-common/src/config.rs for rationale.

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    listen_addr: String,
    store_addr: String,
    database_url: String,
    metrics_addr: std::net::SocketAddr,
    tick_interval_secs: u64,
    /// S3 bucket for build-log flush. `None` = flush disabled.
    /// Env: `RIO_LOG_S3_BUCKET`. Wired into LogFlusher in main().
    log_s3_bucket: Option<String>,
    log_s3_prefix: String,
    /// Size-class cutoff config. Empty = disabled (all workers get all
    /// builds). Workers declare their class in heartbeat; scheduler
    /// routes by estimated duration. TOML array-of-tables:
    ///   [[size_classes]]
    ///   name = "small"
    ///   cutoff_secs = 30.0
    ///   mem_limit_bytes = 1073741824
    /// No CLI override — this is structural deploy config, not a knob
    /// you tweak per-invocation. Change it in scheduler.toml.
    size_classes: Vec<rio_scheduler::SizeClassConfig>,
    /// Fetcher size-class config (I-170). Empty = single-pool mode
    /// (no class filter on FOD dispatch). Just `{name}` per entry,
    /// ordered smallest→largest — the scheduler needs the ORDER to
    /// compute "next larger" for reactive promotion; per-class
    /// resources live on the controller side (`FetcherPool.spec.
    /// classes[]`). MUST match `fetcherPoolDefaults.classes[].name`
    /// in the helm chart (single source of truth: scheduler.yaml
    /// renders both from `.Values.fetcherPoolDefaults.classes`;
    /// class names are arch-agnostic). TOML:
    ///   [[fetcher_size_classes]]
    ///   name = "tiny"
    ///   [[fetcher_size_classes]]
    ///   name = "small"
    fetcher_size_classes: Vec<rio_scheduler::FetcherSizeClassConfig>,
    /// Plaintext health listen address for K8s probes when mTLS is on.
    /// Shares the same HealthReporter as the main server → leadership
    /// toggles propagate. Only listens if server TLS is configured.
    health_addr: std::net::SocketAddr,
    /// mTLS for BOTH server (workers, gateway, controller incoming)
    /// AND client (store outgoing). Set via `RIO_TLS__*`.
    tls: rio_common::tls::TlsConfig,
    /// HMAC key file for signing assignment tokens. The store
    /// verifies on PutPath with the SAME key. Unset = unsigned
    /// tokens (dev mode). Generate: `openssl rand -out /path 32`.
    hmac_key_path: Option<std::path::PathBuf>,
    /// JWT verification. `key_path` → ConfigMap mount at
    /// `/etc/rio/jwt/ed25519_pubkey` (see helm jwt-pubkey-configmap.yaml).
    /// The gateway signs with the matching seed; scheduler verifies.
    /// Unset = interceptor inert (dev mode / pre-key-rotation-infra).
    /// SIGHUP reloads from the same path — kubelet remounts the
    /// ConfigMap on rotation, operator SIGHUPs the pod. Set via
    /// `RIO_JWT__KEY_PATH` (nested figment key — double underscore).
    jwt: rio_common::config::JwtConfig,
    /// Seconds to wait after SIGTERM between set_not_serving()
    /// and serve_with_shutdown returning. Gives the BalancedChannel
    /// probe loop (3s interval) time to observe NOT_SERVING and
    /// reroute. 0 = no drain (tests). Default 6.
    drain_grace_secs: u64,
    /// Kubernetes Lease name for leader election. `None` = non-K8s
    /// mode (single-scheduler; is_leader=true immediately, generation
    /// stays 1). Env: `RIO_LEASE_NAME`. See rio_scheduler::lease.
    lease_name: Option<String>,
    /// Kubernetes namespace for the Lease. `None` = read from the
    /// in-cluster serviceaccount mount, fall back to "default".
    /// Env: `RIO_LEASE_NAMESPACE`. Ignored when `lease_name` is None.
    lease_namespace: Option<String>,
    /// Poison-detection thresholds. `[poison]` table in scheduler.toml.
    /// `r[sched.retry.per-worker-budget]` (scheduler.md:110) specifies
    /// both this and `retry` below as TOML-configurable. P0219 shipped
    /// the structs + builders; this wires them. Default: 3 distinct
    /// workers must fail (matches the former `POISON_THRESHOLD` const).
    /// No CLI override — infrequently-tweaked deploy config.
    poison: rio_scheduler::PoisonConfig,
    /// Per-worker retry backoff curve. `[retry]` table in scheduler.toml.
    /// Default: 2 retries, 5s→300s exponential with 20% jitter. No CLI
    /// override for the same reason as `poison`.
    retry: rio_scheduler::RetryPolicy,
    /// Max concurrent substitute eager-fetch calls at merge time
    /// (r[sched.merge.substitute-fetch]). Bounds the QueryPathInfo
    /// fan-out so the store's S3 connection pool doesn't saturate.
    /// Env: `RIO_SUBSTITUTE_MAX_CONCURRENT`. Default 16.
    #[serde(default = "default_substitute_concurrency")]
    substitute_max_concurrent: usize,
    /// ADR-020 capacity manifest headroom. EMA × this multiplier
    /// before bucketing. Start scheduler-global; per-pool later if
    /// needed. Env: `RIO_HEADROOM_MULTIPLIER`. Default 1.25.
    /// Validated finite + positive at startup.
    #[serde(default = "default_headroom_multiplier")]
    headroom_multiplier: f64,
}

fn default_substitute_concurrency() -> usize {
    rio_scheduler::DEFAULT_SUBSTITUTE_CONCURRENCY
}

fn default_headroom_multiplier() -> f64 {
    rio_scheduler::DEFAULT_HEADROOM_MULTIPLIER
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: rio_common::default_listen_string(9001),
            store_addr: String::new(),
            database_url: String::new(),
            metrics_addr: rio_common::default_addr(9091),
            tick_interval_secs: 10,
            log_s3_bucket: None,
            log_s3_prefix: "logs".into(),
            size_classes: Vec::new(),
            fetcher_size_classes: Vec::new(),
            // 9101 = gRPC (9001) + 100. Same +100 pattern as
            // gateway. Only used when server TLS is configured.
            health_addr: rio_common::default_addr(9101),
            tls: rio_common::tls::TlsConfig::default(),
            hmac_key_path: None,
            jwt: rio_common::config::JwtConfig::default(),
            // periodSeconds: 5 (helm) + 1s propagation. Uniform across
            // all three binaries even though scheduler's actual client
            // probe is 3s — 6s out of 30s termGrace is cheap.
            drain_grace_secs: 6,
            lease_name: None,
            lease_namespace: None,
            poison: rio_scheduler::PoisonConfig::default(),
            retry: rio_scheduler::RetryPolicy::default(),
            substitute_max_concurrent: default_substitute_concurrency(),
            headroom_multiplier: default_headroom_multiplier(),
        }
    }
}

#[derive(Parser, Serialize, Default)]
#[command(
    name = "rio-scheduler",
    about = "DAG-aware build scheduler for rio-build"
)]
struct CliArgs {
    /// gRPC listen address for SchedulerService + ExecutorService
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    listen_addr: Option<String>,

    /// rio-store gRPC address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    store_addr: Option<String>,

    /// PostgreSQL connection URL
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    database_url: Option<String>,

    /// Prometheus metrics listen address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics_addr: Option<std::net::SocketAddr>,

    /// Tick interval for housekeeping (seconds)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    tick_interval_secs: Option<u64>,

    /// S3 bucket for build-log gzip flush (unset = flush disabled)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    log_s3_bucket: Option<String>,

    /// S3 key prefix for build logs
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    log_s3_prefix: Option<String>,

    /// Drain grace period in seconds (0 = disabled)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    drain_grace_secs: Option<u64>,
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
    /// test in the `cfg(test)` mod below). See the scrutiny recipe on
    /// [`rio_common::config::ValidateConfig`].
    fn validate(&self) -> anyhow::Result<()> {
        let cfg = self;
        use rio_common::config::ensure_required as required;
        required(&cfg.store_addr, "store_addr", "scheduler")?;
        required(&cfg.database_url, "database_url", "scheduler")?;
        // `tokio::time::interval(ZERO)` panics. The tick loop feeds
        // `from_secs(cfg.tick_interval_secs)` straight in — `tick_interval_
        // secs = 0` would crash the scheduler on spawn, AFTER migrations
        // ran and the gRPC port was already bound (a very late, very
        // confusing failure). Fail fast at config load.
        anyhow::ensure!(
            cfg.tick_interval_secs > 0,
            "tick_interval_secs must be positive (tokio::time::interval panics on ZERO)"
        );
        // r[impl sched.retry.per-worker-budget]
        // `RetryPolicy::backoff_duration` (worker.rs) computes
        // `random_range(-jf..=jf)` — rand panics if low > high, so jf < 0
        // crashes on the first retry. And jf > 1 makes `clamped * (1 - jf)`
        // negative, which the Duration clamp at worker.rs:248 silently turns
        // into ZERO backoff (retries become thrashing, not backoff). [0.0,
        // 1.0] inclusive — jf=0 means deterministic (no jitter), jf=1 means
        // backoff ∈ [0, 2*clamped] (wide but sane).
        anyhow::ensure!(
            (0.0..=1.0).contains(&cfg.retry.jitter_fraction),
            "retry.jitter_fraction must be in [0.0, 1.0], got {} \
         (negative panics rand::random_range; >1 silently zeros backoff)",
            cfg.retry.jitter_fraction
        );
        // `RetryPolicy::backoff_duration` (worker.rs:229) computes
        // `base_secs * multiplier.powi(attempt)` then `.max(0.0)` at :248.
        // Negative base_secs → negative product → silently zero backoff
        // (retries thrash). NaN/inf → .max(0.0) swallows but the INTENT
        // was a real backoff. Require finite + positive — base_secs=0
        // is also nonsense (zero backoff by design defeats the policy).
        anyhow::ensure!(
            cfg.retry.backoff_base_secs.is_finite() && cfg.retry.backoff_base_secs > 0.0,
            "retry.backoff_base_secs must be finite and positive, got {} \
         (negative/NaN silently zero backoff via worker.rs:248 clamp)",
            cfg.retry.backoff_base_secs
        );
        // `multiplier.powi(attempt)` at worker.rs:229 — attempt grows, so
        // multiplier < 1.0 means backoff SHRINKS with retries (attempt=2
        // waits LESS than attempt=1). multiplier == 1.0 is valid (constant
        // backoff). NaN.powi() = NaN → zero via clamp. Require finite + ≥1.0.
        anyhow::ensure!(
            cfg.retry.backoff_multiplier.is_finite() && cfg.retry.backoff_multiplier >= 1.0,
            "retry.backoff_multiplier must be finite and >= 1.0, got {} \
         (<1.0 makes backoff SHRINK with retries; NaN silently zeros)",
            cfg.retry.backoff_multiplier
        );
        // `base.min(max_secs)` at worker.rs:230 — negative max_secs caps
        // everything negative → zero via clamp. NaN.min(x) = NaN → zero.
        // Infinity is HANDLED (worker.rs test_retry_backoff_infinity_clamped
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
        // `bucketed_estimate` (estimator.rs) computes `(ema × mult).ceil()
        // as u64`. mult ≤ 0 or NaN → saturating cast yields 0 → `.div_ceil
        // (...).max(1)` floors every estimate to minimum bucket (4GiB,
        // 2 cores). Controller would under-provision EVERY build. inf
        // → u64::MAX → `div_ceil × bucket` overflows u64 (panic in
        // debug, wrap in release). Require finite + positive.
        anyhow::ensure!(
            cfg.headroom_multiplier.is_finite() && cfg.headroom_multiplier > 0.0,
            "headroom_multiplier must be finite and positive, got {} \
         (≤0/NaN silently floors all estimates to minimum bucket; \
         inf overflows u64 in div_ceil×bucket)",
            cfg.headroom_multiplier
        );
        for class in &cfg.size_classes {
            // cutoff_secs: TOML supports `nan`/`inf` literals. A typo like
            // `cutoff_secs = nan` would crash the scheduler on every dispatch
            // (the pre-total_cmp sort panicked on NaN). Moved here from the
            // inline main() loop so it fires BEFORE PG connect/migrations/
            // S3-flusher spawn, and is unit-testable alongside config_rejects_*.
            anyhow::ensure!(
                class.cutoff_secs.is_finite() && class.cutoff_secs > 0.0,
                "size_classes[{}].cutoff_secs must be finite and positive, got {}",
                class.name,
                class.cutoff_secs
            );
            // r[impl sched.classify.cpu-bump]
            // cpu_limit_cores is Option<f64> — None means no CPU check. Some(NaN)
            // or Some(neg) would silently disable or always-bump respectively
            // (assignment.rs:128 `c > limit` — NaN→always-false, neg→always-true).
            // Same bounds-check shape as cutoff_secs / P0415's backoff_*.
            // Missed by the P0415 wave (bughunt-mc238, P0424).
            if let Some(limit) = class.cpu_limit_cores {
                anyhow::ensure!(
                    limit.is_finite() && limit > 0.0,
                    "size_classes[{}].cpu_limit_cores must be finite and positive when set, got {}",
                    class.name,
                    limit
                );
            }
        }
        Ok(())
    }
}

impl rio_common::server::HasCommonConfig for Config {
    fn tls(&self) -> &rio_common::tls::TlsConfig {
        &self.tls
    }
    fn metrics_addr(&self) -> std::net::SocketAddr {
        self.metrics_addr
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = CliArgs::parse();
    let rio_common::server::Bootstrap::<Config> {
        cfg,
        shutdown,
        otel_guard: _otel_guard,
    } = rio_common::server::bootstrap(
        "scheduler",
        cli,
        rio_proto::client::init_client_tls,
        rio_scheduler::describe_metrics,
    )?;

    let _root_guard = tracing::info_span!("scheduler", component = "scheduler").entered();
    info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting rio-scheduler"
    );

    // Shutdown chain for the actor: token cancels → actor's select!
    // loop sees it → drops all worker stream_tx → build-exec-bridge
    // tasks exit → ReceiverStream closes → serve_with_shutdown
    // returns → SchedulerGrpc + AdminService drop their ActorHandle
    // clones → tick-loop + lease-loop also break and drop theirs →
    // all mpsc::Sender clones drop → actor's rx.recv() returns None
    // → actor exits → drops event_persist_tx → event-persister also
    // exits (channel-close). event_log::spawn doesn't need a token.

    let (pool, db) = init_db_pool(&cfg.database_url, &shutdown).await?;
    let store_client = connect_store_lazy(&cfg.store_addr);

    // Shared log ring buffers. Written by the BuildExecution recv task
    // (inside SchedulerGrpc), drained by the flusher, read by AdminService.
    let log_buffers = std::sync::Arc::new(rio_scheduler::logs::LogBuffers::new());
    let (log_flush_tx, admin_s3) = init_log_pipeline(
        cfg.log_s3_bucket.as_deref(),
        &cfg.log_s3_prefix,
        pool.clone(),
        Arc::clone(&log_buffers),
    )
    .await;

    // Emit cutoff gauges at startup with the CONFIG values. The
    // rebalancer (apply_pass in rebalancer.rs) re-emits hourly after
    // each pass writes new cutoffs through the RwLock. Operators
    // correlate with class_queue_depth: "small=30s cutoff and 100
    // queued there → scale small pool."
    // Empty config → no gauges emitted (size-classes disabled).
    for class in &cfg.size_classes {
        // Bounds-check happens in validate_config() (fires before PG
        // connect/migrations). This loop is gauge-emit-only.
        metrics::gauge!("rio_scheduler_cutoff_seconds", "class" => class.name.clone())
            .set(class.cutoff_secs);
    }
    if !cfg.size_classes.is_empty() {
        info!(
            classes = ?cfg.size_classes.iter().map(|c| &c.name).collect::<Vec<_>>(),
            "size-class routing enabled"
        );
    }
    if !cfg.fetcher_size_classes.is_empty() {
        info!(
            classes = ?cfg.fetcher_size_classes.iter().map(|c| &c.name).collect::<Vec<_>>(),
            "fetcher size-class routing enabled (reactive)"
        );
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
    // instance. spawn_with_leader injects it into the actor,
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
    // Clone for the health toggle loop + lease loop BEFORE
    // moving into spawn. Both need the same shared Arcs the actor
    // gets; spawn_with_leader consumes the LeaderState.
    let is_leader_for_health = Arc::clone(&leader.is_leader);
    let is_leader_for_grpc = Arc::clone(&leader.is_leader);
    let recovery_complete_for_lease = Arc::clone(&leader.recovery_complete);

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
    let hmac_signer = rio_common::hmac::HmacSigner::load(cfg.hmac_key_path.as_deref())
        .map_err(|e| anyhow::anyhow!("HMAC key load: {e}"))?;
    if hmac_signer.is_some() {
        info!("HMAC assignment token signing enabled");
    }

    // Spawn the DAG actor — now with the shared leader state.
    //
    // Poison + retry: P0219 shipped PoisonConfig/RetryPolicy and
    // the builders; spawn_with_leader chains .with_poison_config
    // + .with_retry_policy internally. We pass the `[poison]` and
    // `[retry]` tables loaded from scheduler.toml (or their
    // defaults via `#[serde(default)]` if absent). Grouped with
    // size_classes: all three are structural deploy config, no
    // CLI override.
    let actor = ActorHandle::spawn_with_leader(
        db,
        store_client,
        log_flush_tx,
        cfg.size_classes,
        cfg.fetcher_size_classes,
        cfg.poison,
        cfg.retry,
        cfg.substitute_max_concurrent,
        cfg.headroom_multiplier,
        Some(leader),
        Some(event_persist_tx),
        hmac_signer,
        shutdown.clone(),
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
        // Reconstruct LeaderState from the SAME Arcs. We moved
        // the original into spawn_with_leader; clone the
        // underlying atomics back out. (They're Arc<Atomic*>;
        // clone is cheap and shares the instance.)
        let lease_state = rio_scheduler::lease::LeaderState {
            generation,
            is_leader: Arc::clone(&is_leader_for_health),
            recovery_complete: recovery_complete_for_lease,
        };
        // Pass actor.clone() for fire-and-forget LeaderAcquired.
        // The lease loop does NOT block on recovery — it keeps
        // renewing while the actor handles LeaderAcquired.
        rio_common::task::spawn_monitored(
            "lease-loop",
            rio_scheduler::lease::run_lease_loop(
                lease_cfg,
                lease_state,
                actor.clone(),
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
    let serve_shutdown = rio_common::signal::Token::new();
    {
        let reporter = health_reporter.clone();
        rio_common::server::spawn_drain_task(
            shutdown.clone(),
            serve_shutdown.clone(),
            std::time::Duration::from_secs(cfg.drain_grace_secs),
            move || async move {
                reporter
                    .set_not_serving::<SchedulerServiceServer<SchedulerGrpc>>()
                    .await;
            },
        );
    }

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
        pool.clone(),
        Arc::clone(&is_leader_for_grpc),
    );

    // Background refresh for ClusterStatus.store_size_bytes — 60s PG poll
    // on the shared DB. Keeps ClusterStatus fast (autoscaler's 30s path).
    let store_size_bytes =
        rio_scheduler::admin::spawn_store_size_refresh(pool.clone(), shutdown.clone());

    // build_samples retention: delete rows older than 30 days, hourly.
    // 30d > rebalancer's 7d query window (P0229) with margin.
    //
    // Fresh SchedulerDb from pool.clone() — `db` was moved into the
    // actor at ActorHandle::spawn_with_leader above. PgPool is
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
        cfg.store_addr.clone(),
        store_size_bytes,
        is_leader_for_grpc,
        shutdown.clone(),
    );

    // Start periodic tick task. Actor-dead handling: try_send fails
    // silently once the channel closes; the shutdown token (cancelled
    // by the actor's drop path) stops the loop shortly after. No
    // early-break needed — spawn_periodic's biased; shutdown wins.
    let tick_actor = actor.clone();
    let tick_interval = std::time::Duration::from_secs(cfg.tick_interval_secs);
    rio_common::task::spawn_periodic("tick-loop", tick_interval, shutdown.clone(), move || {
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
    });

    // Start gRPC server
    let listen_addr: std::net::SocketAddr = cfg.listen_addr.parse()?;
    let max_message_size = rio_proto::max_message_size();

    // Server TLS: if configured, the main port requires client certs.
    // K8s gRPC probes can't do mTLS — so we spawn a SECOND server on
    // `health_addr` with ONLY the health service, plaintext, SHARING
    // the same HealthReporter. The health-toggle loop above writes
    // to that reporter → both servers see the status change → probe
    // on the plaintext port correctly reflects leadership.
    //
    // If the plaintext port had a FRESH reporter, it would never be
    // set to NOT_SERVING (no toggle loop for it) → standby always
    // appears Ready → K8s routes to a non-leader → cluster split.
    // Shared reporter is load-bearing.
    let server_tls = rio_common::tls::load_server_tls(&cfg.tls)?;

    if server_tls.is_some() {
        // r[impl sched.health.shared-reporter]
        // HealthServer<HealthService> clone shares the underlying
        // Arc<RwLock<HashMap>> status map — the toggle loop writes
        // once, both ports see it. See rio_common::server docs.
        rio_common::server::spawn_health_plaintext(
            health_service.clone(),
            cfg.health_addr,
            serve_shutdown.clone(),
        );
        info!("server mTLS enabled — clients must present CA-signed certs");
    }

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
    let jwt_pubkey = rio_common::jwt_interceptor::load_and_wire_jwt(
        cfg.jwt.key_path.as_deref(),
        shutdown.clone(),
    )?;

    info!(
        listen_addr = %listen_addr,
        store_addr = %cfg.store_addr,
        max_message_size,
        log_s3_bucket = ?cfg.log_s3_bucket,
        tls = server_tls.is_some(),
        jwt = jwt_pubkey.is_some(),
        "starting gRPC server"
    );

    let mut builder = tonic::transport::Server::builder();
    if let Some(tls) = server_tls {
        builder = builder.tls_config(tls)?;
    }
    builder
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
            rio_common::jwt_interceptor::jwt_interceptor(jwt_pubkey),
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
    // quick (one K8s PATCH). Ignore the JoinError: it's only set
    // if the lease task panicked, which spawn_monitored already
    // logged, and we're shutting down regardless.
    if let Some(h) = lease_loop {
        let _ = h.await;
    }

    info!("scheduler shut down cleanly");
    Ok(())
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
        Err(RetryError::Exhausted { last, tries }) => {
            anyhow::bail!("PostgreSQL connect failed after {tries} tries: {last}")
        }
    };
    info!("connected to PostgreSQL");

    sqlx::migrate!("../migrations").run(&pool).await?;
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
/// CLIENT_TLS config alongside the other `connect_*` helpers.
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
    prefix: &str,
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
    let aws_cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .load()
        .await;
    let s3 = aws_sdk_s3::Client::new(&aws_cfg);
    let (flush_tx, flush_rx) = tokio::sync::mpsc::channel(1000);
    let flusher = rio_scheduler::logs::LogFlusher::new(
        s3.clone(),
        bucket.to_owned(),
        prefix.to_owned(),
        pool,
        log_buffers,
    );
    flusher.spawn(flush_rx);
    info!(%bucket, %prefix, "log flusher spawned");
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

#[cfg(test)]
mod tests {
    use super::*;
    use rio_common::config::ValidateConfig as _;

    #[test]
    fn config_defaults_are_stable() {
        let d = Config::default();
        assert_eq!(d.listen_addr, "[::]:9001");
        assert_eq!(d.metrics_addr.to_string(), "[::]:9091");
        assert_eq!(d.tick_interval_secs, 10);
        // Phase2a required these; no default.
        assert!(d.store_addr.is_empty());
        assert!(d.database_url.is_empty());
        // Phase2b additions — off by default.
        assert_eq!(d.log_s3_bucket, None);
        assert_eq!(d.log_s3_prefix, "logs");
        // Size-classes: optional feature, off by default.
        assert!(d.size_classes.is_empty());
        // Phase3b: plaintext health port for K8s probes when mTLS on.
        assert_eq!(d.health_addr.to_string(), "[::]:9101");
        assert_eq!(d.drain_grace_secs, 6);
        // Phase 4a (plan 21E): lease config via figment, not raw env.
        assert_eq!(d.lease_name, None, "non-K8s mode by default");
        assert_eq!(d.lease_namespace, None);
        // Phase3b: TLS off by default (dev mode, VM tests).
        assert!(!d.tls.is_configured());
        // JWT verification off by default (interceptor inert until
        // ConfigMap mount configured via RIO_JWT__KEY_PATH).
        assert!(d.jwt.key_path.is_none());
        assert!(!d.jwt.required);
        // P0307: poison+retry wired from scheduler.toml. Defaults
        // match the former hardcoded values in DagActor::new.
        assert_eq!(d.poison, rio_scheduler::PoisonConfig::default());
        assert_eq!(d.retry, rio_scheduler::RetryPolicy::default());
    }

    // r[verify sched.retry.per-worker-budget]
    /// TOML → Config parse for `[poison]` and `[retry]` tables.
    /// Field names match PoisonConfig (`threshold`,
    /// `require_distinct_workers`) and RetryPolicy (`max_retries`,
    /// `backoff_base_secs`, …). The spec at scheduler.md:110 promised
    /// these knobs were TOML-configurable; P0219 shipped the structs
    /// but left the Config side unwired. This proves the parse works.
    ///
    /// Raw figment (not `rio_common::config::load`) to test JUST
    /// the deserialize path — no env/CLI layering concern here.
    /// The Jail tests below exercise the full load() stack.
    #[test]
    fn poison_and_retry_load_from_toml() {
        use figment::providers::{Format, Toml};
        let toml = r#"
            [poison]
            threshold = 5
            require_distinct_workers = false

            [retry]
            max_retries = 4
            backoff_base_secs = 2.5
            backoff_multiplier = 3.0
            backoff_max_secs = 600.0
            jitter_fraction = 0.1
        "#;
        let cfg: Config =
            figment::Figment::from(figment::providers::Serialized::defaults(Config::default()))
                .merge(Toml::string(toml))
                .extract()
                .expect("toml parses into Config");

        assert_eq!(cfg.poison.threshold, 5);
        assert!(!cfg.poison.require_distinct_workers);
        assert_eq!(cfg.retry.max_retries, 4);
        assert_eq!(cfg.retry.backoff_base_secs, 2.5);
        assert_eq!(cfg.retry.backoff_multiplier, 3.0);
        assert_eq!(cfg.retry.backoff_max_secs, 600.0);
        assert_eq!(cfg.retry.jitter_fraction, 0.1);
    }

    /// Empty TOML → `#[serde(default)]` on Config + sub-struct
    /// defaults → identical to `Config::default()`. This is the
    /// "operator didn't configure it" case — existing deployments
    /// with no `[poison]`/`[retry]` tables continue unchanged.
    #[test]
    fn poison_and_retry_default_when_absent() {
        use figment::providers::{Format, Toml};
        let cfg: Config =
            figment::Figment::from(figment::providers::Serialized::defaults(Config::default()))
                .merge(Toml::string(""))
                .extract()
                .expect("empty toml parses");
        assert_eq!(cfg.poison, rio_scheduler::PoisonConfig::default());
        assert_eq!(cfg.retry, rio_scheduler::RetryPolicy::default());
        // Partial table: one field set, others default from the
        // struct-level `#[serde(default)]` on PoisonConfig.
        let partial: Config =
            figment::Figment::from(figment::providers::Serialized::defaults(Config::default()))
                .merge(Toml::string("[poison]\nthreshold = 7"))
                .extract()
                .expect("partial poison table parses");
        assert_eq!(partial.poison.threshold, 7);
        assert!(
            partial.poison.require_distinct_workers,
            "partial table must leave unspecified fields at default"
        );
    }

    #[test]
    fn cli_args_parse_help() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert();
    }

    // -----------------------------------------------------------------------
    // validate_config rejection tests — P0409.
    //
    // P0307 wired `cfg.retry` + `cfg.poison` from scheduler.toml, opening
    // those fields to operator input. Before P0307 they were code-only
    // defaults (jitter_fraction=0.2, threshold=3) — unreachable from
    // config. After P0307, an operator CAN set nonsense values that panic
    // (jitter < 0 → random_range low>high) or silently wrong (threshold=0
    // → every derivation poisons instantly). The validate_config() ensures
    // catch these at startup; these tests prove each ensure fires.
    // -----------------------------------------------------------------------

    /// `Config::default()` leaves `store_addr` and `database_url` empty,
    /// which `validate_config` rejects BEFORE reaching the bounds checks
    /// we want to test. Fill the required fields with placeholders; the
    /// returned config passes validation as-is (the "happy path" baseline
    /// that each rejection test mutates).
    fn test_valid_config() -> Config {
        Config {
            store_addr: "http://store:9002".into(),
            database_url: "postgres://localhost/test".into(),
            ..Config::default()
        }
    }

    /// Whitespace-only store_addr must be rejected as empty.
    /// Regression guard for `ensure_required`'s trim: pre-helper,
    /// bare `is_empty()` accepted `"   "`, startup failed later with
    /// a cryptic "invalid socket address syntax" buried in connect
    /// logs. The helper's trim catches it at config-load with the
    /// clear "X is required" message instead. If this test ever
    /// passes validation, `ensure_required` has regressed to bare
    /// `is_empty()`.
    #[test]
    fn config_rejects_whitespace_store_addr() {
        let mut cfg = test_valid_config();
        cfg.store_addr = "   ".into();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("store_addr is required"),
            "whitespace-only store_addr must be rejected as empty, got: {err}"
        );
    }

    // r[verify sched.retry.per-worker-budget]
    /// Negative `jitter_fraction` → `random_range(-jf..=jf)` with low >
    /// high → rand panic on the FIRST retry (not at config load — hours
    /// later, inside a worker-retry codepath). validate_config catches
    /// it at startup instead.
    #[test]
    fn config_rejects_negative_jitter() {
        let cfg = Config {
            retry: rio_scheduler::RetryPolicy {
                jitter_fraction: -0.1,
                ..Default::default()
            },
            ..test_valid_config()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("jitter_fraction"), "{err}");
        assert!(
            err.contains("-0.1"),
            "error must echo the bad value for operator diagnosis: {err}"
        );
    }

    /// `jitter_fraction > 1` → `clamped * (1 - 1.5)` = `clamped * -0.5`
    /// → negative → silently clamped to `Duration::ZERO` at
    /// worker.rs:248. Retries become thrashing (zero backoff), not
    /// backoff. No crash, no log — the kind of misconfig that costs a
    /// week to diagnose from "scheduler hammers the store on every
    /// failure". validate_config rejects it.
    #[test]
    fn config_rejects_jitter_above_one() {
        let cfg = Config {
            retry: rio_scheduler::RetryPolicy {
                jitter_fraction: 1.5,
                ..Default::default()
            },
            ..test_valid_config()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("jitter_fraction"), "{err}");
        assert!(err.contains("1.5"), "{err}");
    }

    /// `poison.threshold = 0` → `is_poisoned()`'s `count >= 0` is
    /// vacuously true at DAG-merge time → every derivation poisons
    /// before dispatch. Cluster does nothing, no error, just poisoned
    /// rows everywhere. validate_config rejects it.
    #[test]
    fn config_rejects_zero_poison_threshold() {
        let cfg = Config {
            poison: rio_scheduler::PoisonConfig {
                threshold: 0,
                ..Default::default()
            },
            ..test_valid_config()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("poison.threshold"), "{err}");
        assert!(err.contains("0"), "{err}");
    }

    /// Boundary values: the INCLUSIVE endpoints of each range are
    /// valid. jf=0.0 → deterministic (no jitter, `random_range(0..=0)`
    /// is fine). jf=1.0 → backoff ∈ [0, 2*clamped] (wide but sane,
    /// `random_range(-1..=1)` is fine). threshold=1 → poison on first
    /// failure (aggressive but valid for single-worker dev with
    /// `require_distinct_workers=false`). None of these should fail
    /// — the ensure is ∈ [0.0, 1.0] inclusive and > 0, not strict.
    #[test]
    fn config_accepts_boundary_values() {
        // jitter_fraction at both inclusive endpoints.
        for jf in [0.0, 1.0] {
            let cfg = Config {
                retry: rio_scheduler::RetryPolicy {
                    jitter_fraction: jf,
                    ..Default::default()
                },
                ..test_valid_config()
            };
            cfg.validate()
                .unwrap_or_else(|e| panic!("jf={jf} should be valid, got: {e}"));
        }
        // threshold = 1: minimum valid (poison-on-first-failure).
        let cfg = Config {
            poison: rio_scheduler::PoisonConfig {
                threshold: 1,
                ..Default::default()
            },
            ..test_valid_config()
        };
        cfg.validate().expect("threshold=1 should be valid");
        // And the baseline (all defaults + required-field placeholders)
        // passes — proves test_valid_config() itself is valid, so the
        // rejection tests above are testing ONLY their mutation.
        test_valid_config()
            .validate()
            .expect("default config should be valid");
    }

    // r[verify sched.retry.per-worker-budget]
    /// Negative backoff_base_secs → silently zero backoff via the
    /// `.max(0.0)` at worker.rs:248. Same thrash-mode as jitter>1
    /// but via a different field — both guarded at config-load.
    #[test]
    fn config_rejects_negative_backoff_base() {
        let cfg = Config {
            retry: rio_scheduler::RetryPolicy {
                backoff_base_secs: -5.0,
                ..Default::default()
            },
            ..test_valid_config()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("backoff_base_secs"), "{err}");
        assert!(err.contains("-5"), "{err}");
    }

    #[test]
    fn config_rejects_nan_backoff_base() {
        let cfg = Config {
            retry: rio_scheduler::RetryPolicy {
                backoff_base_secs: f64::NAN,
                ..Default::default()
            },
            ..test_valid_config()
        };
        assert!(cfg.validate().is_err());
    }

    /// Multiplier < 1.0 → shrinking backoff (attempt=2 waits LESS than
    /// attempt=1). The math works — it's just operator-error.
    #[test]
    fn config_rejects_sub_one_multiplier() {
        let cfg = Config {
            retry: rio_scheduler::RetryPolicy {
                backoff_multiplier: 0.5,
                ..Default::default()
            },
            ..test_valid_config()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("backoff_multiplier"), "{err}");
        assert!(err.contains(">= 1.0"), "{err}");
    }

    #[test]
    fn config_rejects_nan_multiplier() {
        let cfg = Config {
            retry: rio_scheduler::RetryPolicy {
                backoff_multiplier: f64::NAN,
                ..Default::default()
            },
            ..test_valid_config()
        };
        assert!(cfg.validate().is_err());
    }

    /// max_secs < base_secs is contradictory (the "max" is below the
    /// "base" — every backoff clamps to max, which defeats the
    /// exponential). Catch at config-load with a clear message citing
    /// both values.
    #[test]
    fn config_rejects_max_below_base() {
        let cfg = Config {
            retry: rio_scheduler::RetryPolicy {
                backoff_base_secs: 10.0,
                backoff_max_secs: 5.0,
                ..Default::default()
            },
            ..test_valid_config()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("backoff_max_secs"), "{err}");
        assert!(err.contains(">= backoff_base_secs"), "{err}");
    }

    /// Boundary: multiplier=1.0 (constant backoff), base=max (no growth
    /// room — every attempt waits base_secs). Both valid edge cases.
    #[test]
    fn config_accepts_backoff_boundaries() {
        // multiplier=1.0 → constant backoff, valid.
        let cfg = Config {
            retry: rio_scheduler::RetryPolicy {
                backoff_multiplier: 1.0,
                ..Default::default()
            },
            ..test_valid_config()
        };
        cfg.validate().expect("multiplier=1.0 should be valid");

        // base==max → clamped immediately, no exponential room, valid.
        let cfg = Config {
            retry: rio_scheduler::RetryPolicy {
                backoff_base_secs: 30.0,
                backoff_max_secs: 30.0,
                ..Default::default()
            },
            ..test_valid_config()
        };
        cfg.validate().expect("base==max should be valid");

        // Defaults (5.0, 2.0, 300.0) pass all checks.
        test_valid_config()
            .validate()
            .expect("defaults should be valid");
    }

    /// Helper: single-element size_classes vec with the given cpu_limit_cores.
    /// Fills cutoff_secs and mem_limit_bytes with valid placeholders so the
    /// test exercises ONLY the cpu_limit check.
    fn size_classes_with_cpu_limit(limit: Option<f64>) -> Vec<rio_scheduler::SizeClassConfig> {
        vec![rio_scheduler::SizeClassConfig {
            name: "small".into(),
            cutoff_secs: 30.0,
            mem_limit_bytes: 1 << 30,
            cpu_limit_cores: limit,
        }]
    }

    /// `cpu_limit_cores = Some(NaN)` → `c > NaN` at assignment.rs:128 is
    /// always false → CPU-bump silently disabled, builds never bump even
    /// when CPU-bound. validate_config catches it at startup (bughunt-mc238,
    /// P0424 — missed by the P0415 f64-bounds wave).
    #[test]
    fn config_rejects_nan_cpu_limit_cores() {
        let cfg = Config {
            size_classes: size_classes_with_cpu_limit(Some(f64::NAN)),
            ..test_valid_config()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("cpu_limit_cores") && err.contains("finite"),
            "NaN cpu_limit must be rejected with clear message, got: {err}"
        );
    }

    /// `cpu_limit_cores = Some(-1.0)` → `c > -1.0` at assignment.rs:128 is
    /// always true → every build bumps to next class, misroutes entire
    /// small-class queue.
    #[test]
    fn config_rejects_zero_headroom_multiplier() {
        let cfg = Config {
            headroom_multiplier: 0.0,
            ..test_valid_config()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("headroom_multiplier must be finite and positive"),
            "0.0 headroom floors all estimates to minimum bucket, got: {err}"
        );
    }

    #[test]
    fn config_rejects_nan_headroom_multiplier() {
        let cfg = Config {
            headroom_multiplier: f64::NAN,
            ..test_valid_config()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("headroom_multiplier must be finite and positive"),
            "NaN headroom silently floors all estimates, got: {err}"
        );
    }

    #[test]
    fn config_rejects_negative_cpu_limit_cores() {
        let cfg = Config {
            size_classes: size_classes_with_cpu_limit(Some(-1.0)),
            ..test_valid_config()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("cpu_limit_cores") && err.contains("positive"),
            "negative cpu_limit must be rejected, got: {err}"
        );
    }

    /// `cpu_limit_cores = None` → no CPU check, valid. The Option is what
    /// makes this field optional for existing TOML without the key. Only
    /// Some(bad) is an error.
    #[test]
    fn config_accepts_none_cpu_limit_cores() {
        let cfg = Config {
            size_classes: size_classes_with_cpu_limit(None),
            ..test_valid_config()
        };
        cfg.validate()
            .expect("None cpu_limit_cores = no check, should be valid");
        // Boundary: Some(small positive) is fine.
        let cfg = Config {
            size_classes: size_classes_with_cpu_limit(Some(0.5)),
            ..test_valid_config()
        };
        cfg.validate().expect("positive cpu_limit should be valid");
    }

    // figment::Jail standing-guard tests — see rio-test-support/src/config.rs.
    // When you add Config.newfield: ADD IT to both assert blocks below.

    rio_test_support::jail_roundtrip!(
        "scheduler",
        r#"
        [poison]
        threshold = 7

        [retry]
        backoff_base_secs = 3.33
        "#,
        |cfg: Config| {
            assert_eq!(
                cfg.poison.threshold, 7,
                "[poison] table must thread through figment into PoisonConfig"
            );
            assert_eq!(
                cfg.retry.backoff_base_secs, 3.33,
                "[retry] table must thread through figment into RetryPolicy"
            );
            // Unspecified fields default via #[serde(default)] on
            // the sub-struct — PARTIAL tables must work.
            assert!(
                cfg.poison.require_distinct_workers,
                "unspecified sub-field must fall through to Default"
            );
        }
    );

    rio_test_support::jail_defaults!(
        "scheduler",
        r#"listen_addr = "0.0.0.0:9001""#,
        |cfg: Config| {
            assert_eq!(cfg.poison, rio_scheduler::PoisonConfig::default());
            assert_eq!(cfg.retry, rio_scheduler::RetryPolicy::default());
            assert!(cfg.size_classes.is_empty());
        }
    );

    // -----------------------------------------------------------------------
    // gRPC health service wiring smoke tests.
    //
    // These validate the tonic-health integration pattern used by all
    // three binaries (scheduler/store/gateway). They live HERE (not in
    // each crate) because the pattern is identical and testing it once
    // proves the wiring — the per-crate variation is just WHEN
    // set_serving is called (post-migrations vs post-connect), which
    // main() sequences and the VM tests cover e2e.
    // -----------------------------------------------------------------------

    /// Spin up a tonic server with ONLY the health service on an
    /// ephemeral port, return the address + reporter handle.
    ///
    /// The server task is detached — fine for tests; the process exits
    /// when the test fn returns. No graceful shutdown needed (no
    /// resources to clean up; the listener's socket closes on drop).
    async fn spawn_health_server() -> (std::net::SocketAddr, tonic_health::server::HealthReporter) {
        let (reporter, service) = tonic_health::server::health_reporter();
        // Port 0 → kernel assigns. Read back the bound addr before
        // spawning — serve() consumes the listener so we can't ask later.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(service)
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        (addr, reporter)
    }

    /// Fresh health_reporter() → NOT_SERVING until set_serving is called.
    /// K8s readiness probe failing during boot is correct: the Service
    /// shouldn't route to a half-initialized pod.
    ///
    /// tonic-health's DEFAULT behavior for a service that was never
    /// registered is "Unknown" (gRPC NotFound). The empty-string "" check
    /// (whole server) defaults to SERVING unless explicitly set otherwise.
    /// So we check a NAMED service that hasn't been set — that's the
    /// realistic boot race: K8s probes before main() reaches set_serving.
    #[tokio::test]
    async fn health_not_serving_before_set() -> anyhow::Result<()> {
        use tonic_health::pb::{HealthCheckRequest, health_client::HealthClient};

        let (addr, _reporter) = spawn_health_server().await;
        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
            .connect()
            .await?;
        let mut client = HealthClient::new(channel);

        // Named service, never registered → NotFound (which K8s treats
        // as a probe failure, same as NOT_SERVING for readiness purposes).
        let result = client
            .check(HealthCheckRequest {
                service: "rio.scheduler.SchedulerService".into(),
            })
            .await;
        let status = result.expect_err("unregistered service should be NotFound");
        assert_eq!(
            status.code(),
            tonic::Code::NotFound,
            "probe failure before boot completes — K8s won't route to this pod"
        );
        Ok(())
    }

    #[tokio::test]
    async fn health_serving_after_set() -> anyhow::Result<()> {
        use tonic_health::pb::{
            HealthCheckRequest, health_check_response::ServingStatus, health_client::HealthClient,
        };

        let (addr, reporter) = spawn_health_server().await;

        // The same call main() makes. Type param = the service impl.
        reporter
            .set_serving::<SchedulerServiceServer<SchedulerGrpc>>()
            .await;

        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
            .connect()
            .await?;
        let mut client = HealthClient::new(channel);

        // tonic-health derives the name from S::NAME (NamedService trait,
        // tonic-generated from proto `package rio.scheduler; service
        // SchedulerService`). The test originally guessed ".v1" — there
        // isn't one. This assertion CATCHES proto-package drift: if
        // someone adds versioning to scheduler.proto, this fails and
        // whoever did it updates the K8s probe config to match.
        let resp = client
            .check(HealthCheckRequest {
                service: "rio.scheduler.SchedulerService".into(),
            })
            .await?
            .into_inner();
        assert_eq!(
            ServingStatus::try_from(resp.status)?,
            ServingStatus::Serving,
            "set_serving → SERVING → K8s routes to this pod"
        );

        // The empty-string "whole server" check — K8s probes send this
        // when no service name is configured (the common case; per-
        // service granularity is rarely needed). tonic-health registers
        // under BOTH the named service AND "" on any set_serving call.
        let resp_empty = client
            .check(HealthCheckRequest {
                service: String::new(),
            })
            .await?
            .into_inner();
        assert_eq!(
            ServingStatus::try_from(resp_empty.status)?,
            ServingStatus::Serving,
            "empty-string check also SERVING after any set_serving"
        );
        Ok(())
    }

    // r[verify sched.health.shared-reporter]
    /// The health service is Clone, so we can serve it from TWO
    /// tonic servers (mTLS main port + plaintext health port)
    /// with ONE shared HealthReporter. The toggle loop writes to the
    /// reporter once; BOTH servers see the status change. If we created
    /// a fresh reporter for the plaintext port, it would never toggle
    /// → standby always SERVING → K8s routes to non-leader.
    #[tokio::test]
    async fn health_service_clone_shares_reporter_state() -> anyhow::Result<()> {
        use tonic_health::pb::{
            HealthCheckRequest, health_check_response::ServingStatus, health_client::HealthClient,
        };

        let (reporter, health_service) = tonic_health::server::health_reporter();
        let health_service_clone = health_service.clone();

        // Spawn TWO servers, each on its own port, each with its
        // own clone of the health service.
        let l1 = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr1 = l1.local_addr()?;
        let l2 = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr2 = l2.local_addr()?;

        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(health_service)
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(l1))
                .await
                .unwrap();
        });
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(health_service_clone)
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(l2))
                .await
                .unwrap();
        });

        let ch1 = tonic::transport::Channel::from_shared(format!("http://{addr1}"))?
            .connect()
            .await?;
        let ch2 = tonic::transport::Channel::from_shared(format!("http://{addr2}"))?
            .connect()
            .await?;
        let mut c1 = HealthClient::new(ch1);
        let mut c2 = HealthClient::new(ch2);

        // Set SERVING via the ONE reporter. BOTH servers should see it.
        reporter
            .set_serving::<SchedulerServiceServer<SchedulerGrpc>>()
            .await;
        let req = || HealthCheckRequest {
            service: "rio.scheduler.SchedulerService".into(),
        };
        let s1 = ServingStatus::try_from(c1.check(req()).await?.into_inner().status)?;
        let s2 = ServingStatus::try_from(c2.check(req()).await?.into_inner().status)?;
        assert_eq!(s1, ServingStatus::Serving, "server 1 sees SERVING");
        assert_eq!(
            s2,
            ServingStatus::Serving,
            "server 2 (cloned service) ALSO sees SERVING — shared state"
        );

        // Toggle NOT_SERVING. BOTH should flip — proving the clone
        // shares the underlying status map, not a snapshot.
        reporter
            .set_not_serving::<SchedulerServiceServer<SchedulerGrpc>>()
            .await;
        let s1 = ServingStatus::try_from(c1.check(req()).await?.into_inner().status)?;
        let s2 = ServingStatus::try_from(c2.check(req()).await?.into_inner().status)?;
        assert_eq!(s1, ServingStatus::NotServing);
        assert_eq!(
            s2,
            ServingStatus::NotServing,
            "clone tracks toggles — standby on plaintext port would \
             correctly show NOT_SERVING → K8s excludes it"
        );
        Ok(())
    }

    /// set_not_serving flips back. The health toggle loop uses this
    /// to gate on is_leader: standby replicas stay NOT_SERVING so the
    /// K8s Service routes only to the leader.
    #[tokio::test]
    async fn health_toggle_not_serving() -> anyhow::Result<()> {
        use tonic_health::pb::{
            HealthCheckRequest, health_check_response::ServingStatus, health_client::HealthClient,
        };

        let (addr, reporter) = spawn_health_server().await;
        reporter
            .set_serving::<SchedulerServiceServer<SchedulerGrpc>>()
            .await;
        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
            .connect()
            .await?;
        let mut client = HealthClient::new(channel);

        // SERVING → NOT_SERVING → SERVING. The leader-toggle pattern.
        //
        // IMPORTANT: `set_not_serving::<S>()` only flips the NAMED
        // service, NOT the empty-string "" check. The first
        // `set_serving` registers "" as SERVING and nothing toggles it
        // back. So the K8s readinessProbe MUST be configured with
        // `grpc.service: rio.scheduler.SchedulerService` explicitly.
        // The kustomize manifest needs this. Without it, a standby
        // scheduler would pass readiness on the "" check and K8s would
        // route to a non-leader.
        //
        // async fn not closure: a closure borrowing `&mut client` +
        // returning an async block that uses the borrow doesn't have a
        // stable-Rust spelling (the borrow's lifetime can't outlive the
        // closure call but the async block escapes). `async fn` dodges
        // this entirely.
        async fn check(
            client: &mut HealthClient<tonic::transport::Channel>,
        ) -> Result<ServingStatus, tonic::Status> {
            client
                .check(HealthCheckRequest {
                    // NAMED service, not "" — set_not_serving only
                    // affects this. See above for why this matters.
                    service: "rio.scheduler.SchedulerService".into(),
                })
                .await
                .map(|r| ServingStatus::try_from(r.into_inner().status).unwrap())
        }

        assert_eq!(check(&mut client).await?, ServingStatus::Serving);

        reporter
            .set_not_serving::<SchedulerServiceServer<SchedulerGrpc>>()
            .await;
        assert_eq!(
            check(&mut client).await?,
            ServingStatus::NotServing,
            "set_not_serving → K8s stops routing (standby scheduler)"
        );

        reporter
            .set_serving::<SchedulerServiceServer<SchedulerGrpc>>()
            .await;
        assert_eq!(
            check(&mut client).await?,
            ServingStatus::Serving,
            "re-acquired leadership → resume traffic"
        );
        Ok(())
    }

    /// r[verify common.drain.not-serving-before-exit]
    /// Drain task sequencing: parent token cancel → health flips to
    /// NOT_SERVING → child token cancels AFTER grace. A client checking
    /// health between parent-cancel and child-cancel sees NOT_SERVING.
    /// This is the window K8s kubelet needs to pull us from Endpoints.
    #[tokio::test]
    async fn drain_sets_not_serving_before_child_cancel() -> anyhow::Result<()> {
        use rio_common::signal::Token as CancellationToken;
        use tonic_health::pb::{
            HealthCheckRequest, health_check_response::ServingStatus, health_client::HealthClient,
        };

        let (addr, reporter) = spawn_health_server().await;
        reporter
            .set_serving::<SchedulerServiceServer<SchedulerGrpc>>()
            .await;

        let parent = CancellationToken::new();
        // INDEPENDENT token — NOT parent.child_token(). child_token
        // cascades synchronously: parent.cancel() would set
        // child.is_cancelled()=true instantly, giving zero drain
        // window. This test proves the independent-token pattern
        // actually works (it was the thing that caught the bug in
        // the original child_token-based remediation plan).
        let child = CancellationToken::new();
        // Short grace for test speed — the production default is 6s.
        let grace = std::time::Duration::from_millis(200);

        // Inline the drain task body (can't call main()).
        {
            let reporter = reporter.clone();
            let parent = parent.clone();
            let child = child.clone();
            tokio::spawn(async move {
                parent.cancelled().await;
                reporter
                    .set_not_serving::<SchedulerServiceServer<SchedulerGrpc>>()
                    .await;
                tokio::time::sleep(grace).await;
                child.cancel();
            });
        }

        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
            .connect()
            .await?;
        let mut client = HealthClient::new(channel);
        let req = || HealthCheckRequest {
            service: "rio.scheduler.SchedulerService".into(),
        };

        // Pre-cancel: SERVING, child not cancelled.
        assert_eq!(
            ServingStatus::try_from(client.check(req()).await?.into_inner().status)?,
            ServingStatus::Serving
        );
        assert!(!child.is_cancelled());

        // Fire parent. The drain task is woken; set_not_serving is an
        // async RwLock write — yield to let it run.
        parent.cancel();
        tokio::task::yield_now().await;
        // A few more yields for the broadcast to propagate to the
        // health service's watch channel.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        // CRITICAL ASSERTION: during the grace window, health is
        // NOT_SERVING but child is NOT YET cancelled. This is the
        // window where kubelet probes NOT_SERVING → removes endpoint.
        assert_eq!(
            ServingStatus::try_from(client.check(req()).await?.into_inner().status)?,
            ServingStatus::NotServing,
            "health must flip BEFORE child cancels — this is the drain window"
        );
        assert!(
            !child.is_cancelled(),
            "child must NOT cancel until grace elapses — serve_with_shutdown \
             would return early and we'd exit while kubelet still thinks SERVING"
        );

        // After grace: child cancelled.
        tokio::time::timeout(grace * 3, child.cancelled())
            .await
            .expect("child should cancel within ~grace");

        Ok(())
    }

    /// Gateway's kubelet probe sends empty service name. set_not_serving<S>
    /// does NOT flip "" (proven by health_toggle_not_serving). This test
    /// proves set_service_status("", NotServing) DOES.
    #[tokio::test]
    async fn set_service_status_empty_string_flips_whole_server() -> anyhow::Result<()> {
        use tonic_health::pb::{
            HealthCheckRequest, health_check_response::ServingStatus, health_client::HealthClient,
        };

        let (addr, reporter) = spawn_health_server().await;
        // Register "" as SERVING (side effect of any set_serving call).
        reporter
            .set_serving::<SchedulerServiceServer<SchedulerGrpc>>()
            .await;

        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
            .connect()
            .await?;
        let mut client = HealthClient::new(channel);
        let empty = || HealthCheckRequest {
            service: String::new(),
        };

        assert_eq!(
            ServingStatus::try_from(client.check(empty()).await?.into_inner().status)?,
            ServingStatus::Serving
        );

        // The gateway's drain call.
        reporter
            .set_service_status("", tonic_health::ServingStatus::NotServing)
            .await;

        assert_eq!(
            ServingStatus::try_from(client.check(empty()).await?.into_inner().status)?,
            ServingStatus::NotServing,
            "gateway drain must flip the empty-string check — that's what \
             kubelet probes when helm gateway.yaml has no grpc.service field"
        );
        Ok(())
    }
}
