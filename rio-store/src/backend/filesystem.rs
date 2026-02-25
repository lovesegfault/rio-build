//! Filesystem NAR storage backend.
//!
//! Stores NARs as `{base_dir}/{sha256-hex}.nar` using `tokio::fs` for async I/O.

use std::path::{Path, PathBuf};

use bytes::Bytes;
use tokio::io::AsyncRead;
use tracing::debug;

use super::NarBackend;

/// Filesystem-based NAR blob storage backend.
///
/// NARs are stored as flat files in a single directory: `{base_dir}/{sha256-hex}.nar`.
/// The directory is created automatically on first write if it does not exist.
pub struct FilesystemBackend {
    base_dir: PathBuf,
}

impl FilesystemBackend {
    /// Create a new filesystem backend rooted at `base_dir`.
    ///
    /// The directory is created lazily on the first `put` call.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Compute the full file path for a given storage key.
    fn blob_path(&self, key: &str) -> PathBuf {
        self.base_dir.join(key)
    }
}

#[async_trait::async_trait]
impl NarBackend for FilesystemBackend {
    async fn put(&self, sha256_hex: &str, data: Bytes) -> anyhow::Result<String> {
        let key = format!("{sha256_hex}.nar");
        let path = self.blob_path(&key);
        debug!(path = %path.display(), size = data.len(), "FilesystemBackend: storing NAR blob");

        // Ensure the base directory exists.
        tokio::fs::create_dir_all(&self.base_dir).await?;

        // Write atomically: write to a temp file, then rename.
        // This prevents partial writes from being visible.
        let tmp_path = path.with_extension("nar.tmp");
        tokio::fs::write(&tmp_path, &data).await?;
        tokio::fs::rename(&tmp_path, &path).await?;

        Ok(key)
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<Box<dyn AsyncRead + Send + Unpin>>> {
        let path = self.blob_path(key);
        match tokio::fs::File::open(&path).await {
            Ok(file) => Ok(Some(Box::new(file) as Box<dyn AsyncRead + Send + Unpin>)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        let path = self.blob_path(key);
        debug!(path = %path.display(), "FilesystemBackend: deleting NAR blob");
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn exists(&self, key: &str) -> anyhow::Result<bool> {
        let path = self.blob_path(key);
        Ok(Path::new(&path).exists())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn put_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FilesystemBackend::new(dir.path());
        let data = Bytes::from_static(b"filesystem test nar data");
        let key = backend.put("abc123", data.clone()).await.unwrap();
        assert_eq!(key, "abc123.nar");

        let mut reader = backend.get(&key).await.unwrap().unwrap();
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"filesystem test nar data");
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FilesystemBackend::new(dir.path());
        assert!(backend.get("nonexistent.nar").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn exists_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FilesystemBackend::new(dir.path());
        let key = backend
            .put("def456", Bytes::from_static(b"data"))
            .await
            .unwrap();
        assert!(backend.exists(&key).await.unwrap());

        backend.delete(&key).await.unwrap();
        assert!(!backend.exists(&key).await.unwrap());
    }

    #[tokio::test]
    async fn delete_nonexistent_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FilesystemBackend::new(dir.path());
        backend.delete("nonexistent.nar").await.unwrap();
    }

    #[tokio::test]
    async fn creates_base_dir_on_put() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("deep").join("nested");
        let backend = FilesystemBackend::new(&nested);

        assert!(!nested.exists());
        backend
            .put("test", Bytes::from_static(b"data"))
            .await
            .unwrap();
        assert!(nested.exists());
    }
}
