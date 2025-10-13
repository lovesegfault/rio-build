//! NAR export and streaming
//!
//! Exports build outputs from /nix/store and streams them to subscribers
//! using the channel bridge pattern from DESIGN.md.

use anyhow::{Context, Result};
use rio_common::DerivationPath;
use rio_common::proto::{BuildUpdate, CompressionType, OutputChunk, build_update};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tonic::Status;

/// Stream build outputs to subscribers
///
/// Implements the NAR streaming pattern:
/// 1. Spawn blocking task for nix-store --export
/// 2. Compress chunks with zstd
/// 3. Send to async channel
/// 4. Forward chunks to gRPC subscribers
pub async fn stream_outputs(
    output_paths: &[String],
    drv_path: DerivationPath,
    subscribers: Vec<tokio::sync::mpsc::Sender<Result<BuildUpdate, Status>>>,
) -> Result<()> {
    tracing::info!("Exporting outputs: {:?}", output_paths);

    // Spawn nix-store --export
    let mut child = Command::new("nix-store")
        .arg("--export")
        .args(output_paths)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to spawn nix-store --export")?;

    let mut stdout = child.stdout.take().context("No stdout")?;

    // Stream chunks
    let mut chunk_index = 0i32;
    let mut buffer = vec![0u8; 1024 * 1024]; // 1MB chunks

    loop {
        let n = stdout
            .read(&mut buffer)
            .await
            .context("Failed to read from nix-store")?;

        if n == 0 {
            // End of stream
            break;
        }

        // Compress chunk with zstd
        let compressed = zstd::stream::encode_all(&buffer[..n], 3)
            .context("Failed to compress chunk with zstd")?;

        // Send chunk to subscribers
        let chunk = BuildUpdate {
            derivation_path: drv_path.as_str().to_string(),
            update: Some(build_update::Update::OutputChunk(OutputChunk {
                data: compressed,
                chunk_index,
                last_chunk: false,
                compression: CompressionType::Zstd as i32,
            })),
        };

        for sub in &subscribers {
            let _ = sub.send(Ok(chunk.clone())).await;
        }

        chunk_index += 1;
    }

    // Send final chunk marker
    let final_chunk = BuildUpdate {
        derivation_path: drv_path.as_str().to_string(),
        update: Some(build_update::Update::OutputChunk(OutputChunk {
            data: vec![],
            chunk_index,
            last_chunk: true,
            compression: CompressionType::Zstd as i32,
        })),
    };

    for sub in &subscribers {
        let _ = sub.send(Ok(final_chunk.clone())).await;
    }

    // Wait for process to complete
    let output = child
        .wait_with_output()
        .await
        .context("Failed to wait for nix-store")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("nix-store --export failed: {}", stderr);
    }

    tracing::info!("Exported {} chunks", chunk_index);

    Ok(())
}
