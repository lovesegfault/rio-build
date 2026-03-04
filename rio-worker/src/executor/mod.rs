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

use futures_util::stream::{self, StreamExt, TryStreamExt};
use rio_nix::derivation::Derivation;
use rio_proto::store::store_service_client::StoreServiceClient;
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
}

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
    /// Peak memory (RSS) in bytes, sampled from VmHWM before daemon
    /// kill. 0 = couldn't read (proc gone, build failed early).
    pub peak_memory_bytes: u64,
    /// Total bytes uploaded (sum of NAR sizes). 0 on failure.
    pub output_size_bytes: u64,
}

/// Read VmHWM from /proc/{pid}/status. The kernel tracks process-
/// lifetime peak RSS here — no polling needed, one read gives the max.
///
/// Returns `None` on any failure (file missing = process dead, parse
/// fail = kernel format changed). Caller treats None as "no signal"
/// and reports 0 to the scheduler.
///
/// Format: `VmHWM:    123456 kB` (whitespace-separated, always kB).
/// We parse the number and convert kB → bytes. The "always kB" is a
/// Linux kernel guarantee (fs/proc/task_mmu.c) — no unit parsing needed.
fn read_vmhwm_bytes(pid: u32) -> Option<u64> {
    let path = format!("/proc/{pid}/status");
    let content = std::fs::read_to_string(&path).ok()?;

    // Find the VmHWM line. Lines look like "VmHWM:\t   12345 kB".
    // split_whitespace handles any mix of tabs/spaces.
    content
        .lines()
        .find(|line| line.starts_with("VmHWM:"))
        .and_then(|line| {
            // ["VmHWM:", "12345", "kB"] — we want index 1.
            line.split_whitespace().nth(1)
        })
        .and_then(|kb_str| kb_str.parse::<u64>().ok())
        // kB → bytes. The kernel ALWAYS reports in kB for VmHWM.
        .map(|kb| kb * 1024)
}

/// Execute a single build assignment.
///
/// This is the main entry point for building a derivation. It handles
/// the full lifecycle: overlay setup, synthetic DB, daemon invocation,
/// log streaming, output upload, and cleanup.
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
        input_count = assignment.input_paths.len(),
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
            // Infra failure before daemon spawn = no memory, no outputs.
            peak_memory_bytes: 0,
            output_size_bytes: 0,
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

    // 2. Parse the derivation. If drv_content is inline, use it; otherwise
    // fetch the .drv from the store and extract ATerm from the NAR.
    // Phase 2a: scheduler sends drv_content=empty, so we always fetch.
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
    let mut input_paths: Vec<String> = if assignment.input_paths.is_empty() {
        compute_input_closure(&*store_client, &drv, drv_path).await?
    } else {
        assignment.input_paths.clone()
    };
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
        .unwrap_or_else(rio_common::grpc::daemon_timeout);

    tracing::info!(drv_path = %drv_path, "spawning nix-daemon in mount namespace");
    let mut daemon = spawn_daemon_in_namespace(&overlay_mount).await?;
    tracing::info!(drv_path = %drv_path, pid = ?daemon.id(), "nix-daemon spawned; starting handshake");

    // All daemon I/O is in a helper so we can ALWAYS kill on error.
    // Previously, any `?` between spawn and kill leaked the daemon process.
    let batcher = LogBatcher::new(drv_path.clone(), worker_id.to_string(), log_limits);
    let build_result =
        run_daemon_build(&mut daemon, drv_path, &basic_drv, timeout, batcher, log_tx).await;

    // Sample VmHWM (peak RSS) BEFORE killing — /proc/{pid} goes away
    // on exit. The kernel tracks VmHWM for us (no polling needed);
    // one read at build-end gives the lifetime peak. This feeds
    // build_history.ema_peak_memory_bytes for D7's size-class routing.
    //
    // 0 on any failure (pid None = already dead, file gone, parse fail).
    // Scheduler treats 0 as "no signal" — doesn't drag the EMA.
    let peak_memory_bytes = daemon.id().and_then(read_vmhwm_bytes).unwrap_or(0);

    // ALWAYS kill the daemon, regardless of success/failure.
    if let Err(e) = daemon.kill().await {
        tracing::warn!(error = %e, "daemon.kill() failed (process may already be dead)");
    }
    // Reap the zombie (bounded wait).
    match tokio::time::timeout(Duration::from_secs(2), daemon.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "daemon.wait() failed after kill"),
        Err(_) => tracing::warn!("daemon did not exit within 2s after kill (possible zombie)"),
    }

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
        match upload::upload_all_outputs(store_client, overlay_mount.upper_dir()).await {
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
                        // Build DID run (FOD verification is post-build),
                        // so VmHWM is meaningful even though we reject.
                        peak_memory_bytes,
                        output_size_bytes: 0,
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

                ProtoBuildResult {
                    status: BuildResultStatus::Built.into(),
                    error_msg: String::new(),
                    times_built: build_result.times_built,
                    start_time: None,
                    stop_time: None,
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
fn sanitize_build_id(drv_path: &str) -> String {
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
            drv_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-test.drv".into(),
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

    /// VmHWM parse against a real /proc entry: this process. The test
    /// process itself has a nonzero RSS, so VmHWM must be > 0. This
    /// exercises the whole path (file read, line scan, whitespace
    /// split, kB→bytes) without needing to spawn a daemon.
    #[test]
    fn test_read_vmhwm_bytes_self() {
        let pid = std::process::id();
        let hwm = read_vmhwm_bytes(pid).expect("/proc/self/status should exist and parse");

        // The test binary's peak RSS is at LEAST a few hundred KB
        // (stack + heap + mapped libs). 64 KiB is a safe floor; a
        // real value is typically tens of MB. This guards against
        // silently parsing the wrong field or dropping the ×1024.
        assert!(
            hwm > 64 * 1024,
            "VmHWM for self should be well over 64 KiB, got {hwm} bytes"
        );

        // Kernel reports in whole kB, so bytes MUST be a multiple of
        // 1024. Catches "forgot to ×1024" and "parsed wrong number"
        // classes of bug.
        assert_eq!(hwm % 1024, 0, "VmHWM should be kB-aligned, got {hwm}");
    }

    /// Nonexistent PID → None (not panic, not Err). PID 1 billion is
    /// way past pid_max (default ~4M on Linux), so guaranteed absent.
    #[test]
    fn test_read_vmhwm_bytes_nonexistent_pid() {
        assert_eq!(read_vmhwm_bytes(1_000_000_000), None);
    }
}
