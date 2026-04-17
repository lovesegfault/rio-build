//! Cold-start wiring: identity, host-arch validation, cgroup init,
//! upstream connect, FUSE mount, relay/heartbeat spawn, build context.
//!
//! Everything `main()` did before the `'reconnect` loop. Produces a
//! [`BuilderRuntime`] consumed by [`run`](super::run).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};

use tokio::sync::{Notify, Semaphore, mpsc, watch};
use tracing::info;

use rio_proto::types::ExecutorMessage;

use super::heartbeat::{HeartbeatCtx, spawn_heartbeat};
use super::prefetch::PrefetchDeps;
use super::slot::BuildSlot;
use super::{BuildSpawnContext, BuilderRuntime, relay_loop};
use crate::config::{Config, detect_system};
use crate::fuse::StoreClients;

pub(super) type WorkerClient = rio_proto::ExecutorServiceClient<tonic::transport::Channel>;

/// Probe-loop guards for both balanced channels. Dropping a
/// `BalancedChannel` stops its probe loop, so these must outlive the
/// clients. Either can be `None` (single-channel fallback).
pub(super) type BalanceGuards = (
    Option<rio_proto::client::balance::BalancedChannel>,
    Option<rio_proto::client::balance::BalancedChannel>,
);

/// Wire up cgroups, health server, gRPC clients, FUSE mount, relay,
/// heartbeat, and the build context. Everything `main()` did before the
/// `'reconnect` loop.
///
/// Returns `None` if shutdown fired during cold-start connect — caller
/// exits cleanly (nothing to drain, never connected).
pub async fn setup(
    mut cfg: Config,
    shutdown: rio_common::signal::Token,
) -> anyhow::Result<Option<BuilderRuntime>> {
    let (executor_id, systems, features) = resolve_executor_identity(
        std::mem::take(&mut cfg.executor_id),
        std::mem::take(&mut cfg.systems),
        std::mem::take(&mut cfg.features),
    )?;
    validate_host_arch(cfg.executor_kind, &systems, &detect_system())?;
    info!(%executor_id, "executor identity resolved");

    // cgroup setup BEFORE the health server: if cgroup fails, we don't
    // want liveness passing while startup is hung on `?` propagation.
    // Pod goes straight to CrashLoopBackOff with a clear log line.
    let (cgroup_parent, resource_snapshot) = init_cgroup(&cfg.overlay_base_dir, shutdown.clone())?;

    // Readiness flag + HTTP health server. Spawned BEFORE gRPC connect
    // so liveness passes as soon as the process is up (connect may take
    // seconds if scheduler DNS is slow to resolve). Readiness stays
    // false until the first heartbeat comes back accepted — that's the
    // right gate: a worker that can't heartbeat is not useful capacity.
    let ready = Arc::new(AtomicBool::new(false));
    crate::health::spawn_health_server(cfg.health_addr, Arc::clone(&ready), shutdown.clone());

    let Some((store_clients, scheduler_client, _balance_guard)) =
        connect_upstreams(&cfg, &shutdown).await
    else {
        // Shutdown fired during cold-start connect. Clean exit —
        // nothing to drain (never connected), no FUSE mounted yet.
        return Ok(None);
    };
    info!(
        %executor_id,
        scheduler_addr = %cfg.scheduler.addr,
        store_addr = %cfg.store.addr,
        systems = ?systems,
        features = ?features,
        "connected to gRPC services"
    );

    // ADR-023 phase-10 hw self-calibration. Best-effort, fire-and-
    // forget: a ~5s CPU-bound microbench in a blocking thread + one
    // store RPC. Runs concurrently with FUSE mount + first heartbeat
    // below; the bench finishes well before the first assignment can
    // arrive (Karpenter cold-start is ~30s). Empty hw_class → skipped
    // inside `report` (controller hasn't stamped the annotation yet,
    // or non-k8s).
    rio_common::task::spawn_monitored("hw-bench", {
        let mut store = store_clients.store.clone();
        let hw_class = cfg.hw_class.clone();
        let pod_id = executor_id.clone();
        async move { crate::hw_bench::report(&mut store, &hw_class, &pod_id).await }
    });

    // Set up FUSE cache and mount. Arc so we can clone for the
    // prefetch handler before moving into mount_fuse_background.
    let cache = Arc::new(crate::fuse::cache::Cache::new(cfg.fuse_cache_dir)?);
    // Clone for prefetch. Cache methods use runtime.block_on
    // internally (sync, designed for FUSE callbacks on dedicated
    // threads). The prefetch handler will call them via
    // spawn_blocking — async → nested-runtime panic.
    let prefetch_cache = Arc::clone(&cache);
    let runtime = tokio::runtime::Handle::current();
    // FUSE fetch timeout (60s default) — NOT GRPC_STREAM_TIMEOUT (300s).
    // FUSE is the build-critical path; a stalled fetch blocks a fuser
    // thread. See config.rs fuse_fetch_timeout for the full rationale.
    let fuse_fetch_timeout = cfg.fuse_fetch_timeout;

    // ─── Startup rootfs writes (readOnlyRootFilesystem audit) ─────
    //
    // Pool kind: Fetcher forces readOnlyRootFilesystem:true (ADR-019
    // §Sandbox hardening — reconcilers/pool/pod.rs).
    // Every write below MUST land on an emptyDir mount from
    // reconcilers/common/sts.rs, or the pod CrashLoops with EROFS.
    //
    //   path                        | covering mount (sts.rs)
    //   ──────────────────────────────────────────────────────────
    //   cfg.fuse_mount_point        | `fuse-store` emptyDir
    //     (/var/rio/fuse-store)     |   (readOnlyRoot only)
    //   cfg.overlay_base_dir        | `overlays` emptyDir
    //     (/var/rio/overlays)       |   (always)
    //   /nix/var/{nix,log}/**       | `nix-var` emptyDir
    //                               |   (readOnlyRoot only)
    //   /tmp (tempfile crate)       | `tmp` emptyDir, 64Mi tmpfs
    //                               |   (readOnlyRoot only)
    //   cfg.fuse_cache_dir          | `fuse-cache` emptyDir
    //     (/var/rio/cache —         |   (always)
    //      Cache::new above)        |
    //   /sys/fs/cgroup/**           | cgroupfs, not rootfs —
    //     (cgroup.rs)               |   remounted rw at cgroup.rs
    //                               |   ns-root-remount
    //
    // Adding a new startup write? Extend BOTH this table AND the
    // `if p.read_only_root_fs` blocks in common/sts.rs (Volume +
    // VolumeMount pair). vm-fetcher-split-k3s catches misses.
    std::fs::create_dir_all(&cfg.fuse_mount_point)?;
    std::fs::create_dir_all(&cfg.overlay_base_dir)?;
    // nix's `LocalStore` (chroot-store via `--store local?root=X`)
    // refuses to open if any ancestor of X is world-writable. The k8s
    // emptyDir at overlay_base_dir is 0777; clamp it and its parent.
    {
        use anyhow::Context as _;
        use std::os::unix::fs::PermissionsExt;
        let mode_755 = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&cfg.overlay_base_dir, mode_755.clone()).with_context(|| {
            format!(
                "chmod 0755 {} (nix LocalStore refuses world-writable ancestor)",
                cfg.overlay_base_dir.display()
            )
        })?;
        if let Some(parent) = cfg.overlay_base_dir.parent()
            && let Err(e) = std::fs::set_permissions(parent, mode_755)
        {
            tracing::warn!(path = %parent.display(), error = %e, "chmod parent of overlay_base_dir");
        }
    }

    let (fuse_session, fuse_circuit) = crate::fuse::mount_fuse_background(
        &cfg.fuse_mount_point,
        cache,
        store_clients.clone(),
        runtime.clone(),
        cfg.fuse_passthrough,
        cfg.fuse_threads,
        fuse_fetch_timeout,
    )?;

    info!(
        mount_point = %cfg.fuse_mount_point.display(),
        "FUSE store mounted"
    );

    // ---- BuildExecution stream with reconnect ----
    //
    // Architecture: a PERMANENT sink channel (sink_tx, sink_rx)
    // lives for process lifetime. BuildSpawnContext holds sink_tx
    // — running builds send CompletionReport/LogBatch here.
    // sink_rx is drained by a relay task that pumps into whatever
    // gRPC outbound channel is currently live (via watch::channel).
    //
    // Why: stderr_loop.rs breaks the build with MiscFailure if
    // its log send fails (channel closed). If we handed build
    // tasks the gRPC channel directly, stream death on scheduler
    // failover would kill every running build. With the permanent
    // sink, the build tasks' channel NEVER closes — the relay
    // just buffers (up to mpsc capacity) during the ~1s gap
    // between old-stream-dead and new-stream-open.
    //
    // The relay recovers the one message lost on transition
    // (mpsc::error::SendError<T> holds the unsent message) and
    // blocks on watch.changed() until the reconnect loop swaps
    // in a fresh gRPC channel.
    // r[impl builder.relay.reconnect]
    let (sink_tx, sink_rx) = mpsc::channel::<ExecutorMessage>(256);

    // Relay target: Some(grpc_tx) while connected, None during
    // the reconnect gap. Starts None — the reconnect loop sets
    // it before opening the first stream.
    let (relay_target_tx, relay_target_rx) =
        watch::channel::<Option<mpsc::Sender<ExecutorMessage>>>(None);

    rio_common::task::spawn_monitored("stream-relay", relay_loop(sink_rx, relay_target_rx));

    // P0537: one build per pod. The slot tracks both occupancy and
    // the running drv_path (heartbeat reads it). `try_claim` is
    // non-blocking — see BuildSlot doc for why.
    let slot = Arc::new(BuildSlot::default());

    // I-063: drain state. Set true on first SIGTERM. Heartbeat reports
    // it (worker is authority); the assignment handler rejects while
    // set; the reconnect loop KEEPS the stream alive (completions for
    // in-flight builds reach whichever scheduler is leader) until
    // `drain_done` fires (slot idle via wait_idle()).
    let draining = Arc::new(AtomicBool::new(false));
    let drain_done = Arc::new(Notify::new());

    // Build-complete signal. Notified after the one build is spawned
    // AND its permit returns (CompletionReport sent — spawn_build_task's
    // scopeguard drops the permit after build_tx.send(completion).await).
    // The select loop awaits this; one notification = exit.
    //
    // Notify not oneshot: Notify is cheaper (no channel allocation)
    // and `notified()` is cancel-safe for select!.
    let build_done = Arc::new(Notify::new());

    // Latest generation observed in an accepted HeartbeatResponse.
    // Starts at 0 — scheduler generation is always ≥1 (lease/mod.rs
    // non-K8s path starts at 1; k8s Lease increments from 1 on first
    // acquire), so 0 never rejects a real assignment. Relaxed ordering:
    // this is a fence against a DIFFERENT process's stale writes, not a
    // within-process happens-before. The value itself is the signal.
    let latest_generation = Arc::new(AtomicU64::new(0));
    // Shared with BuildSpawnContext below so the per-build daemon's
    // `extra-platforms` matches what the heartbeat advertises.
    let systems: std::sync::Arc<[String]> = systems.into();
    let heartbeat_handle = spawn_heartbeat(HeartbeatCtx {
        executor_id: executor_id.clone(),
        executor_kind: cfg.executor_kind,
        systems: systems.to_vec(),
        // move: setup() has no further use for features.
        features,
        intent_id: cfg.intent_id.clone(),
        slot: Arc::clone(&slot),
        ready: Arc::clone(&ready),
        resources: Arc::clone(&resource_snapshot),
        // FUSE circuit breaker: polled each tick. move into the task:
        // setup() has no other use for the handle.
        circuit: fuse_circuit,
        draining: Arc::clone(&draining),
        generation: Arc::clone(&latest_generation),
        client: scheduler_client.clone(),
    });

    // Shared context for spawning build tasks (clones done once per assignment
    // inside spawn_build_task, not here).
    let build_ctx = BuildSpawnContext {
        store_clients: store_clients.clone(),
        executor_id,
        fuse_mount_point: cfg.fuse_mount_point,
        overlay_base_dir: cfg.overlay_base_dir,
        // The permanent sink, NOT a per-connection gRPC channel.
        // Build tasks' sends never fail on scheduler failover.
        stream_tx: sink_tx,
        slot: Arc::clone(&slot),
        log_limits: crate::log_stream::LogLimits {
            rate_lines_per_sec: cfg.log_rate_limit,
            total_bytes: cfg.log_size_limit,
        },
        daemon_timeout: cfg.daemon_timeout,
        max_silent_time: cfg.max_silent_time.as_secs(),
        cgroup_parent,
        executor_kind: cfg.executor_kind,
        systems,
        // I-110c: same Arc as prefetch_cache / the FUSE mount —
        // executor primes manifest hints + JIT allowlist, FUSE threads
        // consume them.
        fuse_cache: Arc::clone(&prefetch_cache),
        // Base per-path fetch timeout; JIT lookup scales it with
        // nar_size (I-178). Same value the PrefetchHint handler uses.
        fuse_fetch_timeout,
        // Empty (non-k8s / VM tests) → None: proto3 optional string
        // semantics — absent on the wire, scheduler reads "unknown hw".
        node_name: (!cfg.node_name.is_empty()).then(|| cfg.node_name.clone()),
        // Same Arc as the heartbeat loop (above) — completion reads
        // the snapshot the cgroup poller has been maintaining.
        resources: resource_snapshot,
    };

    Ok(Some(BuilderRuntime {
        scheduler_addr: cfg.scheduler.addr,
        scheduler_client,
        shutdown,
        fuse_session,
        relay_target_tx,
        slot,
        draining,
        drain_done,
        build_done,
        latest_generation,
        heartbeat_handle,
        build_ctx,
        prefetch: PrefetchDeps {
            cache: prefetch_cache,
            clients: store_clients,
            runtime,
            // Prefetch concurrency limit. 8 is conservative: each holds a
            // tokio blocking-pool thread (default pool is 512, so no
            // starvation concern) AND pins an in-flight gRPC stream to the
            // store (which is what we're bounding — don't DDoS the store with
            // 100 parallel NARs when the scheduler sends a big hint list).
            sem: Arc::new(Semaphore::new(8)),
            fetch_timeout: fuse_fetch_timeout,
        },
        idle_timeout: cfg.idle_timeout,
        _balance_guard,
    }))
}

