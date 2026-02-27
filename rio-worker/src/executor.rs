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
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::mpsc;
use tonic::transport::Channel;

use rio_nix::derivation::Derivation;
use rio_nix::protocol::build::BuildMode;
use rio_nix::protocol::client::{
    StderrMessage, client_handshake, client_set_options, read_stderr_message,
};
use rio_nix::protocol::wire;
use rio_proto::store::store_service_client::StoreServiceClient;
use rio_proto::types::{
    BuildResult as ProtoBuildResult, BuildResultStatus, BuiltOutput, GetPathRequest,
    QueryPathInfoRequest, WorkAssignment, WorkerMessage, get_path_response, worker_message,
};

use crate::log_stream::LogBatcher;
use crate::overlay;
use crate::synth_db::{self, SynthPathInfo, path_info_to_synth};
use crate::upload;

/// Timeout for the daemon setup sequence (handshake + setOptions + send build).
/// This bounds the blast radius of a stuck daemon before the build timeout kicks in.
const DAEMON_SETUP_TIMEOUT: Duration = Duration::from_secs(30);

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
#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error("overlay setup failed: {0}")]
    Overlay(#[from] anyhow::Error),
    #[error("synthetic DB generation failed: {0}")]
    SynthDb(String),
    #[error("nix.conf setup failed: {0}")]
    NixConf(String),
    #[error("daemon spawn failed: {0}")]
    DaemonSpawn(std::io::Error),
    #[error("daemon handshake failed: {0}")]
    DaemonHandshake(String),
    #[error("build failed: {0}")]
    BuildFailed(String),
    #[error("upload failed: {0}")]
    Upload(#[from] upload::UploadError),
    #[error("gRPC error: {0}")]
    Grpc(#[from] tonic::Status),
    #[error("input metadata fetch failed for {path}: {source}")]
    MetadataFetch { path: String, source: tonic::Status },
    #[error("wire protocol error: {0}")]
    Wire(String),
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
}

/// Execute a single build assignment.
///
/// This is the main entry point for building a derivation. It handles
/// the full lifecycle: overlay setup, synthetic DB, daemon invocation,
/// log streaming, output upload, and cleanup.
#[allow(clippy::too_many_arguments)]
pub async fn execute_build(
    assignment: &WorkAssignment,
    fuse_mount_point: &Path,
    overlay_base_dir: &Path,
    store_client: &mut StoreServiceClient<Channel>,
    worker_id: &str,
    log_tx: &mpsc::Sender<WorkerMessage>,
) -> Result<ExecutionResult, ExecutorError> {
    let drv_path = &assignment.drv_path;
    let build_id = sanitize_build_id(drv_path);

    tracing::info!(
        drv_path = %drv_path,
        build_id = %build_id,
        input_count = assignment.input_paths.len(),
        is_fod = assignment.is_fixed_output,
        "starting build"
    );

    metrics::gauge!("rio_worker_builds_active").increment(1.0);
    // rio_worker_builds_total is incremented at completion (main.rs) with
    // an outcome label so SLI queries can compute success rate.
    let build_start = std::time::Instant::now();
    let _build_guard = scopeguard::guard((), move |()| {
        metrics::gauge!("rio_worker_builds_active").decrement(1.0);
        metrics::histogram!("rio_worker_build_duration_seconds")
            .record(build_start.elapsed().as_secs_f64());
    });

    // 1. Set up overlay
    let overlay_mount = overlay::setup_overlay(fuse_mount_point, overlay_base_dir, &build_id)?;

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
    let basic_drv = drv.to_basic();

    // 3. Compute input closure. If scheduler sent input_paths, trust it;
    // otherwise compute from the parsed derivation + store references.
    // Phase 2a: scheduler sends input_paths=empty, so we always compute.
    let input_paths: Vec<String> = if assignment.input_paths.is_empty() {
        compute_input_closure(store_client, &drv, drv_path).await?
    } else {
        assignment.input_paths.clone()
    };

    // 4. Fetch input path metadata and generate synthetic DB
    let synth_paths = fetch_input_metadata(store_client, &input_paths).await?;
    let db_dir = overlay::prepare_nix_state_dirs(overlay_mount.upper_dir())?;
    let db_path = db_dir.join("db.sqlite");
    synth_db::generate_db(&db_path, &synth_paths)
        .await
        .map_err(|e| ExecutorError::SynthDb(e.to_string()))?;

    // 4. Set up nix.conf in overlay
    setup_nix_conf(overlay_mount.upper_dir())?;

    // 5. Spawn nix-daemon --stdio
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

    let mut daemon = Command::new("nix-daemon")
        .arg("--stdio")
        .env(
            "NIX_STORE_DIR",
            overlay_mount.merged_dir().join("nix/store"),
        )
        .env(
            "NIX_STATE_DIR",
            overlay_mount.merged_dir().join("nix/var/nix"),
        )
        .env("NIX_CONF_DIR", overlay_mount.upper_dir().join("etc/nix"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // Inherit stderr: daemon diagnostics go to worker's stderr (visible in
        // container logs). Piping without reading would deadlock if nix-daemon
        // writes >64KB to stderr (pipe buffer full, daemon blocks on write).
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(ExecutorError::DaemonSpawn)?;

    // All daemon I/O is in a helper so we can ALWAYS kill on error.
    // Previously, any `?` between spawn and kill leaked the daemon process.
    let mut batcher = LogBatcher::new(drv_path.clone(), worker_id.to_string());
    let build_result = run_daemon_build(
        &mut daemon,
        drv_path,
        &basic_drv,
        timeout,
        &mut batcher,
        log_tx,
    )
    .await;

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

    // Flush any remaining log lines (best-effort: build result is already determined)
    if batcher.has_pending() {
        let batch = batcher.flush();
        let msg = WorkerMessage {
            msg: Some(worker_message::Msg::LogBatch(batch)),
        };
        if log_tx.send(msg).await.is_err() {
            tracing::warn!("log channel closed during final flush");
        }
    }

    // NOW propagate any daemon error (after kill).
    let build_result = build_result?;

    // 10. Check build result
    let proto_result = if build_result.status().is_success() {
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
                    });
                }

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
                        output_hash: result.nar_hash.clone(),
                    })
                    .collect();

                ProtoBuildResult {
                    status: BuildResultStatus::Built.into(),
                    error_msg: String::new(),
                    times_built: build_result.times_built(),
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
            status = ?build_result.status(),
            error = %build_result.error_msg(),
            "build failed"
        );

        let status = match build_result.status() {
            rio_nix::protocol::build::BuildStatus::PermanentFailure => {
                BuildResultStatus::PermanentFailure
            }
            rio_nix::protocol::build::BuildStatus::TransientFailure => {
                BuildResultStatus::TransientFailure
            }
            rio_nix::protocol::build::BuildStatus::TimedOut => BuildResultStatus::PermanentFailure,
            _ => BuildResultStatus::PermanentFailure,
        };

        ProtoBuildResult {
            status: status.into(),
            error_msg: build_result.error_msg().to_string(),
            ..Default::default()
        }
    };

    // 11. Tear down overlay (explicit, before Drop).
    // TODO(phase2b): leaked mounts should cause infrastructure failure once
    // mount tracking is added. For now, Drop is the safety net; the build
    // result is already determined (success or failure), so we log teardown
    // failure but don't override the result. The metric is incremented in
    // OverlayMount::Drop (centralized so ?-early-returns and panics also count).
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
    })
}

