//! NAR data validation utilities shared by all Store backends.
//!
//! Provides both buffered validation ([`validate_nar`]) and streaming validation
//! via [`HashingReader`] + [`validate_nar_digest`]. Store implementations should
//! prefer the streaming approach: wrap their incoming `AsyncRead` in a
//! `HashingReader`, drain it, then call `validate_nar_digest` on the result.

use std::pin::Pin;
use std::task::{Context, Poll};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, ReadBuf};

use super::traits::PathInfo;

// ---------------------------------------------------------------------------
// NarDigest
// ---------------------------------------------------------------------------

/// Accumulated SHA-256 hash and byte count from streaming NAR data.
///
/// Produced by [`HashingReader::into_digest`] or [`NarDigest::from_bytes`].
/// Compare against [`PathInfo`] via [`validate_nar_digest`].
#[derive(Clone, PartialEq, Eq)]
#[must_use]
pub struct NarDigest {
    sha256: [u8; 32],
    size: u64,
}

impl NarDigest {
    /// Compute a digest from an already-buffered NAR.
    #[allow(dead_code)] // used by validate_nar and tests; needed by future Store backends
    pub fn from_bytes(data: &[u8]) -> Self {
        Self {
            sha256: Sha256::digest(data).into(),
            size: data.len() as u64,
        }
    }

    /// The SHA-256 digest of the NAR data.
    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }

    /// The total byte count of the NAR data.
    pub fn size(&self) -> u64 {
        self.size
    }
}

impl std::fmt::Debug for NarDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NarDigest")
            .field("sha256", &hex::encode(self.sha256))
            .field("size", &self.size)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// HashingReader
// ---------------------------------------------------------------------------

/// `AsyncRead` wrapper that computes SHA-256 and counts bytes on the fly.
///
/// After the inner reader reaches EOF, call [`into_digest`](Self::into_digest)
/// to extract the accumulated [`NarDigest`].
///
/// # Example
///
/// ```ignore
/// let mut hashing = HashingReader::new(nar_data);
/// hashing.read_to_end(&mut buf).await?;
/// let digest = hashing.into_digest();
/// validate_nar_digest(&digest, &info)?;
/// ```
pub struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
    bytes_read: u64,
    seen_eof: bool,
}

