//! In-memory NAR storage backend (for testing).
//!
//! Uses a `HashMap<String, Bytes>` protected by a `RwLock` to store NAR blobs
//! keyed by their SHA-256 hex digest.

use std::collections::HashMap;
use std::sync::RwLock;

use bytes::Bytes;
use tokio::io::AsyncRead;
use tracing::debug;

use super::NarBackend;

/// In-memory NAR blob storage backend.
///
/// Suitable for unit tests and development. Not for production use (no
/// persistence, bounded by process memory).
pub struct MemoryBackend {
    inner: RwLock<HashMap<String, Bytes>>,
}

impl MemoryBackend {
    /// Create an empty in-memory backend.
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    fn read_inner(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Bytes>> {
        self.inner.read().unwrap_or_else(|e| {
            tracing::warn!("MemoryBackend: recovering from poisoned read lock");
            e.into_inner()
        })
    }

    fn write_inner(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Bytes>> {
        self.inner.write().unwrap_or_else(|e| {
            tracing::warn!("MemoryBackend: recovering from poisoned write lock");
            e.into_inner()
        })
    }

    /// TEST-ONLY: directly overwrite a blob's contents.
    /// Used by integration tests to corrupt stored NARs and verify that
    /// GetPath's HashingReader integrity check fires DATA_LOSS.
    /// MemoryBackend is already test-only, so no cfg guard.
    pub fn corrupt_for_test(&self, key: &str, new_data: Bytes) {
        self.write_inner().insert(key.to_string(), new_data);
    }
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl NarBackend for MemoryBackend {
    async fn put(&self, sha256_hex: &str, data: Bytes) -> anyhow::Result<String> {
        let key = format!("{sha256_hex}.nar");
        debug!(key = %key, size = data.len(), "MemoryBackend: storing NAR blob");
        self.write_inner().insert(key.clone(), data);
        Ok(key)
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<Box<dyn AsyncRead + Send + Unpin>>> {
        let data = self.read_inner().get(key).cloned();
        Ok(data.map(|b| Box::new(std::io::Cursor::new(b)) as Box<dyn AsyncRead + Send + Unpin>))
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        debug!(key = %key, "MemoryBackend: deleting NAR blob");
        self.write_inner().remove(key);
        Ok(())
    }

    async fn exists(&self, key: &str) -> anyhow::Result<bool> {
        Ok(self.read_inner().contains_key(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn put_and_get() -> anyhow::Result<()> {
        let backend = MemoryBackend::new();
        let data = Bytes::from_static(b"test nar data");
        let key = backend.put("abc123", data.clone()).await?;
        assert_eq!(key, "abc123.nar");

        let mut reader = backend.get(&key).await?.expect("key just written");
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).await?;
        assert_eq!(buf, b"test nar data");
        Ok(())
    }

    #[tokio::test]
    async fn get_missing_returns_none() -> anyhow::Result<()> {
        let backend = MemoryBackend::new();
        assert!(backend.get("nonexistent.nar").await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn exists_and_delete() -> anyhow::Result<()> {
        let backend = MemoryBackend::new();
        let key = backend.put("def456", Bytes::from_static(b"data")).await?;
        assert!(backend.exists(&key).await?);

        backend.delete(&key).await?;
        assert!(!backend.exists(&key).await?);
        Ok(())
    }

    #[tokio::test]
    async fn put_overwrites() -> anyhow::Result<()> {
        let backend = MemoryBackend::new();
        backend.put("same", Bytes::from_static(b"first")).await?;
        let key = backend.put("same", Bytes::from_static(b"second")).await?;

        let mut reader = backend.get(&key).await?.expect("key just written");
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).await?;
        assert_eq!(buf, b"second");
        Ok(())
    }

    #[tokio::test]
    async fn delete_nonexistent_is_noop() -> anyhow::Result<()> {
        let backend = MemoryBackend::new();
        // Should not error
        backend.delete("nonexistent.nar").await?;
        Ok(())
    }
}