/// Resolve executor_id / systems / features from config + environment.
/// Consumes the config's owned fields (caller passes via `mem::take` —
/// main() has no further use for them).
///
/// Errors if executor_id is empty AND gethostname() fails — two workers
/// with the same ID would steal each other's builds via heartbeat
/// merging, so we fail hard rather than silently colliding on "unknown".
pub(super) fn resolve_executor_identity(
    executor_id: String,
    systems: Vec<String>,
    features: Vec<String>,
) -> anyhow::Result<(String, Vec<String>, Vec<String>)> {
    let executor_id = if executor_id.is_empty() {
        nix::unistd::gethostname()
            .ok()
            .and_then(|h| h.into_string().ok())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "cannot determine executor_id: gethostname() failed and \
                     executor_id not set (--worker-id, RIO_WORKER_ID, or worker.toml)"
                )
            })?
    } else {
        executor_id
    };

    // systems: auto-detect single element when not configured.
    // A worker with zero systems is useless (scheduler's hard_filter
    // never matches) — auto-detect is a sensible default, not a
    // silent fallback for misconfiguration.
    let mut systems = if systems.is_empty() {
        vec![detect_system()]
    } else {
        systems
    };
    // r[impl sched.dispatch.fod-builtin-any-arch]
    // Every nix-daemon supports builtin:fetchurl — it's handled
    // internally, no real process forked. Bootstrap derivations
    // (busybox, bootstrap-tools) have system="builtin"; without
    // this, a cold store permanently stalls at the DAG leaves.
    // With per-arch fetcher Pools, this is what makes a `builtin`
    // FOD eligible on either arch's fetchers (hard_filter matches
    // on the union; best_executor scores across both).
    if !systems.iter().any(|s| s == "builtin") {
        systems.push("builtin".to_string());
    }
    // features: no auto-detect. Empty is valid (worker supports no
    // special features). Operator sets these explicitly in the CRD
    // — auto-detecting "kvm" by checking /dev/kvm exists would be
    // surprising (worker on a kvm-capable host but operator wants
    // to reserve it for other work).

    Ok((executor_id, systems, features))
}

