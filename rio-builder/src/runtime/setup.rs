//! Cold-start wiring: identity, host-arch validation, cgroup init,
//! upstream connect, FUSE mount, build context.
//!
//! Everything `main()` does before the pull loop. Produces a
//! [`BuilderRuntime`] consumed by [`run`](super::run).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::mpsc;
use tracing::{info, warn};

use rio_proto::types::ExecutorKind;

use super::slot::BuildSlot;
use super::{BuildSpawnContext, BuilderRuntime};
use crate::config::{Config, detect_system};
use crate::executor::BuildTaskMessage;
use crate::store_fetch::StoreClients;

pub(super) type WorkerClient = rio_proto::ExecutorServiceClient<tonic::transport::Channel>;

/// Probe-loop guards for both balanced channels. Dropping a
/// `BalancedChannel` stops its probe loop, so these must outlive the
/// clients. Either can be `None` (single-channel fallback).
pub(super) type BalanceGuards = (
    Option<rio_proto::client::balance::BalancedChannel>,
    Option<rio_proto::client::balance::BalancedChannel>,
);

/// Wire up cgroups, health server, gRPC clients, FUSE mount, and the
/// build context. Everything `main()` does before the pull loop.
///
/// Returns `None` if shutdown fired during cold-start connect — caller
/// exits cleanly (nothing started, never connected). Returns `Err`
/// when the cold-start connect budget is exceeded (live_056-b) —
/// caller propagates and the process exits NONZERO so the wedge is
/// visible platform-side.
pub async fn setup(
    mut cfg: Config,
    shutdown: rio_common::signal::Token,
) -> anyhow::Result<Option<BuilderRuntime>> {
    let (executor_id, systems, _features) = resolve_executor_identity(
        cfg.executor_kind,
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
    // false until the pull loop has an assignment in hand — a pod that
    // is still asking for work is not useful capacity.
    let ready = Arc::new(AtomicBool::new(false));
    crate::health::spawn_health_server(cfg.health_addr, Arc::clone(&ready), shutdown.clone());

    // live_056-b: the `?` is the cold-start envelope's exit conduit —
    // a budget breach propagates to main() and the process exits
    // NONZERO (Job backoff / CrashLoopBackOff carry the failure).
    let Some((store_clients, scheduler_client, _balance_guard)) =
        connect_upstreams(&cfg, &shutdown).await?
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
        "connected to gRPC services"
    );
    // live_056-b: the serving-state signal — past cold start, channels
    // live, about to ask for work (post-connect, pre-first-pull). The
    // Job's exec readiness probe tests this file, so Pod Ready ⟺
    // serving (W9-CO); a wedged cold-start never creates it. (The
    // /readyz flag is a DIFFERENT axis: pulled/building — useful
    // capacity, not serving state.) Best-effort: a failed write
    // leaves the pod NotReady — observable, never blocking.
    if let Err(e) = std::fs::write(rio_common::k8s::BUILDER_SERVING_STATE_FILE, b"serving\n") {
        tracing::warn!(
            error = %e,
            path = rio_common::k8s::BUILDER_SERVING_STATE_FILE,
            "failed to write the serving-state file; the pod will stay NotReady"
        );
    }

    // ADR-023 phase-10 hw self-calibration. Resolve `hw_class` from
    // the downward-API volume (bounded poll — the controller stamps
    // the annotation reactively after `spec.nodeName` binds, so the
    // file may be empty for the first ~1s). `cfg.hw_class` (env var)
    // is the test-injection override. The bench itself is a ~5s
    // CPU-bound microbench in a blocking thread.
    //
    // The whole resolve→bench chain is SPAWNED so it runs concurrently
    // with the FUSE mount below — `POLL_BOUND=30s` was
    // sized assuming overlap with the ~30s FUSE cold-start, so an
    // annotator outage adds 0s (not 30s) to startup. Both products
    // (`hw_class` string + bench `factor`) are consumed only at first
    // WorkAssignment (`spawn_build_task` takes `BuildSpawnContext::
    // hw_bench`); the store gates `AppendHwPerfSample` on the
    // assignment token (`r[sec.boundary.grpc-hmac]`).
    let hw_class_override = (!cfg.hw_class.is_empty()).then(|| cfg.hw_class.clone());
    let hw_bench_needed = cfg.hw_bench_needed;
    let overlay_dir = cfg.overlay_base_dir.clone();
    let hw_bench = tokio::spawn(async move {
        let hw_class = match hw_class_override {
            Some(c) => c,
            None => crate::hw_class::resolve().await.unwrap_or_default(),
        };
        // Flatten: await the K=3 bench handle inside this task so the
        // consumer sees one JoinHandle<(String, Option<(alu, membw?, ioseq?)>)>.
        let factor = match crate::hw_bench::spawn_measure(&hw_class, hw_bench_needed, overlay_dir) {
            Some(h) => h.await.ok(),
            None => None,
        };
        (hw_class, factor)
    });

    // Set up FUSE cache and mount. Arc so the executor's manifest-prime
    // / JIT-allowlist path can share it with the FUSE threads.
    let fuse_cache_dir = cfg.fuse_cache_dir.clone();
    let cache = Arc::new(crate::fuse::cache::Cache::new(cfg.fuse_cache_dir)?);
    let executor_cache = Arc::clone(&cache);
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
        runtime,
        cfg.fuse_passthrough,
        cfg.fuse_threads,
        fuse_fetch_timeout,
    )?;

    info!(
        mount_point = %cfg.fuse_mount_point.display(),
        "FUSE store mounted"
    );

    // ---- The build-task sink ----
    //
    // A process-lifetime sink channel (sink_tx, sink_rx).
    // BuildSpawnContext holds sink_tx — the build task sends its
    // CompletionReport (and phase edges) here; the pull loop consumes
    // sink_rx directly and forwards the report through `ReportOutcome`
    // until acknowledged.
    //
    // Why a process-lifetime channel: stderr_loop.rs breaks the build
    // with MiscFailure if its send fails (channel closed). The sink
    // never closes while the build runs, so the build task's sends
    // cannot fail regardless of scheduler availability — delivery
    // retries live entirely in the pull loop's report phase.
    let (sink_tx, sink_rx) = mpsc::channel::<BuildTaskMessage>(256);

    // P0537: one build per pod. The slot tracks both occupancy and
    // the running drv_path. `try_claim` is non-blocking — see
    // BuildSlot doc for why.
    let slot = Arc::new(BuildSlot::default());

    // Per-build daemon's `extra-platforms` matches the resolved
    // identity systems.
    let systems: std::sync::Arc<[String]> = systems.into();

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
        fuse_cache_dir,
        daemon_timeout: cfg.daemon_timeout,
        max_silent_time: cfg.max_silent_time.as_secs(),
        cgroup_parent,
        executor_kind: cfg.executor_kind,
        systems,
        // I-110c: same Arc as the FUSE mount — executor primes
        // manifest hints + JIT allowlist, FUSE threads consume them.
        fuse_cache: executor_cache,
        // Base per-path fetch timeout; JIT lookup scales it with
        // nar_size (I-178).
        fuse_fetch_timeout,
        // Empty (non-k8s / VM tests) → None: proto3 optional string
        // semantics — absent on the wire, scheduler reads "unknown hw".
        node_name: (!cfg.node_name.is_empty()).then(|| cfg.node_name.clone()),
        // Populated lazily by `spawn_build_task` when it bounded-awaits
        // `hw_bench` on the first assignment (falls back to a background
        // task if the bench is still running). Before then, `None` —
        // the documented "unknown hw" semantics.
        hw_class: Arc::new(std::sync::Mutex::new(None)),
        // bug_408: same Arc as the FUSE mount — completion stamps read
        // is_open()/trip_count() to mark store-degraded infra failures.
        fuse_circuit,
        // Completion reads the snapshot the cgroup poller has been
        // maintaining.
        resources: resource_snapshot,
        hw_bench: Arc::new(std::sync::Mutex::new(Some(hw_bench))),
    };

    Ok(Some(BuilderRuntime {
        scheduler_client,
        shutdown,
        fuse_session,
        slot,
        build_ctx,
        intent_id: cfg.intent_id.clone(),
        ready,
        pull_sink_rx: Some(sink_rx),
        executor_token: cfg.executor_token,
        idle_timeout: cfg.idle_timeout,
        _balance_guard,
    }))
}