/// All daemon I/O after spawn: handshake, setOptions, wopBuildDerivation, stderr loop.
///
/// Caller MUST kill the daemon after this returns (whether Ok or Err).
/// This is the only function that should touch daemon stdin/stdout —
/// keeping it isolated ensures the caller's always-kill path is reliable.
async fn run_daemon_build(
    daemon: &mut tokio::process::Child,
    drv_path: &str,
    basic_drv: &rio_nix::derivation::BasicDerivation,
    build_timeout: Duration,
    batcher: &mut LogBatcher,
    log_tx: &mpsc::Sender<WorkerMessage>,
) -> Result<rio_nix::protocol::build::BuildResult, ExecutorError> {
    let mut stdin = daemon
        .stdin
        .take()
        .ok_or_else(|| ExecutorError::DaemonHandshake("failed to get daemon stdin".into()))?;
    let mut stdout = daemon
        .stdout
        .take()
        .ok_or_else(|| ExecutorError::DaemonHandshake("failed to get daemon stdout".into()))?;

    // Handshake + setOptions + send build — all bounded by DAEMON_SETUP_TIMEOUT.
    // Previously only the handshake was timed out; a stuck setOptions or
    // a stalled write would hang until build_timeout (potentially hours).
    tokio::time::timeout(DAEMON_SETUP_TIMEOUT, async {
        let handshake_result = client_handshake(&mut stdout, &mut stdin)
            .await
            .map_err(|e| ExecutorError::DaemonHandshake(format!("{e}")))?;

        tracing::debug!(
            version = handshake_result.negotiated_version(),
            "daemon handshake complete"
        );

        client_set_options(&mut stdout, &mut stdin)
            .await
            .map_err(|e| ExecutorError::DaemonHandshake(format!("setOptions failed: {e}")))?;

        wire::write_u64(
            &mut stdin,
            rio_nix::protocol::opcodes::WorkerOp::BuildDerivation as u64,
        )
        .await
        .map_err(|e| ExecutorError::Wire(format!("{e}")))?;
        wire::write_string(&mut stdin, drv_path)
            .await
            .map_err(|e| ExecutorError::Wire(format!("{e}")))?;
        rio_nix::protocol::build::write_basic_derivation(&mut stdin, basic_drv)
            .await
            .map_err(|e| ExecutorError::Wire(format!("{e}")))?;
        wire::write_u64(&mut stdin, BuildMode::Normal as u64)
            .await
            .map_err(|e| ExecutorError::Wire(format!("{e}")))?;
        tokio::io::AsyncWriteExt::flush(&mut stdin)
            .await
            .map_err(|e| ExecutorError::Wire(format!("{e}")))?;

        Ok::<_, ExecutorError>(())
    })
    .await
    .map_err(|_| ExecutorError::DaemonHandshake("daemon setup sequence timed out".into()))??;

    // Read STDERR loop with log streaming (build may run for a long time)
    let build_result = tokio::time::timeout(build_timeout, async {
        read_build_stderr_loop(&mut stdout, batcher, log_tx).await
    })
    .await
    .map_err(|_| ExecutorError::BuildFailed("build timed out".into()))?
    .map_err(|e| ExecutorError::BuildFailed(format!("{e}")))?;

    Ok(build_result)
}

