//! NAR blob storage backends.
//!
//! Phase 2a stores full NARs (no chunking). The `NarBackend` trait
//! abstracts over filesystem and S3 storage.

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
