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
/// Imports the derivation NAR to /nix/store, spawns nix-build, and manages the build lifecycle.
pub async fn start_build(agent: &Agent, drv_path: String, drv_nar_bytes: Vec<u8>) -> Result<()> {
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
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to spawn nix-build")?;

    // Create BuildJob
    let build_job = BuildJob {
        drv_path: actual_drv_path.clone(),
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
    let drv_path_clone = actual_drv_path.clone();

    tokio::spawn(async move {
        if let Err(e) = handle_build_completion(agent_clone, drv_path_clone, start_time).await {
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

/// Handle build completion (runs in background task)
async fn handle_build_completion(
    current_build: std::sync::Arc<tokio::sync::Mutex<Option<BuildJob>>>,
    drv_path: DerivationPath,
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
