//! Build stream output handler
//!
//! Handles BuildUpdate stream from agent:
//! - Prints build logs to stdout
//! - Collects and decompresses NAR chunks
//! - Imports build outputs to local /nix/store

use anyhow::{Context, Result};
use rio_common::proto::BuildUpdate;
use std::collections::BTreeMap;
use tonic::Streaming;

/// Handle a build update stream from the agent
///
/// Processes:
/// - LogLine: Print to stdout
/// - OutputChunk: Collect and decompress
/// - BuildCompleted: Import NAR to /nix/store
/// - BuildFailed: Report error
pub async fn handle_build_stream(mut stream: Streaming<BuildUpdate>) -> Result<()> {
    let mut nar_assembler = NarAssembler::new();

    while let Some(update) = stream
        .message()
        .await
        .context("Failed to receive build update")?
    {
        match update.update {
            Some(rio_common::proto::build_update::Update::Log(log)) => {
                // Print log line to stdout
                print!("{}", log.line);
            }
            Some(rio_common::proto::build_update::Update::OutputChunk(chunk)) => {
                // Collect NAR chunk
                nar_assembler.add_chunk(chunk)?;
            }
            Some(rio_common::proto::build_update::Update::Completed(completed)) => {
                // Build completed successfully
                tracing::info!("Build completed in {}ms", completed.duration_ms);

                // Assemble and decompress the NAR
                let nar_bytes = nar_assembler
                    .assemble()
                    .context("Failed to assemble NAR from chunks")?;

                // Import into local /nix/store
                import_nar(&nar_bytes).await?;

                println!("\nBuild succeeded!");
                println!("Outputs:");
                for path in &completed.output_paths {
                    println!("  {}", path);
                }

                return Ok(());
            }
            Some(rio_common::proto::build_update::Update::Failed(failed)) => {
                // Build failed
                eprintln!("\nBuild failed: {}", failed.error);
                if let Some(stderr) = failed.stderr {
                    eprintln!("\nStderr:\n{}", stderr);
                }
                anyhow::bail!("Build failed");
            }
            None => {
                // Empty update, skip
                continue;
            }
        }
    }

    anyhow::bail!("Build stream ended without completion or failure message")
}

/// Assembles NAR from compressed chunks
struct NarAssembler {
    chunks: BTreeMap<i32, Vec<u8>>,
    last_chunk_received: bool,
}

impl NarAssembler {
    fn new() -> Self {
        Self {
            chunks: BTreeMap::new(),
            last_chunk_received: false,
        }
    }

    /// Add a chunk to the assembler
    fn add_chunk(&mut self, chunk: rio_common::proto::OutputChunk) -> Result<()> {
        if chunk.last_chunk {
            self.last_chunk_received = true;
            // Last chunk marker, may have empty data
            if !chunk.data.is_empty() {
                self.chunks.insert(chunk.chunk_index, chunk.data);
            }
        } else {
            self.chunks.insert(chunk.chunk_index, chunk.data);
        }
        Ok(())
    }

    /// Check if all chunks have been received
    fn is_complete(&self) -> bool {
        self.last_chunk_received
    }

    /// Assemble and decompress all chunks into a single NAR
    fn assemble(&self) -> Result<Vec<u8>> {
        if !self.is_complete() {
            anyhow::bail!("Cannot assemble: not all chunks received");
        }

        // Concatenate all compressed chunks in order
        let mut compressed_nar = Vec::new();
        for (_index, chunk_data) in &self.chunks {
            compressed_nar.extend_from_slice(chunk_data);
        }

        // Decompress with zstd
        let nar_bytes = zstd::stream::decode_all(&compressed_nar[..])
            .context("Failed to decompress NAR with zstd")?;

        tracing::info!(
            "Assembled NAR: {} compressed -> {} decompressed",
            compressed_nar.len(),
            nar_bytes.len()
        );

        Ok(nar_bytes)
    }
}

/// Import a NAR into the local Nix store
async fn import_nar(nar_bytes: &[u8]) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

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

    // Wait for process to complete
    let output = child
        .wait_with_output()
        .await
        .context("Failed to wait for nix-store")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("nix-store --import failed: {}", stderr);
    }

    Ok(())
}