/// Read the STDERR loop from the daemon, streaming logs via the batcher.
///
/// If the log channel closes during the build, returns an InfrastructureFailure —
/// the scheduler stream is gone, so there's no way to report completion anyway.
async fn read_build_stderr_loop<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    batcher: &mut LogBatcher,
    log_tx: &mpsc::Sender<WorkerMessage>,
) -> Result<rio_nix::protocol::build::BuildResult, rio_nix::protocol::wire::WireError> {
    const MAX_BUILD_STDERR_MESSAGES: u64 = 10_000_000;
    let mut msg_count: u64 = 0;

    /// Helper: send a log batch. Returns false if the channel is closed.
    async fn send_batch(
        log_tx: &mpsc::Sender<WorkerMessage>,
        batch: rio_proto::types::BuildLogBatch,
    ) -> bool {
        let msg = WorkerMessage {
            msg: Some(worker_message::Msg::LogBatch(batch)),
        };
        log_tx.send(msg).await.is_ok()
    }

    loop {
        if msg_count >= MAX_BUILD_STDERR_MESSAGES {
            return Err(rio_nix::protocol::wire::WireError::Io(
                std::io::Error::other("exceeded maximum STDERR messages during build"),
            ));
        }
        msg_count += 1;

        // Check for timeout-based flush.
        //
        // TODO(phase2b): honor the 100ms BATCH_TIMEOUT during silent periods.
        // Currently maybe_flush() only fires once per stderr message, so a
        // build that's silent for 60s (common: long compile that buffers
        // stdout) won't flush its partial batch until the next STDERR_NEXT
        // or STDERR_LAST arrives. The observability spec's "64 lines / 100ms"
        // guarantee is NOT upheld during quiet periods.
        //
        // The obvious fix (tokio::time::timeout around read_stderr_message)
        // is UNSAFE: dropping the read future mid-u64-read leaves partial
        // bytes consumed from the daemon stdout pipe; the next read desyncs
        // the Nix STDERR protocol. Safe fixes:
        //   (a) Spawn read_stderr_message into an owned task that pushes to
        //       a mpsc channel (cancel-safe); select! on rx.recv() + interval.
        //   (b) Fused-future pattern: hold the pinned read future across
        //       select! iterations, only recreate on completion.
        // Both require reworking the &mut R borrow. Impact is user-visible
        // log latency (build appears hung), not correctness.
        if let Some(batch) = batcher.maybe_flush()
            && !send_batch(log_tx, batch).await
        {
            return Ok(rio_nix::protocol::build::BuildResult::failure(
                rio_nix::protocol::build::BuildStatus::MiscFailure,
                "log channel closed during build (scheduler stream gone)".to_string(),
            ));
        }

        match read_stderr_message(reader).await? {
            StderrMessage::Last => break,
            StderrMessage::Error(e) => {
                return Ok(rio_nix::protocol::build::BuildResult::failure(
                    rio_nix::protocol::build::BuildStatus::MiscFailure,
                    e.message().to_string(),
                ));
            }
            StderrMessage::Next(msg) => {
                if let Some(batch) = batcher.add_line(msg.into_bytes())
                    && !send_batch(log_tx, batch).await
                {
                    return Ok(rio_nix::protocol::build::BuildResult::failure(
                        rio_nix::protocol::build::BuildStatus::MiscFailure,
                        "log channel closed during build (scheduler stream gone)".to_string(),
                    ));
                }
            }
            StderrMessage::Read(_) => {
                return Ok(rio_nix::protocol::build::BuildResult::failure(
                    rio_nix::protocol::build::BuildStatus::MiscFailure,
                    "daemon sent STDERR_READ, not supported".to_string(),
                ));
            }
            // Activity/progress messages we explicitly don't care about.
            StderrMessage::Write(_)
            | StderrMessage::StartActivity { .. }
            | StderrMessage::StopActivity { .. }
            | StderrMessage::Result { .. } => {}
        }
    }

    // Read BuildResult
    rio_nix::protocol::build::read_build_result(reader).await
}

