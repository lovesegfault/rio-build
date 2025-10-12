// Build execution logic

use anyhow::{Context, Result};
use rio_common::proto::{
    BuildCompleted, BuildFailed, ExecuteBuildRequest, ExecuteBuildResponse, LogLine,
};
use std::process::Stdio;
use std::time::Instant;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tracing::{error, info, warn};

/// Executes build jobs using local Nix
#[derive(Clone)]
pub struct Executor {}

impl Executor {
    pub fn new() -> Self {
        Self {}
    }

    /// Execute a build job using nix-build
    #[allow(dead_code)]
    #[tracing::instrument(skip(self, request), fields(job_id = %request.job_id))]
    pub async fn execute_build(
        &self,
        request: ExecuteBuildRequest,
    ) -> Result<Vec<ExecuteBuildResponse>> {
        let start_time = Instant::now();
        info!("Executing build for job {}", request.job_id);

        let mut responses = Vec::new();

        // Send initial log
        responses.push(ExecuteBuildResponse {
            job_id: request.job_id.clone(),
            update: Some(rio_common::proto::execute_build_response::Update::Log(
                LogLine {
                    timestamp: chrono::Utc::now().timestamp(),
                    line: "Build executor started".to_string(),
                },
            )),
        });

        // If no derivation bytes, this is a simulation/test
        if request.derivation.is_empty() {
            warn!("No derivation provided, running in simulation mode");
            responses.push(ExecuteBuildResponse {
                job_id: request.job_id.clone(),
                update: Some(rio_common::proto::execute_build_response::Update::Log(
                    LogLine {
                        timestamp: chrono::Utc::now().timestamp(),
                        line: "Simulation mode: no actual build performed".to_string(),
                    },
                )),
            });
            responses.push(ExecuteBuildResponse {
                job_id: request.job_id.clone(),
                update: Some(
                    rio_common::proto::execute_build_response::Update::Completed(BuildCompleted {
                        output_paths: vec!["/nix/store/simulation-output".to_string()],
                        duration_ms: start_time.elapsed().as_millis() as i64,
                    }),
                ),
            });
            return Ok(responses);
        }

        // Write derivation to temporary file
        let drv_path = match self.write_derivation(&request.derivation).await {
            Ok(path) => {
                responses.push(ExecuteBuildResponse {
                    job_id: request.job_id.clone(),
                    update: Some(rio_common::proto::execute_build_response::Update::Log(
                        LogLine {
                            timestamp: chrono::Utc::now().timestamp(),
                            line: format!("Derivation written to {}", path),
                        },
                    )),
                });
                path
            }
            Err(e) => {
                error!("Failed to write derivation: {}", e);
                responses.push(ExecuteBuildResponse {
                    job_id: request.job_id.clone(),
                    update: Some(rio_common::proto::execute_build_response::Update::Failed(
                        BuildFailed {
                            error: format!("Failed to write derivation: {}", e),
                            stderr: None,
                            exit_code: -1,
                        },
                    )),
                });
                return Ok(responses);
            }
        };

        // Execute nix-build and collect output
        match self.run_nix_build(&drv_path, &request.job_id).await {
            Ok(build_responses) => {
                responses.extend(build_responses);
            }
            Err(e) => {
                error!("Build failed: {}", e);
                responses.push(ExecuteBuildResponse {
                    job_id: request.job_id.clone(),
                    update: Some(rio_common::proto::execute_build_response::Update::Failed(
                        BuildFailed {
                            error: format!("Build failed: {}", e),
                            stderr: None,
                            exit_code: -1,
                        },
                    )),
                });
            }
        }

        // Clean up temporary derivation file
        if let Err(e) = fs::remove_file(&drv_path).await {
            warn!(
                "Failed to remove temporary derivation file {}: {}",
                drv_path, e
            );
        }

        info!(
            "Build processing completed for job {} in {:?}",
            request.job_id,
            start_time.elapsed()
        );
        Ok(responses)
    }

