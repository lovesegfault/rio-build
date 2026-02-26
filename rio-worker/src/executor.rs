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
    BuildResult as ProtoBuildResult, BuildResultStatus, BuiltOutput, QueryPathInfoRequest,
    WorkAssignment, WorkerMessage, worker_message,
};

use crate::log_stream::LogBatcher;
use crate::overlay;
use crate::synth_db::{self, SynthPathInfo, path_info_to_synth};
use crate::upload;

/// Default timeout for daemon operations.
const DAEMON_BUILD_TIMEOUT: Duration = Duration::from_secs(7200); // 2 hours

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
    let _build_guard = scopeguard::guard((), |()| {
        metrics::gauge!("rio_worker_builds_active").decrement(1.0);
    });

    // 1. Set up overlay
    let overlay_mount = overlay::setup_overlay(fuse_mount_point, overlay_base_dir, &build_id)?;

    // 2. Fetch input path metadata and generate synthetic DB
    let synth_paths = fetch_input_metadata(store_client, &assignment.input_paths).await?;
    let db_dir = overlay::prepare_nix_state_dirs(overlay_mount.upper_dir())?;
    let db_path = db_dir.join("db.sqlite");
    synth_db::generate_db(&db_path, &synth_paths)
        .await
        .map_err(|e| ExecutorError::SynthDb(e.to_string()))?;

    // 3. Parse the derivation from inline ATerm content
    let drv_content = String::from_utf8_lossy(&assignment.drv_content);
    let drv = Derivation::parse(&drv_content)
        .map_err(|e| ExecutorError::BuildFailed(format!("failed to parse derivation: {e}")))?;
    let basic_drv = drv.to_basic();

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
        .unwrap_or(DAEMON_BUILD_TIMEOUT);

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
        .stderr(std::process::Stdio::piped())
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
    let _ = tokio::time::timeout(Duration::from_secs(2), daemon.wait()).await;

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
                let built_outputs: Vec<BuiltOutput> = upload_results
                    .iter()
                    .zip(assignment.output_names.iter())
                    .map(|(result, name)| BuiltOutput {
                        output_name: name.clone(),
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

    // 11. Tear down overlay (explicit, before Drop)
    overlay::teardown_overlay(overlay_mount)
        .unwrap_or_else(|e| tracing::error!(error = %e, "overlay teardown failed"));

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

        // Check for timeout-based flush
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
            _ => {} // discard activity messages
        }
    }

    // Read BuildResult
    rio_nix::protocol::build::read_build_result(reader).await
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

        match store_client.query_path_info(request).await {
            Ok(response) => {
                let info = response.into_inner();
                synth_paths.push(path_info_to_synth(&info));
            }
            Err(e) => {
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
        }
    }

    Ok(synth_paths)
}

/// Write nix.conf to the overlay upper layer.
fn setup_nix_conf(upper_dir: &Path) -> Result<(), ExecutorError> {
    let conf_dir = upper_dir.join("etc/nix");
    std::fs::create_dir_all(&conf_dir)
        .map_err(|e| ExecutorError::SynthDb(format!("failed to create nix conf dir: {e}")))?;
    std::fs::write(conf_dir.join("nix.conf"), WORKER_NIX_CONF)
        .map_err(|e| ExecutorError::SynthDb(format!("failed to write nix.conf: {e}")))?;
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

    /// Verify that DAEMON_SETUP_TIMEOUT is shorter than DAEMON_BUILD_TIMEOUT.
    /// This is a compile-time invariant check — if setup timeout were longer,
    /// it would be pointless.
    #[test]
    fn test_timeout_ordering() {
        assert!(
            DAEMON_SETUP_TIMEOUT < DAEMON_BUILD_TIMEOUT,
            "setup timeout ({DAEMON_SETUP_TIMEOUT:?}) must be shorter than build timeout ({DAEMON_BUILD_TIMEOUT:?})"
        );
    }
}