/// Verify FOD output hashes match the declared outputHash (defense-in-depth;
/// nix-daemon also verifies, but we re-check before accepting).
///
/// For `r:sha256` (recursive): compare the upload's NAR hash against outputHash.
/// For `sha256` (flat): read the file from the overlay upper layer, hash its
/// contents directly, and compare.
pub(crate) fn verify_fod_hashes(
    drv: &Derivation,
    uploads: &[upload::UploadResult],
    overlay_upper: &Path,
) -> anyhow::Result<()> {
    use anyhow::{Context, bail};
    use sha2::{Digest, Sha256};

    for output in drv.outputs() {
        // Only FOD outputs have a declared hash
        if output.hash().is_empty() {
            continue;
        }

        let expected = hex::decode(output.hash())
            .with_context(|| format!("FOD outputHash is not valid hex: {}", output.hash()))?;
        let is_recursive = output.hash_algo().starts_with("r:");

        if is_recursive {
            // NAR hash — match against upload result
            let upload = uploads
                .iter()
                .find(|u| u.store_path == output.path())
                .with_context(|| format!("FOD output '{}' not found in uploads", output.name()))?;
            if upload.nar_hash != expected {
                bail!(
                    "FOD NAR hash mismatch for '{}': expected {}, got {}",
                    output.name(),
                    output.hash(),
                    hex::encode(&upload.nar_hash)
                );
            }
        } else {
            // Flat hash — read file from overlay upper and hash contents
            let store_basename = output
                .path()
                .strip_prefix("/nix/store/")
                .with_context(|| format!("invalid output path: {}", output.path()))?;
            let file_path = overlay_upper.join("nix/store").join(store_basename);
            let content = std::fs::read(&file_path).with_context(|| {
                format!("failed to read FOD output file {}", file_path.display())
            })?;
            let computed: [u8; 32] = Sha256::digest(&content).into();
            if computed.as_slice() != expected {
                bail!(
                    "FOD flat hash mismatch for '{}': expected {}, got {}",
                    output.name(),
                    output.hash(),
                    hex::encode(computed)
                );
            }
        }
    }
    Ok(())
}