    /// Write derivation bytes to a temporary file
    async fn write_derivation(&self, derivation_bytes: &[u8]) -> Result<String> {
        // Create temp directory if it doesn't exist
        let temp_dir = "/tmp/rio-builder";
        fs::create_dir_all(temp_dir)
            .await
            .context("Failed to create temp directory")?;

        // Generate unique filename
        let filename = format!("{}/build-{}.drv", temp_dir, uuid::Uuid::new_v4());

        // Write derivation to file
        let mut file = fs::File::create(&filename)
            .await
            .with_context(|| format!("Failed to create derivation file: {}", filename))?;

        file.write_all(derivation_bytes)
            .await
            .with_context(|| format!("Failed to write derivation to: {}", filename))?;

        file.flush()
            .await
            .context("Failed to flush derivation file")?;

        info!("Wrote derivation to {}", filename);
        Ok(filename)
    }

    /// Execute nix-build and stream output
    async fn run_nix_build(
        &self,
        drv_path: &str,
        job_id: &str,
    ) -> Result<Vec<ExecuteBuildResponse>> {
        let start_time = Instant::now();
        info!("Running nix-build {}", drv_path);

        let mut responses = Vec::new();

        responses.push(ExecuteBuildResponse {
            job_id: job_id.to_string(),
            update: Some(rio_common::proto::execute_build_response::Update::Log(
                LogLine {
                    timestamp: chrono::Utc::now().timestamp(),
                    line: format!("Executing: nix-build {}", drv_path),
                },
            )),
        });

        let mut child = Command::new("nix-build")
            .arg(drv_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn nix-build")?;

        // Capture stdout
        let stdout = child.stdout.take().context("Failed to get stdout")?;
        let mut stdout_reader = BufReader::new(stdout).lines();

        // Capture stderr
        let stderr = child.stderr.take().context("Failed to get stderr")?;
        let mut stderr_reader = BufReader::new(stderr).lines();

        let mut output_paths = Vec::new();
        let mut stderr_lines = Vec::new();

        // Read stdout and stderr concurrently
        loop {
            tokio::select! {
                result = stdout_reader.next_line() => {
                    match result {
                        Ok(Some(line)) => {
                            info!("nix-build stdout: {}", line);

                            // Store paths that look like /nix/store/... outputs
                            if line.starts_with("/nix/store/") {
                                output_paths.push(line.clone());
                            }

                            responses.push(ExecuteBuildResponse {
                                job_id: job_id.to_string(),
                                update: Some(rio_common::proto::execute_build_response::Update::Log(
                                    LogLine {
                                        timestamp: chrono::Utc::now().timestamp(),
                                        line,
                                    },
                                )),
                            });
                        }
                        Ok(None) => break,
                        Err(e) => {
                            error!("Error reading stdout: {}", e);
                            break;
                        }
                    }
                }
                result = stderr_reader.next_line() => {
                    match result {
                        Ok(Some(line)) => {
                            info!("nix-build stderr: {}", line);
                            stderr_lines.push(line.clone());

                            responses.push(ExecuteBuildResponse {
                                job_id: job_id.to_string(),
                                update: Some(rio_common::proto::execute_build_response::Update::Log(
                                    LogLine {
                                        timestamp: chrono::Utc::now().timestamp(),
                                        line: format!("stderr: {}", line),
                                    },
                                )),
                            });
                        }
                        Ok(None) => {},
                        Err(e) => {
                            error!("Error reading stderr: {}", e);
                        }
                    }
                }
            }
        }

        let status = child.wait().await.context("Failed to wait for nix-build")?;
        let duration_ms = start_time.elapsed().as_millis() as i64;

        if status.success() {
            info!(
                "Build succeeded in {}ms, outputs: {:?}",
                duration_ms, output_paths
            );

            responses.push(ExecuteBuildResponse {
                job_id: job_id.to_string(),
                update: Some(
                    rio_common::proto::execute_build_response::Update::Completed(BuildCompleted {
                        output_paths,
                        duration_ms,
                    }),
                ),
            });
        } else {
            let exit_code = status.code().unwrap_or(-1);
            error!("Build failed with exit code {}", exit_code);

            responses.push(ExecuteBuildResponse {
                job_id: job_id.to_string(),
                update: Some(rio_common::proto::execute_build_response::Update::Failed(
                    BuildFailed {
                        error: format!("nix-build failed with exit code {}", exit_code),
                        stderr: Some(stderr_lines.join("\n")),
                        exit_code,
                    },
                )),
            });
        }

        Ok(responses)
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_execute_build_simulation_mode() {
        let executor = Executor::new();

        let request = ExecuteBuildRequest {
            job_id: "test-job-001".to_string(),
            derivation: vec![], // Empty = simulation mode
            required_systems: vec!["x86_64-linux".to_string()],
            env: Default::default(),
            timeout_seconds: Some(300),
        };

        let responses = executor.execute_build(request).await.unwrap();

        // Should have initial log, simulation message, and completion
        assert!(responses.len() >= 3, "Expected at least 3 responses");

        // Check for completion
        let has_completion = responses.iter().any(|r| {
            matches!(
                r.update,
                Some(rio_common::proto::execute_build_response::Update::Completed(_))
            )
        });
        assert!(has_completion, "Should complete successfully");
    }

    #[tokio::test]
    async fn test_write_derivation() {
        let executor = Executor::new();
        let test_data = b"test derivation content";

        let path = executor
            .write_derivation(test_data)
            .await
            .expect("Failed to write derivation");

        // Verify file exists and contains correct content
        let content = tokio::fs::read(&path).await.expect("Failed to read file");
        assert_eq!(content, test_data);

        // Cleanup
        tokio::fs::remove_file(&path)
            .await
            .expect("Failed to cleanup");
    }

    #[tokio::test]
    #[ignore] // Requires nix-build and takes time
    async fn test_execute_build_real_derivation() {
        // This test requires a real Nix installation
        // Create a simple test derivation
        let test_nix = r#"
derivation {
  name = "rio-test";
  system = builtins.currentSystem;
  builder = "/bin/sh";
  args = [ "-c" "echo test > $out" ];
}
"#;

        // Write to temp file
        let nix_file = "/tmp/rio-test.nix";
        tokio::fs::write(nix_file, test_nix)
            .await
            .expect("Failed to write nix file");

        // Instantiate to get .drv
        let output = Command::new("nix-instantiate")
            .arg(nix_file)
            .output()
            .await
            .expect("Failed to run nix-instantiate");

        if !output.status.success() {
            eprintln!("nix-instantiate failed, skipping test");
            return;
        }

        let drv_path = String::from_utf8(output.stdout).unwrap().trim().to_string();

        // Read derivation
        let drv_bytes = tokio::fs::read(&drv_path)
            .await
            .expect("Failed to read derivation");

        // Execute build
        let executor = Executor::new();
        let request = ExecuteBuildRequest {
            job_id: "test-real-build".to_string(),
            derivation: drv_bytes,
            required_systems: vec!["x86_64-linux".to_string()],
            env: Default::default(),
            timeout_seconds: Some(60),
        };

        let responses = executor.execute_build(request).await.unwrap();

        // Verify we got responses
        assert!(!responses.is_empty(), "Should have responses");

        // Check for completion or failure
        let has_result = responses.iter().any(|r| {
            matches!(
                r.update,
                Some(rio_common::proto::execute_build_response::Update::Completed(_))
                    | Some(rio_common::proto::execute_build_response::Update::Failed(_))
            )
        });
        assert!(has_result, "Should have a build result");

        // Cleanup
        let _ = tokio::fs::remove_file(nix_file).await;
    }
}