impl<R> HashingReader<R> {
    /// Wrap an `AsyncRead` source with incremental SHA-256 hashing.
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes_read: 0,
            seen_eof: false,
        }
    }

    /// Total bytes delivered to the caller so far.
    #[allow(dead_code)] // used by tests; useful for diagnostics in future Store backends
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Consume this reader and return the accumulated digest.
    ///
    /// Should only be called after the inner reader has been fully consumed
    /// (i.e., after `read_to_end` or equivalent). In debug builds, panics
    /// if EOF was never observed.
    pub fn into_digest(self) -> NarDigest {
        debug_assert!(
            self.seen_eof,
            "HashingReader::into_digest called before reader reached EOF"
        );
        NarDigest {
            sha256: self.hasher.finalize().into(),
            size: self.bytes_read,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for HashingReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();

        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let n = buf.filled().len() - before;
                if n > 0 {
                    this.hasher.update(&buf.filled()[before..]);
                    this.bytes_read += n as u64;
                } else {
                    this.seen_eof = true;
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

// ---------------------------------------------------------------------------
// Validation functions
// ---------------------------------------------------------------------------

/// Validate a streaming NAR digest against [`PathInfo`]'s declared hash and size.
///
/// Error messages contain the substrings "size mismatch" and "hash mismatch"
/// respectively, which existing protocol tests assert on.
pub fn validate_nar_digest(digest: &NarDigest, info: &PathInfo) -> anyhow::Result<()> {
    if digest.size() != info.nar_size() {
        return Err(anyhow::anyhow!(
            "NAR size mismatch for '{}': declared {}, actual {}",
            info.path(),
            info.nar_size(),
            digest.size()
        ));
    }

    if digest.sha256().as_slice() != info.nar_hash().digest() {
        return Err(anyhow::anyhow!(
            "NAR hash mismatch for '{}': declared {}, computed {}",
            info.path(),
            hex::encode(info.nar_hash().digest()),
            hex::encode(digest.sha256())
        ));
    }

    Ok(())
}

/// Validate that NAR data matches the [`PathInfo`]'s declared hash and size.
///
/// Convenience wrapper over [`validate_nar_digest`] for callers that already
/// have the full NAR buffered.
///
/// Error messages contain the substrings "size mismatch" and "hash mismatch"
/// respectively, which existing protocol tests assert on.
#[allow(dead_code)] // used by tests; kept for future Store backends with buffered data
pub fn validate_nar(data: &[u8], info: &PathInfo) -> anyhow::Result<()> {
    validate_nar_digest(&NarDigest::from_bytes(data), info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::PathInfoBuilder;
    use rio_nix::hash::{HashAlgo, NixHash};
    use rio_nix::store_path::StorePath;
    use tokio::io::AsyncReadExt;

    fn test_path() -> StorePath {
        StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-test-1.0").unwrap()
    }

    fn make_info(data: &[u8]) -> PathInfo {
        let hash = NixHash::compute(HashAlgo::SHA256, data);
        PathInfoBuilder::new(test_path(), hash, data.len() as u64)
            .build()
            .unwrap()
    }

    // --- validate_nar (existing tests, now delegating to validate_nar_digest) ---

    #[test]
    fn validate_nar_accepts_valid() {
        let data = b"valid nar data";
        let info = make_info(data);
        assert!(validate_nar(data, &info).is_ok());
    }

    #[test]
    fn validate_nar_rejects_size_mismatch() {
        let data = b"valid nar data";
        let info = make_info(data);
        let err = validate_nar(b"short", &info).unwrap_err();
        assert!(
            err.to_string().contains("size mismatch"),
            "expected 'size mismatch', got: {err}"
        );
    }

    #[test]
    fn validate_nar_rejects_hash_mismatch() {
        let data = b"valid nar data";
        // Build info with the correct size but wrong hash
        let wrong_hash = NixHash::compute(HashAlgo::SHA256, b"wrong data");
        let info = PathInfoBuilder::new(test_path(), wrong_hash, data.len() as u64)
            .build()
            .unwrap();
        let err = validate_nar(data, &info).unwrap_err();
        assert!(
            err.to_string().contains("hash mismatch"),
            "expected 'hash mismatch', got: {err}"
        );
    }

    // --- validate_nar_digest ---

    #[test]
    fn validate_nar_digest_accepts_valid() {
        let data = b"valid nar data";
        let info = make_info(data);
        let digest = NarDigest::from_bytes(data);
        assert!(validate_nar_digest(&digest, &info).is_ok());
    }

    #[test]
    fn validate_nar_digest_rejects_size_mismatch() {
        let data = b"valid nar data";
        let info = make_info(data);
        let digest = NarDigest::from_bytes(b"short");
        let err = validate_nar_digest(&digest, &info).unwrap_err();
        assert!(
            err.to_string().contains("size mismatch"),
            "expected 'size mismatch', got: {err}"
        );
    }

    #[test]
    fn validate_nar_digest_rejects_hash_mismatch() {
        let data = b"valid nar data";
        let wrong_hash = NixHash::compute(HashAlgo::SHA256, b"wrong data");
        let info = PathInfoBuilder::new(test_path(), wrong_hash, data.len() as u64)
            .build()
            .unwrap();
        let digest = NarDigest::from_bytes(data);
        let err = validate_nar_digest(&digest, &info).unwrap_err();
        assert!(
            err.to_string().contains("hash mismatch"),
            "expected 'hash mismatch', got: {err}"
        );
    }

    #[test]
    fn validate_nar_delegates_to_digest() {
        let data = b"delegation test data";
        let info = make_info(data);
        let digest = NarDigest::from_bytes(data);
        // Both paths should produce the same result
        assert_eq!(
            validate_nar(data, &info).is_ok(),
            validate_nar_digest(&digest, &info).is_ok()
        );

        // Also test the error case
        let wrong_hash = NixHash::compute(HashAlgo::SHA256, b"wrong");
        let bad_info = PathInfoBuilder::new(test_path(), wrong_hash, data.len() as u64)
            .build()
            .unwrap();
        let err1 = validate_nar(data, &bad_info).unwrap_err().to_string();
        let err2 = validate_nar_digest(&digest, &bad_info)
            .unwrap_err()
            .to_string();
        assert_eq!(err1, err2);
    }

    // --- NarDigest ---

    #[test]
    fn nar_digest_from_bytes() {
        let data = b"test data";
        let digest = NarDigest::from_bytes(data);
        let expected_hash: [u8; 32] = Sha256::digest(data).into();
        assert_eq!(digest.sha256(), &expected_hash);
        assert_eq!(digest.size(), data.len() as u64);
    }

    #[test]
    fn nar_digest_debug_shows_hex() {
        let digest = NarDigest::from_bytes(b"hello");
        let debug = format!("{digest:?}");
        // SHA-256 of "hello" starts with "2cf24dba..."
        assert!(debug.contains("2cf24dba"), "debug should show hex: {debug}");
        assert!(
            !debug.contains("["),
            "debug should not show raw bytes: {debug}"
        );
    }

    // --- HashingReader ---

    #[tokio::test]
    async fn hashing_reader_computes_correct_digest() {
        let data = b"hello world, this is test data for hashing";
        let mut hashing = HashingReader::new(std::io::Cursor::new(data.as_slice()));
        let mut buf = Vec::new();
        hashing.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, data);

        let digest = hashing.into_digest();
        let expected: [u8; 32] = Sha256::digest(data).into();
        assert_eq!(digest.sha256(), &expected);
        assert_eq!(digest.size(), data.len() as u64);
    }

    #[tokio::test]
    async fn hashing_reader_empty() {
        let mut hashing = HashingReader::new(std::io::Cursor::new(&[] as &[u8]));
        let mut buf = Vec::new();
        hashing.read_to_end(&mut buf).await.unwrap();
        assert!(buf.is_empty());

        let digest = hashing.into_digest();
        let expected: [u8; 32] = Sha256::digest(b"").into();
        assert_eq!(digest.sha256(), &expected);
        assert_eq!(digest.size(), 0);
    }

    #[tokio::test]
    async fn hashing_reader_bytes_read_tracks_progress() {
        let data = b"abcdefghij"; // 10 bytes
        let mut hashing = HashingReader::new(std::io::Cursor::new(data.as_slice()));
        assert_eq!(hashing.bytes_read(), 0);

        let mut buf = [0u8; 5];
        let n = hashing.read(&mut buf).await.unwrap();
        assert_eq!(n, 5);
        assert_eq!(hashing.bytes_read(), 5);

        let n = hashing.read(&mut buf).await.unwrap();
        assert_eq!(n, 5);
        assert_eq!(hashing.bytes_read(), 10);
    }

    #[tokio::test]
    async fn hashing_reader_io_error_propagates() {
        // A reader that fails after delivering some bytes.
        struct FailingReader {
            delivered: usize,
        }
        impl AsyncRead for FailingReader {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buf: &mut ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                if self.delivered < 5 {
                    let n = 5_usize.min(buf.remaining());
                    buf.put_slice(&vec![0xAA; n]);
                    self.delivered += n;
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "simulated failure",
                    )))
                }
            }
        }

        let mut hashing = HashingReader::new(FailingReader { delivered: 0 });
        let mut buf = Vec::new();
        let err = hashing.read_to_end(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
        // Should have read the 5 bytes before the error
        assert_eq!(hashing.bytes_read(), 5);
    }

    #[tokio::test]
    async fn hashing_reader_matches_from_bytes() {
        // Verify streaming digest matches bulk digest for various data sizes.
        for size in [0, 1, 7, 64, 255, 1024, 65536] {
            let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();

            let bulk = NarDigest::from_bytes(&data);

            let mut hashing = HashingReader::new(std::io::Cursor::new(data.as_slice()));
            let mut buf = Vec::new();
            hashing.read_to_end(&mut buf).await.unwrap();
            let streaming = hashing.into_digest();

            assert_eq!(
                bulk, streaming,
                "digest mismatch for size {size}: {bulk:?} vs {streaming:?}"
            );
        }
    }
}
