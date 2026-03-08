//! Build executor: receives WorkAssignment from scheduler, runs builds.
//!
//! Flow:
//! 1. Set up overlay for the build
//! 2. Generate synthetic SQLite DB with input closure metadata
//! 3. Spawn `nix-daemon --stdio` in overlay
//! 4. Client handshake + wopSetOptions + wopBuildDerivation
//! 5. Stream logs via LogBatcher -> BuildLogBatch -> scheduler stream
//! 6. On completion: upload outputs to store via PutPath
//! 7. Send CompletionReport on scheduler stream
//! 8. Tear down overlay
//!
//! FOD handling: detect fixed-output derivations via `is_fixed_output`
//! flag on WorkAssignment, skip network namespace isolation.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tonic::transport::Channel;
use tracing::instrument;

use futures_util::stream::{self, StreamExt, TryStreamExt};
use rio_nix::derivation::Derivation;
use rio_proto::StoreServiceClient;
use rio_proto::types::{
    BuildResult as ProtoBuildResult, BuildResultStatus, BuiltOutput, WorkAssignment, WorkerMessage,
};

use crate::log_stream::{LogBatcher, LogLimits};
use crate::overlay;
use crate::synth_db::{self, SynthDrvOutput};
use crate::upload;

mod daemon;
mod inputs;

use daemon::{run_daemon_build, spawn_daemon_in_namespace};
use inputs::{
    compute_input_closure, fetch_drv_from_store, fetch_input_metadata, verify_fod_hashes,
};

/// Max concurrent gRPC calls for input metadata/drv fetches.
/// Bounds memory (each in-flight QueryPathInfo response is small; each
/// GetPath .drv stream is typically <10 KB). 16 saturates a LAN without
/// thundering the store.
const MAX_PARALLEL_FETCHES: usize = 16;

/// Per-worker immutable configuration for `execute_build`.
///
/// These don't change per-assignment — they're set once at startup from
/// CLI/config. Bundled to avoid `too_many_arguments` on `execute_build`
/// (the alternative was `#[allow]`, which we don't use in prod code).
///
/// Distinct from `BuildSpawnContext` (lib.rs): that holds Arc-shared
/// state (stream_tx, running_builds) that needs per-call cloning; this
/// holds `Copy`/cheap-to-copy config that can just be passed by value.
#[derive(Clone)]
pub struct ExecutorEnv {
    pub fuse_mount_point: std::path::PathBuf,
    pub overlay_base_dir: std::path::PathBuf,
    pub worker_id: String,
    pub log_limits: LogLimits,
    /// Threshold for leaked overlay mounts before refusing new builds.
    /// A leaked mount means `umount2` failed in `OverlayMount::Drop` —
    /// typically the mount is stuck busy (open file handles, zombie
    /// nix-daemon). After N leaks the worker is degraded; refusing builds
    /// and reporting `InfrastructureFailure` lets the scheduler reassign
    /// to a healthy worker, and the supervisor can restart this one.
    pub max_leaked_mounts: usize,
    /// Timeout for the local nix-daemon subprocess build. Used when the
    /// client didn't specify `BuildOptions.build_timeout`. Intentionally
    /// long (default 2h) — some builds genuinely take that long; the
    /// purpose is to bound blast radius of a truly stuck daemon.
    pub daemon_timeout: Duration,
    /// Parent cgroup for per-build sub-cgroups. This is
    /// `cgroup::delegated_root()` — the PARENT of the worker's own
    /// cgroup (with `DelegateSubgroup=builds`, worker lives in
    /// `.../service/builds/`; delegated_root is `.../service/`).
    /// Per-build cgroups go here as SIBLINGS. main.rs computes it
    /// ONCE at startup (and calls `enable_subtree_controllers` on
    /// it, which fail-fasts on delegation misconfig). Each build
    /// gets a sub-cgroup named by drv hash. cgroup v2 is a hard
    /// requirement — no Option.
    pub cgroup_parent: std::path::PathBuf,
}

/// Default daemon build timeout: 2 hours. See `ExecutorEnv.daemon_timeout`.
pub const DEFAULT_DAEMON_TIMEOUT: Duration = Duration::from_secs(7200);

/// Worker nix.conf content for sandbox builds.
const WORKER_NIX_CONF: &str = "\
builders =
substitute = false
sandbox = true
sandbox-fallback = false
restrict-eval = true
experimental-features =
";

