//! Build execution logic

use anyhow::{Context, Result};
use backon::{ExponentialBuilder, Retryable};
use camino::Utf8PathBuf;
use openraft::Raft;
use rio_common::DerivationPath;
use rio_common::proto::{
    BuildCompleted, BuildCompletedReport, BuildFailed, BuildFailedReport, BuildUpdate, LogLine,
    ReportBuildResultRequest, build_update, report_build_result_request,
    rio_agent_client::RioAgentClient,
};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::agent::BuildJob;
use crate::nar_exporter;
use crate::state_machine::RaftCommand;
use crate::storage::{StateMachineStore, TypeConfig};

/// Start a build
///
/// Imports the derivation NAR to /nix/store, spawns nix-build, and manages the build lifecycle.
pub async fn start_build(
    current_build: &Arc<Mutex<Option<BuildJob>>>,
    raft: Arc<Raft<TypeConfig>>,
    state_machine: StateMachineStore,
    drv_path: String,
    drv_nar_bytes: Vec<u8>,
) -> Result<()> {
    let expected_drv_path = Utf8PathBuf::from(drv_path);
    let start_time = Instant::now();

    // Import derivation NAR to /nix/store
    let actual_drv_path = import_derivation_nar(&drv_nar_bytes)
        .await
        .context("Failed to import derivation to Nix store")?;

    // Verify the path matches what CLI sent
    if actual_drv_path != expected_drv_path {
        anyhow::bail!(
            "Derivation path mismatch: expected {}, got {}",
            expected_drv_path,
            actual_drv_path
        );
    }

    tracing::info!("Imported derivation to: {}", actual_drv_path);

    // Spawn nix-build process
    let child = Command::new("nix-build")
        .arg(actual_drv_path.as_str())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to spawn nix-build")?;

    // Create BuildJob (metadata only, process owned by background task)
    let build_job = BuildJob::new(actual_drv_path.clone());

    // Store in current_build
    {
        let mut current = current_build.lock().await;
        *current = Some(build_job);
    }

    // Spawn task to handle build completion (owns the process)
    let current_clone = current_build.clone();
    let drv_path_clone = actual_drv_path.clone();
    let raft_clone = raft.clone();
    let sm_clone = state_machine.clone();

    tokio::spawn(async move {
        if let Err(e) = handle_build_completion(
            current_clone,
            raft_clone,
            sm_clone,
            drv_path_clone,
            child,
            start_time,
        )
        .await
        {
            tracing::error!("Build completion error: {}", e);
        }
    });

    Ok(())
}

/// Import a derivation NAR into the Nix store
///
/// Returns the canonical store path where the derivation was imported.
async fn import_derivation_nar(nar_bytes: &[u8]) -> Result<Utf8PathBuf> {
    use tokio::io::AsyncWriteExt;

    let mut child = Command::new("nix-store")
        .arg("--import")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to spawn nix-store --import")?;

    // Write NAR bytes to stdin
    let mut stdin = child.stdin.take().context("Failed to open stdin")?;
    stdin
        .write_all(nar_bytes)
        .await
        .context("Failed to write NAR to nix-store stdin")?;
    stdin
        .shutdown()
        .await
        .context("Failed to close nix-store stdin")?;

    // Wait for process to complete and capture output
    let output = child
        .wait_with_output()
        .await
        .context("Failed to wait for nix-store")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("nix-store --import failed: {}", stderr);
    }

    // Parse the output path (nix-store --import prints the imported path)
    let stdout = String::from_utf8(output.stdout).context("nix-store output not UTF-8")?;
    let store_path = stdout.trim();

    Ok(Utf8PathBuf::from(store_path))
}