/// I-098: refuse to start when the host arch isn't in `RIO_SYSTEMS`.
/// A Pool with `systems=[x86_64-linux]` whose pod lands on an
/// arm64 node would otherwise register as x86_64, accept x86_64 drvs,
/// and have nix-daemon refuse them at build time. CrashLoopBackOff is
/// the right shape — visible in `kubectl get pods`, doesn't poison drvs.
///
/// Fetchers skip this: FODs are `builtin` (arch-agnostic) so a fetcher
/// on the "wrong" arch is fine — and intentional (cheaper Gravitons).
/// `host` is a parameter (not `detect_system()` inline) for testability.
pub(super) fn validate_host_arch(
    kind: rio_proto::types::ExecutorKind,
    systems: &[String],
    host: &str,
) -> anyhow::Result<()> {
    use rio_proto::types::ExecutorKind;
    if kind == ExecutorKind::Fetcher {
        return Ok(());
    }
    let mut non_builtin = systems.iter().filter(|s| s.as_str() != "builtin");
    if non_builtin.clone().next().is_none() {
        return Ok(());
    }
    if non_builtin.any(|s| s == host) {
        return Ok(());
    }
    anyhow::bail!(
        "host system {host:?} not in RIO_SYSTEMS={systems:?} — pod likely \
         scheduled onto wrong-arch node. Fix the pool's nodeSelector or \
         systems list."
    )
}