/// Resolve executor_id / systems / features from config + environment.
/// Consumes the config's owned fields (caller passes via `mem::take` —
/// main() has no further use for them).
///
/// Errors if executor_id is empty AND gethostname() fails — the
/// executor identity keys log banners, per-execution log uploads, and
/// the open-attempt row, so we fail hard rather than silently
/// colliding on "unknown".
///
/// `kind` enforces the §13e biconditional (`Fetcher ⟺ [fetcher]`) at
/// identity resolution, mirroring the controller's
/// `effective_features(spec)` chokepoint — see
/// `rio-controller/src/reconcilers/pool/pod.rs` and
/// `r[ctrl.crd.fetcher-no-features]`.
pub(super) fn resolve_executor_identity(
    kind: ExecutorKind,
    executor_id: String,
    systems: Vec<String>,
    features: Vec<String>,
) -> anyhow::Result<(String, Vec<String>, Vec<String>)> {
    let executor_id = if executor_id.is_empty() {
        nix::unistd::gethostname()
            .ok()
            .and_then(|h| h.into_string().ok())
            .ok_or_else(|| {
                // bug_156: derive the hint — hand-typed `--worker-id,
                // RIO_WORKER_ID, or worker.toml` here survived the
                // worker_id→executor_id rename and sent operators to
                // a knob the config loader silently ignores.
                anyhow::anyhow!(
                    "cannot determine executor_id: gethostname() failed and \
                     executor_id not set ({})",
                    rio_common::config::config_hint("executor_id", "builder")
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
    // r[impl sched.dispatch.fod-builtin-any-arch+2]
    // Every nix-daemon supports builtin:fetchurl — it's handled
    // internally, no real process forked. Bootstrap derivations
    // (busybox, bootstrap-tools) have system="builtin"; without
    // this, a cold store permanently stalls at the DAG leaves.
    // The executor therefore always treats `builtin` as a supported
    // system, on either arch's fetchers (the spawn path adds no arch
    // constraint for builtin intents).
    if !systems.iter().any(|s| s == "builtin") {
        systems.push("builtin".to_string());
    }
    // features: §13e biconditional `Fetcher ⟺ [fetcher]` enforced at
    // identity resolution — the same `effective_features(spec)`
    // chokepoint the controller applies when injecting `RIO_FEATURES`
    // (rio-controller/src/reconcilers/pool/pod.rs). The controller's
    // injection covers k8s-spawned pods; this covers every OTHER
    // deployment path (NixOS module, manual env, future operators)
    // where `RIO_EXECUTOR_KIND=fetcher` is set without `RIO_FEATURES`.
    // Feature matching itself happens at the spawn-intent filter (the
    // pod is born for a specific drv), so the resolved set is a
    // misconfiguration tripwire here, not an advertisement.
    //
    // The override is unconditional (matches the controller's): a
    // fetcher declaring `[kvm]` would otherwise claim a feature it
    // can't honor (fetcher pods have no /dev/kvm) AND drop the routing
    // tag FODs match on. Warn so a misconfigured operator gets a log
    // line, not silence.
    //
    // Builder kind: declared verbatim, no auto-detect. Empty is valid
    // (worker supports no special features). Operator sets these
    // explicitly in the CRD — auto-detecting "kvm" by checking
    // /dev/kvm exists would be surprising (worker on a kvm-capable
    // host but operator wants to reserve it for other work).
    let fetcher_only = vec![rio_common::k8s::FETCHER_FEATURE.to_string()];
    let features = if kind == ExecutorKind::Fetcher {
        if features != fetcher_only && !features.is_empty() {
            warn!(
                declared = ?features,
                "fetcher executor declared non-[fetcher] features; \
                 overriding to [fetcher] (§13e biconditional — fetchers \
                 advertise the routing tag and nothing else)"
            );
        }
        fetcher_only
    } else {
        features
    };

    Ok((executor_id, systems, features))
}

/// I-098: refuse to start when the host arch isn't in `RIO_SYSTEMS`.
/// A Pool with `systems=[x86_64-linux]` whose pod lands on an
/// arm64 node would otherwise register as x86_64, accept x86_64 drvs,
/// and have nix-daemon refuse them at build time. CrashLoopBackOff is
/// the right shape — visible in `kubectl get pods`, doesn't poison drvs.
///
/// r35 bug_039: Fetcher workers DO need arch validation for the
/// arch-typed FODs (`x86_64-linux pkgs.fetchurl`) in
/// `pool.spec.systems`. The pre-§13e helm-static fetcher arch
/// nodeSelector that compensated for the old `kind == Fetcher`
/// early-return was deleted in §13e — both compensations gone meant a
/// misplaced fetcher silently registered, accepted dispatch, and
/// failed builds at run-time. A misplaced fetcher now refuses
/// arch-typed systems at register time instead. `["builtin"]`-only
/// fetchers (the common `builtins.fetchurl` case) stay arch-agnostic
/// — the `non_builtin.is_none()` early-return below covers them. The
/// `kind` param is retained for the call signature and a future third
/// `ExecutorKind` with different arch rules; it no longer gates.
///
/// `host` is a parameter (not `detect_system()` inline) for testability.
pub(super) fn validate_host_arch(
    _kind: rio_proto::types::ExecutorKind,
    systems: &[String],
    host: &str,
) -> anyhow::Result<()> {
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
    // snapshot `completion_stamp` reads for the report's
    // `final_resources`. Single sampling site means Prometheus and the
    // completion telemetry always agree. Shutdown token lets the 10s
    // sleep break immediately on SIGTERM so main() can return and
    // profraw flush.
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

/// live_056-b R17 envelope: the cold-start first-connect budget —
/// typed, VIOLABLE const. Axes: time = 120 s (load-bearing — the
/// wedge mode is hung connects, where attempts never complete);
/// attempts = derived (~10 at the 1→16 s capped curve when attempts
/// fail fast; fewer when they hang). Derivation: (a) ≥ 20× the
/// healthy cold-start (helm install / node churn resolves in
/// single-digit seconds, covered by attempt 4 of the curve); (b)
/// ≥ 5 capped retries past the curve's 31 s ramp, so a slow-rolling
/// upstream restart is ridden out; (c) strictly under the
/// controller's 300 s `ORPHAN_REAP_GRACE`, so a wedged cold-start
/// EXITS and surfaces through the typed terminal lane (Job Failed →
/// counted death → escalating respawn backoff) before the blind
/// orphan reap would recycle the same name invisibly. Violable: a
/// deployment whose cold path legitimately exceeds this raises the
/// const WITH a new derivation note — deliberately not config (a
/// deployment invariant, not an operator dial; config would drag the
/// BLESS/docs-data schema obligations).
const COLD_START_CONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);

/// Bounded first-connect of store + scheduler clients via
/// [`connect_within`](rio_proto::client::connect_within) —
/// shutdown-aware, exponential backoff, COLD-START budget
/// ([`COLD_START_CONNECT_BUDGET`]).
///
/// live_056-b: this is deliberately the cold-start posture ONLY. The
/// dual face is load-bearing — once serving, an upstream outage must
/// not kill a worker holding claim state (post-serving traffic rides
/// the tonic channel's own reconnect; no other builder call site
/// uses a bounded connect), and the controller's bind-before-connect
/// bootstrap keeps `connect_forever`'s infinite posture untouched.
///
/// Cold-start race: store/scheduler Services may have no endpoints
/// yet. /healthz stays 200 (process IS alive, restart won't help),
/// /readyz stays 503 (ready flag won't flip until the pull loop has
/// an assignment in hand, far past this loop); the serving-state
/// file ([`rio_common::k8s::BUILDER_SERVING_STATE_FILE`]) does not
/// exist yet, so the Job's exec readiness probe holds the pod
/// NotReady for exactly the un-served window.
///
/// Returns `Ok(None)` if shutdown fires during retry — caller exits
/// main() cleanly (nothing started, never connected). Returns
/// `Err` when the budget is exceeded — the caller propagates and
/// main() exits NONZERO, so the platform's own escalation alphabet
/// (Job backoff / CrashLoopBackOff) makes the failure visible
/// (W9-CN: the incident's invisible-wedge inverse).
///
/// Scheduler has two modes:
/// - Balanced (K8s, multi-replica): DNS-resolve headless Service,
///   health-probe pod IPs, route to leader. The pull/report unaries
///   route through the same balanced channel — leadership flip
///   detected within one probe tick (~3s).
/// - Single (non-K8s): plain connect. VM tests use this.
// r[impl sys.guard.kill-wired-isolated]
async fn connect_upstreams(
    cfg: &crate::config::Config,
    shutdown: &rio_common::signal::Token,
) -> anyhow::Result<Option<(StoreClients, WorkerClient, BalanceGuards)>> {
    let connected =
        rio_proto::client::connect_within(COLD_START_CONNECT_BUDGET, shutdown, || async {
            // `connect_raw` returns the bare Channel; StoreClients wraps it
            // in the typed StoreService client with the standard message-size
            // headroom.
            let (ch, store_guard) =
                rio_proto::client::connect_raw::<rio_proto::StoreServiceClient<_>>(&cfg.store)
                    .await?;
            let store = StoreClients::from_channel(ch);
            let (sched, sched_guard) = rio_proto::client::connect(&cfg.scheduler).await?;
            anyhow::Ok((store, sched, (store_guard, sched_guard)))
        })
        .await?;
    Ok(connected)
}