/// Report build result to leader (via RPC if not leader, or direct Raft if leader)
///
/// Non-leader agents call the leader via ReportBuildResult RPC.
/// Leader agents propose directly to their local Raft.
async fn report_build_result_to_leader(
    raft: &Arc<Raft<TypeConfig>>,
    state_machine: &StateMachineStore,
    request: ReportBuildResultRequest,
) -> Result<()> {
    // Try direct Raft proposal first (works if we're the leader)
    let drv_path: DerivationPath = request.derivation_path.clone().into();

    let cmd = match &request.result {
        Some(report_build_result_request::Result::Started(_)) => RaftCommand::BuildStarted {
            derivation_path: drv_path.clone(),
        },
        Some(report_build_result_request::Result::Completed(c)) => {
            let output_paths: Vec<DerivationPath> =
                c.output_paths.iter().map(|p| p.into()).collect();
            RaftCommand::BuildCompleted {
                derivation_path: drv_path.clone(),
                output_paths,
            }
        }
        Some(report_build_result_request::Result::Failed(f)) => RaftCommand::BuildFailed {
            derivation_path: drv_path.clone(),
            error: f.error.clone(),
        },
        None => anyhow::bail!("No result in ReportBuildResultRequest"),
    };

    // Try to propose directly (with retry)
    let raft_clone = raft.clone();
    let cmd_clone = cmd.clone();
    let propose = || async { raft_clone.client_write(cmd_clone.clone()).await };

    let retry_policy = ExponentialBuilder::default()
        .with_max_times(3)
        .with_max_delay(std::time::Duration::from_millis(100));

    match propose.retry(retry_policy).await {
        Ok(_) => {
            tracing::info!("Build result committed to Raft (this agent is leader)");
            return Ok(());
        }
        Err(e) => {
            let error_msg = e.to_string();
            if !error_msg.contains("has to forward request to") {
                // Real Raft error, not a forwarding issue
                anyhow::bail!("Raft proposal failed: {}", e);
            }
            // Fall through to forwarding logic
            tracing::debug!("Not leader, forwarding build result to leader via RPC");
        }
    }

    // We're not the leader - find leader and call via RPC
    let (leader_id, leader_addr) = {
        let metrics = raft.metrics().borrow().clone();
        let leader_id = metrics
            .current_leader
            .ok_or_else(|| anyhow::anyhow!("No current leader known"))?;

        let state = state_machine.data.read();
        let leader_agent =
            state.cluster.agents.get(&leader_id).ok_or_else(|| {
                anyhow::anyhow!("Leader {} not found in cluster state", leader_id)
            })?;

        (leader_id, leader_agent.address.clone())
    };

    tracing::info!(
        "Forwarding build result to leader {} at {}",
        leader_id,
        leader_addr
    );

    // Connect to leader and call ReportBuildResult
    let mut leader_client = RioAgentClient::connect(leader_addr.to_string())
        .await
        .context("Failed to connect to leader")?;

    let response = leader_client
        .report_build_result(request)
        .await
        .context("Failed to report build result to leader")?
        .into_inner();

    if !response.success {
        anyhow::bail!("Leader rejected build result: {}", response.message);
    }

    tracing::info!("Build result forwarded to leader and committed");
    Ok(())
}

