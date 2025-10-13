//! Build execution logic

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use rio_common::DerivationPath;
use rio_common::proto::{BuildCompleted, BuildFailed, BuildUpdate, LogLine, build_update};
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tonic::Status;

use crate::agent::{Agent, BuildJob};
use crate::nar_exporter;

/// Start a build
///
/// Stores the derivation, spawns nix-build, and manages the build lifecycle.
pub async fn start_build(agent: &Agent, drv_path: String, drv_bytes: Vec<u8>) -> Result<()> {
    let drv_path = Utf8PathBuf::from(drv_path);
    let start_time = Instant::now();

    // Write derivation to data directory
    let drv_filename = drv_path.file_name().context("Invalid derivation path")?;
    let local_drv_path = agent.data_dir.join(drv_filename);

    tokio::fs::write(&local_drv_path, &drv_bytes)
        .await
        .with_context(|| format!("Failed to write derivation to: {}", local_drv_path))?;

    tracing::info!("Wrote derivation to: {}", local_drv_path);

    // Spawn nix-build process
    let child = Command::new("nix-build")
        .arg(local_drv_path.as_str())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to spawn nix-build")?;

    // Create BuildJob
    let build_job = BuildJob {
        drv_path: drv_path.clone(),
        process: child,
        subscribers: Vec::new(),
    };

    // Store in agent's current_build
    {
        let mut current = agent.current_build.lock().await;
        *current = Some(build_job);
    }

    // Spawn task to handle build completion
    let agent_clone = agent.current_build.clone();
    let drv_path_clone = drv_path.clone();
    let data_dir = agent.data_dir.clone();

    tokio::spawn(async move {
        if let Err(e) =
            handle_build_completion(agent_clone, drv_path_clone, data_dir, start_time).await
        {
            tracing::error!("Build completion error: {}", e);
        }
    });

    Ok(())
}

/// Handle build completion (runs in background task)
async fn handle_build_completion(
    current_build: std::sync::Arc<tokio::sync::Mutex<Option<BuildJob>>>,
    drv_path: DerivationPath,
    data_dir: Utf8PathBuf,
    start_time: Instant,
) -> Result<()> {
    // Stream logs
    {
        let mut current = current_build.lock().await;
        if let Some(ref mut build) = *current {
            // Take stdout and stderr
            let stdout = build.process.stdout.take().context("No stdout")?;
            let stderr = build.process.stderr.take().context("No stderr")?;

            // Spawn log streaming tasks
            let subscribers = build.subscribers.clone();
            tokio::spawn(stream_logs(stdout, stderr, drv_path.clone(), subscribers));
        }
    }

    // Wait for completion
    let exit_status = {
        let mut current = current_build.lock().await;
        if let Some(ref mut build) = *current {
            build
                .process
                .wait()
                .await
                .context("Failed to wait for nix-build")?
        } else {
            anyhow::bail!("Build disappeared");
        }
    };

    let duration_ms = start_time.elapsed().as_millis() as i64;

    // Handle result
    if exit_status.success() {
        // Parse output paths from nix-build
        let output_paths = vec![drv_path.as_str().replace(".drv", "")];

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

        // Send completion message
        let completion = BuildUpdate {
            derivation_path: drv_path.as_str().to_string(),
            update: Some(build_update::Update::Completed(BuildCompleted {
                output_paths: output_paths.iter().map(|s| s.to_string()).collect(),
                duration_ms,
            })),
        };

        for sub in &subscribers {
            let _ = sub.send(Ok(completion.clone())).await;
        }
    } else {
        tracing::error!("Build failed with exit code: {:?}", exit_status.code());

        // Send failure message
        let failure = BuildUpdate {
            derivation_path: drv_path.as_str().to_string(),
            update: Some(build_update::Update::Failed(BuildFailed {
                error: format!("nix-build exited with code: {:?}", exit_status.code()),
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

    // Clean up
    {
        let mut current = current_build.lock().await;
        *current = None;
    }

    // Remove temporary derivation file
    let drv_filename = drv_path.file_name().context("Invalid drv path")?;
    let local_drv_path = data_dir.join(drv_filename);
    let _ = tokio::fs::remove_file(&local_drv_path).await;

    Ok(())
}

/// Stream build logs to subscribers
async fn stream_logs(
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    drv_path: DerivationPath,
    subscribers: Vec<tokio::sync::mpsc::Sender<Result<BuildUpdate, Status>>>,
) {
    let mut stdout_lines = BufReader::new(stdout).lines();
    let mut stderr_lines = BufReader::new(stderr).lines();

    loop {
        tokio::select! {
            line = stdout_lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        let update = BuildUpdate {
                            derivation_path: drv_path.as_str().to_string(),
                            update: Some(build_update::Update::Log(LogLine {
                                timestamp: chrono::Utc::now().timestamp_millis(),
                                line: format!("{}\n", line),
                            })),
                        };

                        for sub in &subscribers {
                            let _ = sub.send(Ok(update.clone())).await;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::error!("Error reading stdout: {}", e);
                        break;
                    }
                }
            }
            line = stderr_lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        let update = BuildUpdate {
                            derivation_path: drv_path.as_str().to_string(),
                            update: Some(build_update::Update::Log(LogLine {
                                timestamp: chrono::Utc::now().timestamp_millis(),
                                line: format!("{}\n", line),
                            })),
                        };

                        for sub in &subscribers {
                            let _ = sub.send(Ok(update.clone())).await;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::error!("Error reading stderr: {}", e);
                        break;
                    }
                }
            }
        }
    }
}