/// cgroup v2 setup + background utilization reporter spawn.
///
/// HARD REQUIREMENT — `?` on both delegated_root and
/// enable_subtree_controllers. Fail startup loudly rather than silently
/// fall back to broken metrics (the phase2c VmHWM bug measured ~10MB
/// for every build; poisoning build_samples like that mis-trains the
/// SLA fit until the ring buffer cycles).
///
/// `delegated_root()` returns the PARENT of /proc/self/cgroup — NOT
/// own_cgroup(). cgroup v2's no-internal-processes rule means per-build
/// cgroups must be SIBLINGS of where the worker process is, not
/// children. systemd DelegateSubgroup=builds puts the worker in
/// .../service/builds/; delegated_root() returns .../service/ (empty,
/// writable via Delegate=yes); per-build cgroups go there as siblings
/// of builds/.
///
/// `enable_subtree_controllers` writes +memory +cpu (fails on EACCES =
/// Delegate=yes not configured).
fn init_cgroup(
    overlay_base_dir: &std::path::Path,
    shutdown: rio_common::signal::Token,
) -> anyhow::Result<(std::path::PathBuf, crate::cgroup::ResourceSnapshotHandle)> {
    let cgroup_parent =
        crate::cgroup::delegated_root().map_err(|e| anyhow::anyhow!("cgroup v2 required: {e}"))?;
    crate::cgroup::enable_subtree_controllers(&cgroup_parent)
        .map_err(|e| anyhow::anyhow!("cgroup delegation required: {e}"))?;
    info!(cgroup = %cgroup_parent.display(), "cgroup v2 subtree ready");

    // Background utilization reporter: polls parent cgroup cpu.stat +
    // memory.current/max every 10s → Prometheus gauges AND the shared
    // snapshot the heartbeat loop reads for ResourceUsage. Single
    // sampling site means Prometheus and ListExecutors always agree.
    // Shutdown token lets the 10s sleep break immediately on SIGTERM
    // so main() can return and profraw flush.
    let resource_snapshot: crate::cgroup::ResourceSnapshotHandle = Default::default();
    rio_common::task::spawn_monitored(
        "cgroup-utilization-reporter",
        crate::cgroup::utilization_reporter_loop_with_shutdown(
            cgroup_parent.clone(),
            overlay_base_dir.to_path_buf(),
            std::sync::Arc::clone(&resource_snapshot),
            shutdown,
        ),
    );

    Ok((cgroup_parent, resource_snapshot))
}