/// Handle build completion (runs in background task, owns the process)
async fn handle_build_completion(
    current_build: std::sync::Arc<tokio::sync::Mutex<Option<BuildJob>>>,
    raft: Arc<Raft<TypeConfig>>,
    state_machine: StateMachineStore,
    drv_path: DerivationPath,
    mut process: tokio::process::Child,
    start_time: Instant,
) -> Result<()> {
    tracing::info!("Build completion handler started for {}", drv_path);

    // Take stdout and stderr for streaming/capturing
    let stdout = process.stdout.take().context("No stdout")?;
    let stderr = process.stderr.take().context("No stderr")?;

    // Spawn log streaming task (will read subscribers on each line)
    let current_for_logs = current_build.clone();
    tokio::spawn(stream_logs(stderr, drv_path.clone(), current_for_logs));

    tracing::info!("Log streaming task spawned");

    // Read stdout (nix-build output paths) in separate task
    let stdout_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut stdout_reader = stdout;
        let mut buffer = Vec::new();
        stdout_reader
            .read_to_end(&mut buffer)
            .await
            .context("Failed to read stdout")?;
        Ok::<Vec<u8>, anyhow::Error>(buffer)
    });

    tracing::info!("Waiting for nix-build to complete...");

    // Wait for process completion (no lock needed - we own the process)
    let exit_status = process
        .wait()
        .await
        .context("Failed to wait for nix-build")?;

    tracing::info!("nix-build exited with status: {:?}", exit_status);

    // Get stdout output
    let stdout_output = stdout_task.await.context("stdout task panicked")??;

    tracing::info!("Got stdout: {} bytes", stdout_output.len());

    let duration_ms = start_time.elapsed().as_millis() as i64;

    // Handle result
    if exit_status.success() {
        // Parse output paths from nix-build stdout
        let stdout_str = String::from_utf8(stdout_output).context("nix-build output not UTF-8")?;
        let output_paths: Vec<String> = stdout_str
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.trim().to_string())
            .collect();

        if output_paths.is_empty() {
            anyhow::bail!("nix-build produced no output paths");
        }

        tracing::info!("Build succeeded: {:?}", output_paths);

        // Stream outputs
        let subscribers = {
            let current = current_build.lock().await;
            current
                .as_ref()
                .map(|b| b.subscribers.clone())
                .unwrap_or_default()
        };

        nar_exporter::stream_outputs(&output_paths, drv_path.clone(), subscribers.clone()).await?;

        // Report BuildCompleted to leader (forwards via RPC if not leader)
        let report_request = ReportBuildResultRequest {
            derivation_path: drv_path.as_str().to_string(),
            result: Some(report_build_result_request::Result::Completed(
                BuildCompletedReport {
                    output_paths: output_paths.clone(),
                },
            )),
        };

        if let Err(e) = report_build_result_to_leader(&raft, &state_machine, report_request).await {
            tracing::error!("Failed to report BuildCompleted to leader: {}", e);
            // Continue anyway - build succeeded, just Raft coordination failed
        }

        // Send completion message (get fresh subscriber list in case new clients subscribed)
        let completion = BuildUpdate {
            derivation_path: drv_path.as_str().to_string(),
            update: Some(build_update::Update::Completed(BuildCompleted {
                output_paths: output_paths.iter().map(|s| s.to_string()).collect(),
                duration_ms,
            })),
        };

        let current_subscribers = {
            let current = current_build.lock().await;
            current
                .as_ref()
                .map(|b| b.subscribers.clone())
                .unwrap_or_default()
        };

        for sub in &current_subscribers {
            let _ = sub.send(Ok(completion.clone())).await;
        }
    } else {
        tracing::error!("Build failed with exit code: {:?}", exit_status.code());

        let error_msg = format!("nix-build exited with code: {:?}", exit_status.code());

        // Report BuildFailed to leader (forwards via RPC if not leader)
        let report_request = ReportBuildResultRequest {
            derivation_path: drv_path.as_str().to_string(),
            result: Some(report_build_result_request::Result::Failed(
                BuildFailedReport {
                    error: error_msg.clone(),
                },
            )),
        };

        if let Err(e) = report_build_result_to_leader(&raft, &state_machine, report_request).await {
            tracing::error!("Failed to report BuildFailed to leader: {}", e);
            // Continue anyway - build failed, just Raft coordination failed
        }

        // Send failure message
        let failure = BuildUpdate {
            derivation_path: drv_path.as_str().to_string(),
            update: Some(build_update::Update::Failed(BuildFailed {
                error: error_msg,
                stderr: None,
            })),
        };

        let subscribers = {
            let current = current_build.lock().await;
            current
                .as_ref()
                .map(|b| b.subscribers.clone())
                .unwrap_or_default()
        };

        for sub in &subscribers {
            let _ = sub.send(Ok(failure.clone())).await;
        }
    }

    // Clean up already done (process taken from current_build above)
    Ok(())
}

/// Stream build logs to subscribers (from stderr)
async fn stream_logs(
    stderr: tokio::process::ChildStderr,
    drv_path: DerivationPath,
    current_build: std::sync::Arc<tokio::sync::Mutex<Option<BuildJob>>>,
) {
    let mut stderr_lines = BufReader::new(stderr).lines();

    while let Ok(Some(line)) = stderr_lines.next_line().await {
        let update = BuildUpdate {
            derivation_path: drv_path.as_str().to_string(),
            update: Some(build_update::Update::Log(LogLine {
                timestamp: chrono::Utc::now().timestamp_millis(),
                line: format!("{}\n", line),
            })),
        };

        // Use BuildJob helper to add log to history and broadcast to subscribers
        let mut current = current_build.lock().await;
        if let Some(ref mut build) = *current {
            build.add_log(update).await;
        }
    }
}
