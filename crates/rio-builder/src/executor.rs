// Build execution logic

use anyhow::{Context, Result};
use rio_common::proto::{BuildCompleted, ExecuteBuildRequest, ExecuteBuildResponse, LogLine};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::info;

/// Executes build jobs using local Nix
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
        info!("Executing build for job {}", request.job_id);

        // TODO: Parse derivation bytes and save to temporary file
        // For now, simulate a build

        let responses = vec![
            // Send initial log
            ExecuteBuildResponse {
                job_id: request.job_id.clone(),
                update: Some(rio_common::proto::execute_build_response::Update::Log(
                    LogLine {
                        timestamp: chrono::Utc::now().timestamp(),
                        line: "Build executor started (simulation mode)".to_string(),
                    },
                )),
            },
            // TODO: Actually execute nix-build when derivation parsing is implemented
            // For now, return placeholder success
            ExecuteBuildResponse {
                job_id: request.job_id.clone(),
                update: Some(
                    rio_common::proto::execute_build_response::Update::Completed(BuildCompleted {
                        output_paths: vec!["/nix/store/placeholder-output".to_string()],
                        duration_ms: 100,
                    }),
                ),
            },
        ];

        info!("Build completed for job {}", request.job_id);
        Ok(responses)
    }

    /// Execute nix-build and stream output (for future use)
    #[allow(dead_code)]
    async fn run_nix_build(&self, drv_path: &str) -> Result<Vec<String>> {
        info!("Running nix-build {}", drv_path);

        let mut child = Command::new("nix-build")
            .arg(drv_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn nix-build")?;

        let stdout = child.stdout.take().context("Failed to get stdout")?;
        let mut reader = BufReader::new(stdout).lines();

        let mut outputs = Vec::new();
        while let Ok(Some(line)) = reader.next_line().await {
            info!("nix-build: {}", line);
            outputs.push(line);
        }

        let status = child.wait().await.context("Failed to wait for nix-build")?;

        if !status.success() {
            return Err(anyhow::anyhow!("nix-build failed with status: {}", status));
        }

        Ok(outputs)
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}