/// Error type for executor operations.
///
/// No `#[from] anyhow::Error` — every variant has a typed source. Before
/// C1, `Overlay(#[from] anyhow::Error)` was the only type-erasing `From`
/// in the codebase: any `?` on an `anyhow::Result` anywhere in
/// `execute_build` became "overlay setup failed", even for unrelated
/// failures. Now the compiler catches those at the `?` site.
#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error("overlay setup failed: {0}")]
    Overlay(#[from] overlay::OverlayError),
    #[error("overlay setup task panicked: {0}")]
    OverlayTaskPanic(tokio::task::JoinError),
    #[error("synthetic DB generation failed: {0}")]
    SynthDb(#[from] sqlx::Error),
    #[error("failed to write nix.conf: {0}")]
    NixConf(#[source] std::io::Error),
    #[error("daemon spawn failed: {0}")]
    DaemonSpawn(std::io::Error),
    #[error("daemon handshake failed: {0}")]
    Handshake(#[from] rio_nix::protocol::handshake::HandshakeError),
    #[error("daemon setup failed: {0}")]
    DaemonSetup(String),
    #[error("build failed: {0}")]
    BuildFailed(String),
    #[error("upload failed: {0}")]
    Upload(#[from] upload::UploadError),
    #[error("gRPC error: {0}")]
    Grpc(#[from] tonic::Status),
    #[error("input metadata fetch failed for {path}: {source}")]
    MetadataFetch { path: String, source: tonic::Status },
    #[error("wire protocol error: {0}")]
    Wire(#[from] rio_nix::protocol::wire::WireError),
    #[error("cgroup resource tracking failed: {0}")]
    Cgroup(String),
}

/// Result of executing a single build.
#[derive(Debug)]
pub struct ExecutionResult {
    /// The derivation path that was built.
    pub drv_path: String,
    /// The proto BuildResult.
    pub result: ProtoBuildResult,
    /// Assignment token from the WorkAssignment.
    pub assignment_token: String,
    /// Peak memory in bytes from the per-build cgroup's `memory.peak`.
    /// Tree-wide: daemon + builder + every child compiler. 0 = build
    /// failed before cgroup populated (executor error before spawn).
    pub peak_memory_bytes: u64,
    /// Total bytes uploaded (sum of NAR sizes). 0 on failure.
    pub output_size_bytes: u64,
    /// Peak CPU cores-equivalent, polled 1Hz from cgroup `cpu.stat`
    /// usage_usec. `delta_usec / elapsed_usec`, max over build lifetime.
    /// Tree-wide. 0.0 = build failed before any sample (exited <1s).
    pub peak_cpu_cores: f64,
}

/// Read `cpu.stat` `usage_usec` from a cgroup path. Free fn (not a
/// method on BuildCgroup) so the CPU poll task can clone the PATH
/// and call this without holding a `&BuildCgroup` across the
/// `run_daemon_build` await.
///
/// Thin wrapper over the pure parser in cgroup.rs. `None` on read
/// fail (cgroup directory removed mid-poll — shouldn't happen, the
/// executor drops BuildCgroup AFTER the poll task is aborted).
fn read_cpu_stat(cgroup_path: &Path) -> Option<u64> {
    let content = std::fs::read_to_string(cgroup_path.join("cpu.stat")).ok()?;
    crate::cgroup::parse_cpu_stat_usage_usec(&content)
}

/// Execute a single build assignment.
///
/// This is the main entry point for building a derivation. It handles
/// the full lifecycle: overlay setup, synthetic DB, daemon invocation,
/// log streaming, output upload, and cleanup.
///
/// This is the ROOT span for the worker's contribution to a build trace.
/// Per observability.md:152 (trace structure), child spans are:
/// `fetch_drv_from_store`, `compute_input_closure`, `fetch_input_metadata`,
/// `generate_db`, `spawn_daemon_in_namespace`, `run_daemon_build`,
/// `upload_all_outputs`. `drv_path` is the primary identifier (matches
/// scheduler's `drv_key` span field via the derivation hash substring).
#[instrument(
    skip_all,
    fields(
        drv_path = %assignment.drv_path,
        worker_id = %env.worker_id,
        is_fod = assignment.is_fixed_output,
    )
)]
pub async fn execute_build(
    assignment: &WorkAssignment,
    env: &ExecutorEnv,
    store_client: &mut StoreServiceClient<Channel>,
    log_tx: &mpsc::Sender<WorkerMessage>,
    leak_counter: &Arc<AtomicUsize>,
) -> Result<ExecutionResult, ExecutorError> {
    // Destructure for ergonomics — the body was written with these as
    // params, and rewriting every `fuse_mount_point` to `env.fuse_mount_point`
    // would be noise. The compiler inlines this away.
    let fuse_mount_point: &Path = &env.fuse_mount_point;
    let overlay_base_dir: &Path = &env.overlay_base_dir;
    let worker_id: &str = &env.worker_id;
    let log_limits = env.log_limits;

    let drv_path = &assignment.drv_path;
    let build_id = sanitize_build_id(drv_path);

    tracing::info!(
        drv_path = %drv_path,
        build_id = %build_id,
        is_fod = assignment.is_fixed_output,
        "starting build"
    );

    // Refuse new builds once the worker has leaked too many overlay mounts.
    // A leaked mount (umount2 failed in OverlayMount::Drop) usually means
    // the mount is stuck busy — open file handles, zombie nix-daemon, FUSE
    // hang. After N leaks the worker is degraded; returning
    // InfrastructureFailure here lets the scheduler reassign to a healthy
    // worker, and the supervisor can restart this one. We check at ENTRY,
    // not exit: a build that completes successfully shouldn't have its
    // result overridden just because its own teardown later fails — the
    // NEXT build is what gets refused.
    let leaked = leak_counter.load(Ordering::Relaxed);
    let threshold = env.max_leaked_mounts;
    if leaked >= threshold {
        tracing::error!(
            leaked,
            threshold,
            drv_path = %drv_path,
            "refusing build: leaked overlay mount threshold exceeded; worker needs restart"
        );
        return Ok(ExecutionResult {
            drv_path: drv_path.clone(),
            result: ProtoBuildResult {
                status: BuildResultStatus::InfrastructureFailure.into(),
                error_msg: format!(
                    "worker has {leaked} leaked overlay mounts (threshold {threshold}); refusing new builds"
                ),
                ..Default::default()
            },
            assignment_token: assignment.assignment_token.clone(),
            // Infra failure before daemon spawn → cgroup never
            // created, never populated. All resource fields = 0.
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        });
    }

    metrics::gauge!("rio_worker_builds_active").increment(1.0);
    // rio_worker_builds_total is incremented at completion (main.rs) with
    // an outcome label so SLI queries can compute success rate.
    let build_start = std::time::Instant::now();
    let _build_guard = scopeguard::guard((), move |()| {
        metrics::gauge!("rio_worker_builds_active").decrement(1.0);
        metrics::histogram!("rio_worker_build_duration_seconds")
            .record(build_start.elapsed().as_secs_f64());
    });

    // 1. Set up overlay. `setup_overlay` is synchronous (mkdir + stat +
    // overlayfs mount syscall); run on the blocking pool so a slow mount
    // (e.g., FUSE lower stalled on remote fetch) doesn't starve the Tokio
    // worker thread and block the heartbeat loop.
    let fuse_mp = fuse_mount_point.to_path_buf();
    let overlay_base = overlay_base_dir.to_path_buf();
    let build_id_owned = build_id.clone();
    let leak_counter_owned = leak_counter.clone();
    let overlay_mount = tokio::task::spawn_blocking(move || {
        overlay::setup_overlay(&fuse_mp, &overlay_base, &build_id_owned, leak_counter_owned)
    })
    .await
    .map_err(ExecutorError::OverlayTaskPanic)??;

    // 2. Parse the derivation. Scheduler inlines drv_content for
    // missing-output nodes (phase2c D8); empty means cache-hit or
    // inline-budget exceeded, so fall back to store fetch.
    let drv = if assignment.drv_content.is_empty() {
        fetch_drv_from_store(store_client, drv_path).await?
    } else {
        let drv_text = String::from_utf8_lossy(&assignment.drv_content);
        Derivation::parse(&drv_text)
            .map_err(|e| ExecutorError::BuildFailed(format!("failed to parse derivation: {e}")))?
    };

    // Resolve inputDrv outputs → add to BasicDerivation's inputSrcs.
    // `drv.to_basic()` only copies the static input_srcs (e.g., busybox);
    // it does NOT resolve inputDrvs to their output paths. nix-daemon's
    // sandbox only bind-mounts inputSrcs into the chroot, so without this
    // the builder can't find its input derivations' outputs.
    // Each inputDrv's .drv file is already in rio-store (uploaded by the
    // gateway during SubmitBuild); fetch + parse to get output paths.
    let mut resolved_input_srcs = drv.input_srcs().clone();
    // Collect owned (path, names) pairs up-front so the async closures
    // don't borrow from `drv` (which is not 'static inside spawn_monitored).
    let input_drv_specs: Vec<(String, std::collections::BTreeSet<String>)> = drv
        .input_drvs()
        .iter()
        .map(|(p, n)| (p.clone(), n.clone()))
        .collect();
    let fetched: Vec<Vec<String>> = stream::iter(input_drv_specs)
        .map(|(path, names)| {
            let mut client = store_client.clone();
            async move {
                let input_drv = fetch_drv_from_store(&mut client, &path).await?;
                let matching: Vec<String> = input_drv
                    .outputs()
                    .iter()
                    .filter(|out| names.contains(out.name()))
                    .map(|out| out.path().to_string())
                    .collect();
                Ok::<_, ExecutorError>(matching)
            }
        })
        .buffer_unordered(MAX_PARALLEL_FETCHES)
        .try_collect()
        .await?;
    for paths in fetched {
        resolved_input_srcs.extend(paths);
    }
    let basic_drv = rio_nix::derivation::BasicDerivation::new(
        drv.outputs().to_vec(),
        resolved_input_srcs.clone(),
        drv.platform().to_string(),
        drv.builder().to_string(),
        drv.args().to_vec(),
        drv.env().clone(),
    )
    .map_err(|e| ExecutorError::BuildFailed(format!("failed to build BasicDerivation: {e}")))?;

    // 3. Compute input closure for the synthetic DB (ValidPaths table).
    // Seed with resolved_input_srcs (includes inputDrv outputs, not just
    // static srcs) so nix-daemon's isValidPath() finds dependency outputs.
    // compute_input_closure only seeds from drv.input_srcs() (static), so
    // we merge the resolved set in.
    //
    // Worker computes closure via QueryPathInfo BFS. The scheduler ALSO
    // sends a PrefetchHint (approx_input_closure — DAG children's outputs,
    // bloom-filtered) before the WorkAssignment, so the FUSE cache starts
    // warming while we parse the .drv here. That's a HINT, not a
    // replacement for THIS computation: the synth DB needs the FULL
    // closure (transitive deps + input_srcs), not just the direct
    // dependency outputs the scheduler knows about. The hint gets ahead
    // on the common-case paths; this BFS covers the rest.
    let mut input_paths: Vec<String> =
        compute_input_closure(&*store_client, &drv, drv_path).await?;
    // Add resolved inputDrv outputs (their runtime closure is BFS'd via
    // fetch_input_metadata's references, but they need to be in the seed
    // set first). Dedup via set conversion.
    {
        let mut set: std::collections::HashSet<String> = input_paths.into_iter().collect();
        set.extend(resolved_input_srcs.iter().cloned());
        input_paths = set.into_iter().collect();
    }

    // 4. Fetch input path metadata and generate synthetic DB.
    // CRITICAL: populate DerivationOutputs so nix-daemon's
    // queryPartialDerivationOutputMap(drvPath) returns our output paths.
    // Without it, initialOutputs[out].known is None → nix-daemon builds at
    // makeFallbackPath() (hash of "rewrite:<drvPath>:name:out" + zero hash),
    // but the builder's $out (from BasicDerivation env) is the REAL path →
    // output path mismatch → "builder failed to produce output path".
    let synth_paths = fetch_input_metadata(&*store_client, &input_paths).await?;
    let drv_outputs: Vec<SynthDrvOutput> = drv
        .outputs()
        .iter()
        .map(|o| SynthDrvOutput {
            drv_path: drv_path.clone(),
            output_name: o.name().to_string(),
            output_path: o.path().to_string(),
        })
        .collect();
    let db_dir = overlay::prepare_nix_state_dirs(overlay_mount.upper_dir())?;
    let db_path = db_dir.join("db.sqlite");
    synth_db::generate_db(&db_path, &synth_paths, &drv_outputs).await?;

    // 4. Set up nix.conf in overlay
    setup_nix_conf(overlay_mount.upper_dir())?;

    // 5. Spawn nix-daemon --stdio in a private mount namespace.
    //
    // The daemon sees the overlay bind-mounted at canonical paths:
    //   /nix/store       → overlay merged (FUSE inputs ∪ build outputs)
    //   /nix/var/nix/db  → synthetic SQLite DB (input closure metadata)
    //   /etc/nix         → WORKER_NIX_CONF (sandbox=true, no substituters)
    //
    // This replaces the earlier (broken) NIX_STORE_DIR env var approach:
    // derivations hardcode `/nix/store/...` paths, so the daemon's store
    // root MUST be `/nix/store` — not some overlay subdirectory. The
    // namespace bind-mount achieves that without touching the host's store.
    let timeout = assignment
        .build_options
        .as_ref()
        .and_then(|opts| {
            if opts.build_timeout > 0 {
                Some(Duration::from_secs(opts.build_timeout))
            } else {
                None
            }
        })
        .unwrap_or(env.daemon_timeout);

    tracing::info!(drv_path = %drv_path, "spawning nix-daemon in mount namespace");
    let mut daemon = spawn_daemon_in_namespace(&overlay_mount).await?;
    tracing::info!(drv_path = %drv_path, pid = ?daemon.id(), "nix-daemon spawned; starting handshake");

    // Per-build cgroup. Created AFTER spawn (we need the PID) but
    // BEFORE run_daemon_build — critical ordering: the daemon must
    // be in the cgroup BEFORE it forks the builder (step 4 of
    // run_daemon_build's handshake), otherwise the builder inherits
    // the PARENT cgroup and we measure only daemon RSS — right back
    // to the phase2c VmHWM bug. The handshake hasn't started yet
    // (run_daemon_build does it), so this is safe.
    //
    // `?` on both create and add_process: if cgroup setup fails here,
    // the build fails. cgroup v2 is a hard requirement — we already
    // validated the parent cgroup at startup, so failure here is
    // exceptional (stale directory from a crash that we couldn't
    // rmdir because it has a stuck process, or daemon died between
    // spawn and now). Both are real errors the operator should see.
    //
    // build_id = sanitize_build_id(drv_path). nixbase32 hash chars
    // are valid cgroup names; sanitize replaces '.' (from ".drv")
    // with '_'. Same name as the overlay directory — easy to
    // correlate in debugging.
    let build_cgroup = crate::cgroup::BuildCgroup::create(&env.cgroup_parent, &build_id)
        .map_err(|e| ExecutorError::Cgroup(format!("create sub-cgroup: {e}")))?;
    let daemon_pid = daemon
        .id()
        .ok_or_else(|| ExecutorError::Cgroup("daemon PID unavailable (died at spawn?)".into()))?;
    build_cgroup
        .add_process(daemon_pid)
        .map_err(|e| ExecutorError::Cgroup(format!("add daemon to cgroup: {e}")))?;

    // CPU polling task. Runs concurrently with run_daemon_build
    // below (which awaits). Samples cpu.stat usage_usec every second,
    // computes instantaneous cores = delta_usec/elapsed_usec, tracks
    // max. The cgroup's usage_usec is tree-cumulative, so this
    // captures the builder's CPU too.
    //
    // Stores max as f64 bits in an AtomicU64 — there's no AtomicF64.
    // compare_exchange loop for max (fetch_max on u64 bits would
    // compare BIT PATTERNS, not float values — 2.0_f64.to_bits() >
    // 8.0_f64.to_bits() is NOT guaranteed). Standard f64-atomic
    // pattern.
    //
    // Clone the cgroup PATH (not the BuildCgroup — moving it would
    // put Drop in the task, which we don't want; Drop must run after
    // daemon.wait() below).
    let peak_cpu_atomic = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let cpu_poll_path = build_cgroup.path().to_path_buf();
    let cpu_poll_peak = peak_cpu_atomic.clone();
    let cpu_poll = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        // First tick fires immediately — skip it, we want a 1s baseline.
        interval.tick().await;
        let mut prev_usec = read_cpu_stat(&cpu_poll_path);
        let mut prev_instant = std::time::Instant::now();
        loop {
            interval.tick().await;
            let now_usec = read_cpu_stat(&cpu_poll_path);
            let now_instant = std::time::Instant::now();
            // Both samples must be Some. If the first read failed
            // (cgroup not populated yet — daemon hasn't forked),
            // prev is None and we just advance. If THIS read fails
            // (cgroup removed? shouldn't happen until Drop), skip.
            if let (Some(prev), Some(now)) = (prev_usec, now_usec) {
                let delta_usec = now.saturating_sub(prev);
                let elapsed_usec = now_instant.duration_since(prev_instant).as_micros() as u64;
                // elapsed_usec is ~1_000_000 (1s interval) but
                // jitters. Guard /0 for the impossible case where
                // two ticks fire at the same instant.
                if elapsed_usec > 0 {
                    let cores = delta_usec as f64 / elapsed_usec as f64;
                    // Compare-exchange max: load, if cores > current,
                    // try to swap. Loop until success or current >= cores.
                    let mut current_bits = cpu_poll_peak.load(Ordering::Relaxed);
                    loop {
                        if f64::from_bits(current_bits) >= cores {
                            break; // already higher, done
                        }
                        match cpu_poll_peak.compare_exchange_weak(
                            current_bits,
                            cores.to_bits(),
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => break,                       // we set it
                            Err(actual) => current_bits = actual, // raced, retry
                        }
                    }
                }
            }
            prev_usec = now_usec;
            prev_instant = now_instant;
        }
    });

    // All daemon I/O is in a helper so we can ALWAYS kill on error.
    // The cgroup setup above (create/add_process, added in phase3a)
    // is NOT inside this helper — its `?` paths rely on the
    // kill_on_drop set in spawn_daemon_in_namespace as a safety
    // net. The explicit kill below remains the primary cleanup
    // (graceful, bounded wait for reap); kill_on_drop covers early
    // returns between spawn and here.
    let batcher = LogBatcher::new(drv_path.clone(), worker_id.to_string(), log_limits);
    let build_result =
        run_daemon_build(&mut daemon, drv_path, &basic_drv, timeout, batcher, log_tx).await;

    // Stop CPU polling. The last sample is up to 1s stale; good
    // enough (peak CPU doesn't change in the last second of a
    // multi-minute build). abort() doesn't wait — the task is
    // pure read, no cleanup needed.
    cpu_poll.abort();
    let peak_cpu_cores = f64::from_bits(peak_cpu_atomic.load(Ordering::Acquire));

    // Read cgroup memory.peak. Kernel-tracked lifetime max of the
    // WHOLE TREE — daemon + builder + every child. One read, no
    // polling. This FIXES the phase2c bug: VmHWM on daemon.id()
    // measured ~10MB (daemon's own RSS) because the builder was a
    // FORKED child, not exec'd — the builder's memory never showed
    // in daemon's /proc.
    //
    // 0 on None (file missing would mean memory controller not
    // enabled, but enable_subtree_controllers at startup would have
    // caught that — this is a belt-and-suspenders default).
    let peak_memory_bytes = build_cgroup.memory_peak().unwrap_or(0);

    // ALWAYS kill the daemon, regardless of success/failure.
    if let Err(e) = daemon.kill().await {
        tracing::warn!(error = %e, "daemon.kill() failed (process may already be dead)");
    }
    // Reap the zombie (bounded wait). After this, cgroup is empty
    // and build_cgroup Drop can rmdir cleanly.
    match tokio::time::timeout(Duration::from_secs(2), daemon.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "daemon.wait() failed after kill"),
        Err(_) => tracing::warn!("daemon did not exit within 2s after kill (possible zombie)"),
    }
    // build_cgroup drops here (end of scope). rmdir should succeed
    // now that daemon.wait() reaped the tree. If the wait timed out
    // (zombie), rmdir fails EBUSY → warn! + leak (cleared on pod
    // restart).
    drop(build_cgroup);

    // (Final log flush happens inside read_build_stderr_loop, which now
    // owns the batcher.)

    // NOW propagate any daemon error (after kill).
    let build_result = build_result?;

    // 10. Check build result. output_size_bytes tracked alongside:
    // sum of uploaded NAR sizes (only set on successful upload path).
    let mut output_size_bytes = 0u64;
    let proto_result = if build_result.status.is_success() {
        tracing::info!(drv_path = %drv_path, "build succeeded, uploading outputs");

        // Upload outputs
        match upload::upload_all_outputs(
            store_client,
            overlay_mount.upper_dir(),
            // Pass the assignment token as gRPC metadata on each
            // PutPath. Store with hmac_verifier checks it. Empty
            // token (scheduler without hmac_signer, dev mode) →
            // no header → store with verifier=None accepts.
            &assignment.assignment_token,
        )
        .await
        {
            Ok(upload_results) => {
                // FOD defense-in-depth: verify output hashes match declared outputHash.
                // nix-daemon already verifies, but we re-check before accepting.
                if assignment.is_fixed_output
                    && let Err(e) =
                        verify_fod_hashes(&drv, &upload_results, overlay_mount.upper_dir())
                {
                    tracing::error!(
                        drv_path = %drv_path,
                        error = %e,
                        "FOD output hash verification failed"
                    );
                    return Ok(ExecutionResult {
                        drv_path: drv_path.clone(),
                        result: ProtoBuildResult {
                            status: BuildResultStatus::OutputRejected.into(),
                            error_msg: format!("FOD output hash verification failed: {e}"),
                            ..Default::default()
                        },
                        assignment_token: assignment.assignment_token.clone(),
                        // Build DID run (FOD verification is post-build).
                        // cgroup was populated → memory+cpu are meaningful
                        // even though we reject the output.
                        peak_memory_bytes,
                        output_size_bytes: 0,
                        peak_cpu_cores,
                    });
                }

                // Sum NAR sizes for the CompletionReport. Feeds
                // build_history.ema_output_size_bytes — not used for
                // routing yet but useful for dashboards / capacity.
                output_size_bytes = upload_results.iter().map(|r| r.nar_size).sum();

                // Map store_path → output_name from the derivation. Upload
                // results are unordered (buffer_unordered), and even the
                // prior sequential scan had undefined order (read_dir).
                let path_to_name: HashMap<&str, &str> =
                    drv.outputs().iter().map(|o| (o.path(), o.name())).collect();
                let built_outputs: Vec<BuiltOutput> = upload_results
                    .iter()
                    .map(|result| BuiltOutput {
                        output_name: path_to_name
                            .get(result.store_path.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| {
                                tracing::warn!(
                                    store_path = %result.store_path,
                                    "uploaded path not in derivation outputs; using basename"
                                );
                                result
                                    .store_path
                                    .rsplit('/')
                                    .next()
                                    .unwrap_or(&result.store_path)
                                    .to_string()
                            }),
                        output_path: result.store_path.clone(),
                        output_hash: result.nar_hash.to_vec(),
                    })
                    .collect();

                // start/stop_time: nix-daemon's BuildResult already has
                // these as Unix epoch seconds (rio-nix/build.rs:118-120).
                // Scheduler guards update_build_history on BOTH being
                // Some (completion.rs:182) — without them, EMA duration
                // can't be computed and the WHOLE build_history write
                // is skipped (peak_memory_bytes included). Never caught
                // before: phase2c.nix pre-seeds via psql INSERT.
                //
                // 0 → None: nix-daemon sends 0 on some error paths.
                // A real build at 1970-01-01 doesn't exist.
                let to_proto_ts = |secs: u64| {
                    (secs > 0).then_some(prost_types::Timestamp {
                        seconds: secs as i64,
                        nanos: 0,
                    })
                };
                ProtoBuildResult {
                    status: BuildResultStatus::Built.into(),
                    error_msg: String::new(),
                    times_built: build_result.times_built,
                    start_time: to_proto_ts(build_result.start_time),
                    stop_time: to_proto_ts(build_result.stop_time),
                    built_outputs,
                }
            }
            Err(e) => {
                tracing::error!(drv_path = %drv_path, error = %e, "output upload failed");
                ProtoBuildResult {
                    status: BuildResultStatus::InfrastructureFailure.into(),
                    error_msg: format!("output upload failed: {e}"),
                    ..Default::default()
                }
            }
        }
    } else {
        tracing::warn!(
            drv_path = %drv_path,
            status = ?build_result.status,
            error = %build_result.error_msg,
            "build failed"
        );

        let status = match build_result.status {
            rio_nix::protocol::build::BuildStatus::PermanentFailure => {
                BuildResultStatus::PermanentFailure
            }
            rio_nix::protocol::build::BuildStatus::TransientFailure => {
                BuildResultStatus::TransientFailure
            }
            _ => BuildResultStatus::PermanentFailure,
        };

        ProtoBuildResult {
            status: status.into(),
            error_msg: build_result.error_msg.clone(),
            ..Default::default()
        }
    };

    // 11. Tear down overlay (explicit, before Drop).
    // The leak-threshold check happens at ENTRY (above), not here: we don't
    // override a successful build result just because its own teardown fails.
    // Teardown failure increments the counter (in OverlayMount::Drop); the
    // NEXT build is what gets refused. The metric is also incremented in
    // Drop (centralized so ?-early-returns and panics also count).
    let merged_path = overlay_mount.merged_dir().to_path_buf();
    if let Err(e) = overlay::teardown_overlay(overlay_mount) {
        tracing::error!(
            error = %e,
            merged = %merged_path.display(),
            "overlay teardown failed; mount leaked"
        );
        // Metric incremented in Drop (see overlay.rs).
    }

    Ok(ExecutionResult {
        drv_path: drv_path.clone(),
        result: proto_result,
        assignment_token: assignment.assignment_token.clone(),
        peak_memory_bytes,
        output_size_bytes,
        peak_cpu_cores,
    })
}

/// Write nix.conf to the overlay upper layer.
fn setup_nix_conf(upper_dir: &Path) -> Result<(), ExecutorError> {
    let conf_dir = upper_dir.join("etc/nix");
    std::fs::create_dir_all(&conf_dir).map_err(ExecutorError::NixConf)?;
    std::fs::write(conf_dir.join("nix.conf"), WORKER_NIX_CONF).map_err(ExecutorError::NixConf)?;
    Ok(())
}

/// Convert a derivation path to a safe build ID for directory names.
///
/// Public so `spawn_build_task` can predict the cgroup path for the
/// cancel registry (cgroup_parent/sanitize_build_id(drv_path)) without
/// execute_build having to report it back. The cgroup is created
/// DURING execute_build (after daemon spawn, needs PID), so spawn_
/// build_task registers the path PREDICTIVELY before spawning and
/// removes it after. If a cancel arrives before the cgroup exists,
/// cgroup.kill returns ENOENT — try_cancel_build logs and moves on
/// (the build will fail anyway since the daemon dies when the cgroup
/// IS created with a stale kill file — no, cgroup.kill isn't a
/// persistent file, it's a write-once trigger. ENOENT just means no
/// kill happened, the build proceeds. Harmless race — a cancel
/// arriving THAT early is extremely rare and the scheduler will
/// re-send on the next dispatch cycle if the build keeps running).
pub fn sanitize_build_id(drv_path: &str) -> String {
    // /nix/store/abc...-foo.drv -> abc...-foo.drv
    drv_path
        .rsplit('/')
        .next()
        .unwrap_or(drv_path)
        .replace('.', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_build_id() {
        assert_eq!(
            sanitize_build_id("/nix/store/abc123-hello.drv"),
            "abc123-hello_drv"
        );
        assert_eq!(sanitize_build_id("simple"), "simple");
        assert_eq!(sanitize_build_id("foo.bar.drv"), "foo_bar_drv");
    }

    #[test]
    fn test_worker_nix_conf_content() {
        assert!(WORKER_NIX_CONF.contains("sandbox = true"));
        assert!(WORKER_NIX_CONF.contains("substitute = false"));
        assert!(WORKER_NIX_CONF.contains("builders ="));
        assert!(WORKER_NIX_CONF.contains("sandbox-fallback = false"));
    }

    #[test]
    fn test_setup_nix_conf() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        setup_nix_conf(dir.path())?;

        let conf_path = dir.path().join("etc/nix/nix.conf");
        assert!(conf_path.exists());
        let content = std::fs::read_to_string(&conf_path)?;
        assert!(content.contains("sandbox = true"));
        Ok(())
    }

    /// When the leak counter is at or over threshold, execute_build must
    /// short-circuit with InfrastructureFailure BEFORE touching the overlay.
    /// This test does NOT require CAP_SYS_ADMIN: it sets the counter over
    /// the threshold and asserts the short-circuit path, which runs before
    /// setup_overlay is ever called.
    #[tokio::test]
    async fn test_execute_build_refuses_when_leaked_exceeds_threshold() -> anyhow::Result<()> {
        // Set well over any plausible threshold (default is 3).
        let leak_counter = Arc::new(AtomicUsize::new(999));

        let assignment = WorkAssignment {
            drv_path: rio_test_support::fixtures::test_drv_path("test"),
            assignment_token: "token-123".into(),
            ..Default::default()
        };

        // store_client: execute_build short-circuits before any gRPC call, so
        // we pass a client pointed at a garbage endpoint. connect_lazy() does
        // not dial until first use.
        let channel = Channel::from_static("http://127.0.0.1:1").connect_lazy();
        let mut store_client = StoreServiceClient::new(channel);

        let (log_tx, _log_rx) = mpsc::channel(1);
        let dir = tempfile::tempdir()?;

        let env = ExecutorEnv {
            fuse_mount_point: dir.path().to_path_buf(),
            overlay_base_dir: dir.path().to_path_buf(),
            worker_id: "test-worker".into(),
            log_limits: LogLimits::UNLIMITED,
            max_leaked_mounts: 3,
            daemon_timeout: DEFAULT_DAEMON_TIMEOUT,
            // Short-circuit path never reaches cgroup setup (bails at
            // the leak-threshold check). Tempdir is fine.
            cgroup_parent: dir.path().to_path_buf(),
        };
        let result =
            execute_build(&assignment, &env, &mut store_client, &log_tx, &leak_counter).await;

        // Must be Ok(ExecutionResult{InfrastructureFailure}), NOT Err —
        // Ok ensures CompletionReport is sent so the scheduler reassigns.
        let exec = result.expect("short-circuit path returns Ok, not Err");
        assert_eq!(exec.drv_path, assignment.drv_path);
        assert_eq!(exec.assignment_token, "token-123");
        assert_eq!(
            exec.result.status,
            BuildResultStatus::InfrastructureFailure as i32,
            "should report InfrastructureFailure"
        );
        assert!(
            exec.result.error_msg.contains("leaked overlay mount"),
            "error message should mention leaked mounts, got: {}",
            exec.result.error_msg
        );
        assert!(
            exec.result.error_msg.contains("999"),
            "error message should include the leak count, got: {}",
            exec.result.error_msg
        );

        // Counter unchanged — short-circuit doesn't touch the overlay.
        assert_eq!(leak_counter.load(Ordering::Relaxed), 999);
        Ok(())
    }

    // read_vmhwm_bytes tests removed — function replaced by cgroup
    // memory.peak (I2). The equivalent canary (real-system parse)
    // is cgroup::tests::own_cgroup_parses_on_this_system.
}