/// Fetch metadata for all input paths from the store.
async fn fetch_input_metadata(
    store_client: &mut StoreServiceClient<Channel>,
    input_paths: &[String],
) -> Result<Vec<SynthPathInfo>, ExecutorError> {
    let mut synth_paths = Vec::with_capacity(input_paths.len());

    for path in input_paths {
        let request = QueryPathInfoRequest {
            store_path: path.clone(),
        };

        match tokio::time::timeout(
            rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
            store_client.query_path_info(request),
        )
        .await
        {
            Ok(Ok(response)) => {
                let info = response.into_inner();
                synth_paths.push(path_info_to_synth(&info));
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    path = %path,
                    error = %e,
                    "failed to fetch input path metadata"
                );
                return Err(ExecutorError::MetadataFetch {
                    path: path.clone(),
                    source: e,
                });
            }
            Err(_) => {
                return Err(ExecutorError::MetadataFetch {
                    path: path.clone(),
                    source: tonic::Status::deadline_exceeded("QueryPathInfo timed out"),
                });
            }
        }
    }

    Ok(synth_paths)
}

/// Fetch a .drv file from the store and parse it.
///
/// Used when the scheduler sends `drv_content: empty` (Phase 2a default).
/// The .drv is a single regular file in the store, so we fetch its NAR and
/// extract the ATerm content via `extract_single_file`.
async fn fetch_drv_from_store(
    store_client: &mut StoreServiceClient<Channel>,
    drv_path: &str,
) -> Result<Derivation, ExecutorError> {
    // Wrap in GRPC_STREAM_TIMEOUT: this is the first gRPC call after
    // setup_overlay, so a stalled store would hang the build with an overlay
    // mount held indefinitely. .drv files are small (KB range), so the
    // stream timeout is generous.
    let nar_data = tokio::time::timeout(rio_common::grpc::GRPC_STREAM_TIMEOUT, async {
        let req = GetPathRequest {
            store_path: drv_path.to_string(),
        };
        let mut stream = store_client
            .get_path(req)
            .await
            .map_err(|e| ExecutorError::BuildFailed(format!("GetPath({drv_path}) failed: {e}")))?
            .into_inner();

        let mut nar_data = Vec::new();
        while let Some(msg) = stream.message().await.map_err(|e| {
            ExecutorError::BuildFailed(format!("GetPath({drv_path}) stream error: {e}"))
        })? {
            match msg.msg {
                Some(get_path_response::Msg::Info(_)) => {
                    // .drv files are small; we don't need to pre-size from info.
                }
                Some(get_path_response::Msg::NarChunk(chunk)) => {
                    let new_len = (nar_data.len() as u64).saturating_add(chunk.len() as u64);
                    if new_len > rio_common::limits::MAX_NAR_SIZE {
                        return Err(ExecutorError::BuildFailed(format!(
                            "NAR for {drv_path} exceeds MAX_NAR_SIZE ({} bytes, limit {})",
                            new_len,
                            rio_common::limits::MAX_NAR_SIZE
                        )));
                    }
                    nar_data.extend_from_slice(&chunk);
                }
                None => {
                    tracing::warn!("empty GetPathResponse message (possible proto mismatch)");
                }
            }
        }
        Ok::<Vec<u8>, ExecutorError>(nar_data)
    })
    .await
    .map_err(|_| {
        ExecutorError::BuildFailed(format!(
            "GetPath({drv_path}) timed out after {:?} (store unreachable?)",
            rio_common::grpc::GRPC_STREAM_TIMEOUT
        ))
    })??;

    if nar_data.is_empty() {
        return Err(ExecutorError::BuildFailed(format!(
            ".drv not found in store: {drv_path}"
        )));
    }

    let drv_bytes = rio_nix::nar::extract_single_file(&nar_data)
        .map_err(|e| ExecutorError::BuildFailed(format!("failed to extract .drv from NAR: {e}")))?;

    let drv_text = String::from_utf8(drv_bytes)
        .map_err(|e| ExecutorError::BuildFailed(format!(".drv is not valid UTF-8: {e}")))?;

    Derivation::parse(&drv_text)
        .map_err(|e| ExecutorError::BuildFailed(format!("failed to parse derivation: {e}")))
}

