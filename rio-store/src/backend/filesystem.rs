//! Filesystem NAR storage backend.
//!
//! Stores NARs as `{base_dir}/{sha256-hex}.nar` using `tokio::fs` for async I/O.

use std::path::PathBuf;

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

    /// Validate a storage key: reject path-separator characters, parent
    /// directory components, and null bytes. Keys are SHA-256 hex strings
    /// in practice, but the trait accepts arbitrary strings.
    fn validate_key(key: &str) -> anyhow::Result<()> {
        if key.is_empty() {
            anyhow::bail!("storage key is empty");
        }
        if key.contains('/') || key.contains('\\') || key.contains('\0') {
            anyhow::bail!("storage key contains path separator or null byte: {key:?}");
        }
        if key.contains("..") {
            anyhow::bail!("storage key contains parent directory component: {key:?}");
        }
        Ok(())
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
        Self::validate_key(&key)?;
        let path = self.blob_path(&key);
        debug!(path = %path.display(), size = data.len(), "FilesystemBackend: storing NAR blob");

        // Ensure the base directory exists.
        tokio::fs::create_dir_all(&self.base_dir).await?;

        // Write atomically: write to temp + fsync, rename, fsync parent dir.
        // fsync is critical: complete_upload() flips PG status='complete'
        // immediately after put() returns. Without fsync, a power loss leaves
        // PG saying 'complete' but the blob is zero-length or missing (rename
        // not durable). Subsequent PutPath returns created=false (idempotency
        // short-circuit), so the path is permanently stuck with a corrupt blob.
        let tmp_path = path.with_extension("nar.tmp");
        {
            use tokio::io::AsyncWriteExt;
            let mut f = tokio::fs::File::create(&tmp_path).await?;
            f.write_all(&data).await?;
            f.sync_all().await?;
        }
        tokio::fs::rename(&tmp_path, &path).await?;
        // fsync the parent directory so the rename itself is durable.
        let dir = tokio::fs::File::open(&self.base_dir).await?;
        dir.sync_all().await?;

        Ok(key)
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<Box<dyn AsyncRead + Send + Unpin>>> {
        Self::validate_key(key)?;
        let path = self.blob_path(key);
        match tokio::fs::File::open(&path).await {
            Ok(file) => Ok(Some(Box::new(file) as Box<dyn AsyncRead + Send + Unpin>)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        Self::validate_key(key)?;
        let path = self.blob_path(key);
        debug!(path = %path.display(), "FilesystemBackend: deleting NAR blob");
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn exists(&self, key: &str) -> anyhow::Result<bool> {
        Self::validate_key(key)?;
        let path = self.blob_path(key);
        Ok(tokio::fs::try_exists(&path).await.unwrap_or(false))
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

    #[tokio::test]
    async fn rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FilesystemBackend::new(dir.path());

        // All operations should reject traversal attempts.
        assert!(backend.get("../etc/passwd").await.is_err());
        assert!(backend.get("foo/../bar").await.is_err());
        assert!(backend.delete("../../outside").await.is_err());
        assert!(backend.exists("subdir/file.nar").await.is_err());
        assert!(backend.get("").await.is_err());

        // Valid keys (no slashes, no ..) should work.
        assert!(backend.get("valid-key.nar").await.is_ok());
    }
}
