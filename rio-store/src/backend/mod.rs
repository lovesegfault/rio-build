//! Storage backends.
//!
//! `NarBackend` (filesystem/memory/s3): whole-NAR storage. Unused since
//! E1 (inline storage in manifests.inline_blob replaced it), kept for the
//! C3 ChunkBackend construction path — same config fields, different trait.
//!
//! `ChunkBackend` (chunk.rs): BLAKE3-addressed chunk storage for the
//! phase2c chunked CAS. C3 wires this into PutPath.

pub mod chunk;
pub mod filesystem;
pub mod memory;
pub mod s3;

use bytes::Bytes;
use tokio::io::AsyncRead;

/// Trait for NAR blob storage backends.
///
/// Stores and retrieves NAR blobs keyed by their SHA-256 hex digest.
#[async_trait::async_trait]
pub trait NarBackend: Send + Sync {
    /// Store a NAR blob. Returns the storage key.
    async fn put(&self, sha256_hex: &str, data: Bytes) -> anyhow::Result<String>;

    /// Retrieve a NAR blob as a streaming reader.
    async fn get(&self, key: &str) -> anyhow::Result<Option<Box<dyn AsyncRead + Send + Unpin>>>;

    /// Delete a NAR blob.
    async fn delete(&self, key: &str) -> anyhow::Result<()>;

    /// Check if a NAR blob exists.
    async fn exists(&self, key: &str) -> anyhow::Result<bool>;
}