/// Compute the input closure for a derivation by querying the store.
///
/// The input closure consists of:
///   - The .drv file itself (nix-daemon reads it)
///   - All `input_srcs` (source store paths)
///   - All outputs of all `input_drvs` (dependency outputs)
///   - Transitively: all references of the above
///
/// We bootstrap from the .drv's own references (which the store computes at
/// upload time from the NAR content) and walk the reference graph via
/// QueryPathInfo. Paths not yet in the store (e.g., outputs of not-yet-built
/// input drvs) are skipped — FUSE will lazy-fetch them at build time.
async fn compute_input_closure(
    store_client: &mut StoreServiceClient<Channel>,
    drv: &Derivation,
    drv_path: &str,
) -> Result<Vec<String>, ExecutorError> {
    use std::collections::{HashSet, VecDeque};

    let mut closure: HashSet<String> = HashSet::new();
    let mut to_visit: VecDeque<String> = VecDeque::new();

    // Seed: the .drv itself, its input_srcs, and input_drv paths.
    // nix-daemon needs to read the .drv; build needs srcs + dep outputs.
    to_visit.push_back(drv_path.to_string());
    for src in drv.input_srcs() {
        to_visit.push_back(src.clone());
    }
    for input_drv in drv.input_drvs().keys() {
        to_visit.push_back(input_drv.clone());
    }

    // BFS the reference graph. PathInfo.references gives runtime deps;
    // for .drv files, references include all inputDrvs and inputSrcs.
    while let Some(path) = to_visit.pop_front() {
        if !closure.insert(path.clone()) {
            continue; // already visited
        }

        let req = QueryPathInfoRequest {
            store_path: path.clone(),
        };
        let info = match tokio::time::timeout(
            rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
            store_client.query_path_info(req),
        )
        .await
        {
            Ok(Ok(resp)) => resp.into_inner(),
            Ok(Err(e)) if e.code() == tonic::Code::NotFound => {
                // Path not in store yet (output of a not-yet-built input drv).
                // Remove it from the closure set (we inserted it above for
                // dedup) so downstream fetch_input_metadata doesn't fail on it.
                // FUSE will lazy-fetch it at build time if needed.
                closure.remove(&path);
                tracing::debug!(path = %path, "input path not in store; FUSE will lazy-fetch");
                continue;
            }
            Ok(Err(e)) => {
                return Err(ExecutorError::MetadataFetch {
                    path: path.clone(),
                    source: e,
                });
            }
            Err(_) => {
                return Err(ExecutorError::MetadataFetch {
                    path: path.clone(),
                    source: tonic::Status::deadline_exceeded(
                        "QueryPathInfo timed out (closure BFS)",
                    ),
                });
            }
        };

        for r in info.references {
            if !closure.contains(&r) {
                to_visit.push_back(r);
            }
        }
    }

    Ok(closure.into_iter().collect())
}