/// Retry-until-connected store + scheduler clients via
/// [`connect_forever`](rio_proto::client::connect_forever)
/// (shutdown-aware, exponential backoff).
///
/// Cold-start race: store/scheduler Services may have no endpoints
/// yet. /healthz stays 200 (process IS alive, restart won't help),
/// /readyz stays 503 (ready flag won't flip until first heartbeat
/// accepted, far past this loop).
///
/// Returns `None` if shutdown fires during retry — caller exits
/// main() cleanly (nothing to drain, never connected).
///
/// Scheduler has two modes:
/// - Balanced (K8s, multi-replica): DNS-resolve headless Service,
///   health-probe pod IPs, route to leader. Heartbeat routes through
///   the same balanced channel — leadership flip detected within one
///   probe tick (~3s).
/// - Single (non-K8s): plain connect. VM tests use this.
async fn connect_upstreams(
    cfg: &crate::config::Config,
    shutdown: &rio_common::signal::Token,
) -> Option<(StoreClients, WorkerClient, BalanceGuards)> {
    rio_proto::client::connect_forever(shutdown, || async {
        // `connect_raw` returns the bare Channel; StoreClients wraps it
        // in the typed StoreService client with the standard message-size
        // headroom.
        let (ch, store_guard) =
            rio_proto::client::connect_raw::<rio_proto::StoreServiceClient<_>>(&cfg.store).await?;
        let store = StoreClients::from_channel(ch);
        let (sched, sched_guard) = rio_proto::client::connect(&cfg.scheduler).await?;
        anyhow::Ok((store, sched, (store_guard, sched_guard)))
    })
    .await
}