/// Write nix.conf to the overlay upper layer.
fn setup_nix_conf(upper_dir: &Path) -> Result<(), ExecutorError> {
    let conf_dir = upper_dir.join("etc/nix");
    std::fs::create_dir_all(&conf_dir)
        .map_err(|e| ExecutorError::NixConf(format!("failed to create nix conf dir: {e}")))?;
    std::fs::write(conf_dir.join("nix.conf"), WORKER_NIX_CONF)
        .map_err(|e| ExecutorError::NixConf(format!("failed to write nix.conf: {e}")))?;
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
    fn test_setup_nix_conf() {
        let dir = tempfile::tempdir().unwrap();
        setup_nix_conf(dir.path()).unwrap();

        let conf_path = dir.path().join("etc/nix/nix.conf");
        assert!(conf_path.exists());
        let content = std::fs::read_to_string(&conf_path).unwrap();
        assert!(content.contains("sandbox = true"));
    }

    // -----------------------------------------------------------------------
    // Group 3: Daemon lifecycle
    // -----------------------------------------------------------------------

    /// Verify that run_daemon_build fails when the process doesn't speak the
    /// Nix protocol (handshake failure), and that the caller's always-kill
    /// pattern leaves no leaked process.
    #[tokio::test]
    async fn test_daemon_killed_on_handshake_failure() {
        // Spawn a process that closes stdout immediately (causing handshake
        // read to get EOF fast) but keeps running. `sh -c 'exec >&-; sleep 1000'`
        // closes stdout (FD 1) then sleeps.
        let mut fake_daemon = Command::new("sh")
            .arg("-c")
            .arg("exec >&-; sleep 1000")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn sh");
        let pid = fake_daemon.id().expect("pid");

        // Minimal basic derivation for the test
        let output =
            rio_nix::derivation::DerivationOutput::new("out", "/nix/store/test-out", "", "")
                .unwrap();
        let basic_drv = rio_nix::derivation::BasicDerivation::new(
            vec![output],
            Default::default(),
            "x86_64-linux".into(),
            "/bin/sh".into(),
            vec![],
            Default::default(),
        )
        .unwrap();
        let mut batcher = LogBatcher::new("test.drv".into(), "test-worker".into());
        let (log_tx, _log_rx) = mpsc::channel(4);

        // run_daemon_build should fail quickly (handshake reads EOF from closed stdout).
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run_daemon_build(
                &mut fake_daemon,
                "/nix/store/test.drv",
                &basic_drv,
                Duration::from_secs(5),
                &mut batcher,
                &log_tx,
            ),
        )
        .await
        .expect("handshake should fail fast, not hang");
        assert!(
            result.is_err(),
            "handshake against closed stdout should fail"
        );

        // Caller must kill (as execute_build does)
        let _ = fake_daemon.kill().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), fake_daemon.wait()).await;

        // Verify the process is actually dead. On Linux, /proc/<pid> goes away
        // once the process is reaped. We reaped it above via wait(), so check.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let proc_path = format!("/proc/{pid}");
        let alive = std::path::Path::new(&proc_path).exists();
        assert!(!alive, "daemon process should be dead after kill + wait");
    }

    /// Verify that DAEMON_SETUP_TIMEOUT is shorter than the default daemon
    /// build timeout. If setup timeout were longer, it would be pointless.
    #[test]
    fn test_timeout_ordering() {
        assert!(
            DAEMON_SETUP_TIMEOUT < rio_common::grpc::DEFAULT_DAEMON_TIMEOUT,
            "setup timeout ({DAEMON_SETUP_TIMEOUT:?}) must be shorter than default daemon timeout ({:?})",
            rio_common::grpc::DEFAULT_DAEMON_TIMEOUT
        );
    }

    // -----------------------------------------------------------------------
    // fetch_drv_from_store NAR extraction
    // -----------------------------------------------------------------------

    /// Verify the NAR extraction + ATerm parsing pipeline works end-to-end.
    /// This is the core of fetch_drv_from_store (minus the gRPC transport,
    /// which is straightforward streaming).
    #[test]
    fn test_nar_wrapped_drv_parseable() {
        // Minimal valid ATerm derivation (no inputs, one output).
        let drv_text = r#"Derive([("out","/nix/store/00000000000000000000000000000000-test","","")],[],[],"x86_64-linux","/bin/sh",[],[("out","/nix/store/00000000000000000000000000000000-test")])"#;

        // Wrap in NAR as a single regular file (same as a .drv in the store).
        let nar_node = rio_nix::nar::NarNode::Regular {
            executable: false,
            contents: drv_text.as_bytes().to_vec(),
        };
        let mut nar_bytes = Vec::new();
        rio_nix::nar::serialize(&mut nar_bytes, &nar_node).unwrap();

        // Extract + parse (the tail of fetch_drv_from_store).
        let extracted =
            rio_nix::nar::extract_single_file(&nar_bytes).expect("should extract single-file NAR");
        let text = String::from_utf8(extracted).expect("should be UTF-8");
        let drv = Derivation::parse(&text).expect("should parse as ATerm");

        assert_eq!(drv.outputs().len(), 1);
        assert_eq!(drv.outputs()[0].name(), "out");
        assert_eq!(drv.platform(), "x86_64-linux");
    }

    /// Empty NAR data should produce a clear error (not silent success or panic).
    #[test]
    fn test_empty_nar_rejected() {
        let result = rio_nix::nar::extract_single_file(&[]);
        assert!(result.is_err(), "empty NAR should fail extraction");
    }

    // -----------------------------------------------------------------------
    // Group 6: FOD output hash verification
    // -----------------------------------------------------------------------

    fn make_fod_drv(
        output_path: &str,
        hash_algo: &str,
        hash_hex: &str,
    ) -> rio_nix::derivation::Derivation {
        // Derivation has no public constructor; parse a minimal ATerm.
        let aterm = format!(
            r#"Derive([("out","{output_path}","{hash_algo}","{hash_hex}")],[],[],"x86_64-linux","/bin/sh",[],[("out","{output_path}")])"#
        );
        rio_nix::derivation::Derivation::parse(&aterm)
            .unwrap_or_else(|e| panic!("invalid test ATerm: {e} -- ATerm was: {aterm}"))
    }

    #[test]
    fn test_verify_fod_output_hash_recursive_ok() {
        // r:sha256 — NAR hash comparison
        let expected_hash = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let drv = make_fod_drv("/nix/store/test-fod", "r:sha256", expected_hash);

        let upload = upload::UploadResult {
            store_path: "/nix/store/test-fod".into(),
            nar_hash: hex::decode(expected_hash).unwrap(),
            nar_size: 100,
        };

        let tmp = tempfile::tempdir().unwrap();
        assert!(verify_fod_hashes(&drv, &[upload], tmp.path()).is_ok());
    }

    #[test]
    fn test_verify_fod_output_hash_recursive_mismatch() {
        let expected_hash = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let drv = make_fod_drv("/nix/store/test-fod", "r:sha256", expected_hash);

        // Upload has DIFFERENT hash
        let upload = upload::UploadResult {
            store_path: "/nix/store/test-fod".into(),
            nar_hash: vec![0u8; 32], // all zeros, != expected
            nar_size: 100,
        };

        let tmp = tempfile::tempdir().unwrap();
        let result = verify_fod_hashes(&drv, &[upload], tmp.path());
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("mismatch"),
            "error should mention hash mismatch"
        );
    }

    #[test]
    fn test_verify_fod_output_hash_flat_ok() {
        use sha2::{Digest, Sha256};

        // Flat sha256 — file content hash
        let content = b"hello world flat fod content";
        let expected_hash_bytes: [u8; 32] = Sha256::digest(content).into();
        let expected_hash = hex::encode(expected_hash_bytes);

        let drv = make_fod_drv("/nix/store/test-flat-fod", "sha256", &expected_hash);

        // Write the file to overlay/nix/store/test-flat-fod
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("nix/store");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::write(store_dir.join("test-flat-fod"), content).unwrap();

        // Uploads not used for flat hash verification
        assert!(verify_fod_hashes(&drv, &[], tmp.path()).is_ok());
    }

    #[test]
    fn test_verify_fod_output_hash_flat_mismatch() {
        use sha2::{Digest, Sha256};

        let content = b"actual content";
        let wrong_hash: [u8; 32] = Sha256::digest(b"different content").into();
        let wrong_hash_hex = hex::encode(wrong_hash);

        let drv = make_fod_drv("/nix/store/test-flat-fod", "sha256", &wrong_hash_hex);

        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("nix/store");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::write(store_dir.join("test-flat-fod"), content).unwrap();

        let result = verify_fod_hashes(&drv, &[], tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_fod_non_fod_skipped() {
        // Non-FOD (no hash) should be skipped without error
        let drv = make_fod_drv("/nix/store/test-non-fod", "", "");
        let tmp = tempfile::tempdir().unwrap();
        assert!(verify_fod_hashes(&drv, &[], tmp.path()).is_ok());
    }
}
