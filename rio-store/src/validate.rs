//! NAR data validation utilities.
//!
//! For callers that already have buffered data (the common case: PutPath
//! accumulates the full NAR before validation), use
//! `validate_nar_digest(&NarDigest::from_bytes(data), ...)`.
// r[impl sec.drv.validate]
//!
//! The streaming `HashingReader` path is test-only — grpc.rs now buffers
//! the full NAR and uses `from_bytes` (see grpc.rs:~517 comment re: avoiding
//! the double-Vec peak).

use sha2::{Digest, Sha256};

#[cfg(test)]
use std::pin::Pin;
#[cfg(test)]
use std::task::{Context, Poll};
#[cfg(test)]
use tokio::io::{AsyncRead, ReadBuf};

// PathInfo validation is done at the gRPC boundary

// ---------------------------------------------------------------------------
// NarDigest
// ---------------------------------------------------------------------------

/// Accumulated SHA-256 hash and byte count from NAR data.
///
/// Produced by [`NarDigest::from_bytes`]. Compare against expected
/// values via [`validate_nar_digest`].
#[derive(Clone, PartialEq, Eq)]
#[must_use]
pub struct NarDigest {
    sha256: [u8; 32],
    size: u64,
}

impl NarDigest {
    /// Compute a digest from an already-buffered NAR.
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
/// validate_nar_digest(&digest, expected_hash, expected_size)?;
/// ```
#[cfg(test)]
pub struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
    pub(crate) bytes_read: u64,
    seen_eof: bool,
}

#[cfg(test)]
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

#[cfg(test)]
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

/// Validate a streaming NAR digest against expected hash and size.
///
/// `expected_hash` must be a 32-byte SHA-256 digest.
///
/// Error messages contain the substrings "size mismatch" and "hash mismatch"
/// respectively, which existing protocol tests assert on.
pub fn validate_nar_digest(
    digest: &NarDigest,
    expected_hash: &[u8],
    expected_size: u64,
) -> anyhow::Result<()> {
    if digest.size() != expected_size {
        return Err(anyhow::anyhow!(
            "NAR size mismatch: declared {}, actual {}",
            expected_size,
            digest.size()
        ));
    }

    if digest.sha256().as_slice() != expected_hash {
        return Err(anyhow::anyhow!(
            "NAR hash mismatch: declared {}, computed {}",
            hex::encode(expected_hash),
            hex::encode(digest.sha256())
        ));
    }

    Ok(())
}

// r[verify sec.drv.validate]
// r[verify store.integrity.verify-on-put]
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn compute_sha256(data: &[u8]) -> [u8; 32] {
        Sha256::digest(data).into()
    }

    // --- validate_nar_digest ---

    #[test]
    fn validate_nar_digest_accepts_valid() {
        let data = b"valid nar data";
        let hash = compute_sha256(data);
        let digest = NarDigest::from_bytes(data);
        assert!(validate_nar_digest(&digest, &hash, data.len() as u64).is_ok());
    }

    #[test]
    fn validate_nar_digest_rejects_size_mismatch() {
        let data = b"valid nar data";
        let hash = compute_sha256(data);
        let digest = NarDigest::from_bytes(b"short");
        let err = validate_nar_digest(&digest, &hash, data.len() as u64).unwrap_err();
        assert!(
            err.to_string().contains("size mismatch"),
            "expected 'size mismatch', got: {err}"
        );
    }

    #[test]
    fn validate_nar_digest_rejects_hash_mismatch() {
        let data = b"valid nar data";
        let wrong_hash = compute_sha256(b"wrong data");
        let digest = NarDigest::from_bytes(data);
        let err = validate_nar_digest(&digest, &wrong_hash, data.len() as u64).unwrap_err();
        assert!(
            err.to_string().contains("hash mismatch"),
            "expected 'hash mismatch', got: {err}"
        );
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
    async fn hashing_reader_computes_correct_digest() -> anyhow::Result<()> {
        let data = b"hello world, this is test data for hashing";
        let mut hashing = HashingReader::new(std::io::Cursor::new(data.as_slice()));
        let mut buf = Vec::new();
        hashing.read_to_end(&mut buf).await?;
        assert_eq!(buf, data);

        let digest = hashing.into_digest();
        let expected: [u8; 32] = Sha256::digest(data).into();
        assert_eq!(digest.sha256(), &expected);
        assert_eq!(digest.size(), data.len() as u64);
        Ok(())
    }

    #[tokio::test]
    async fn hashing_reader_empty() -> anyhow::Result<()> {
        let mut hashing = HashingReader::new(std::io::Cursor::new(&[] as &[u8]));
        let mut buf = Vec::new();
        hashing.read_to_end(&mut buf).await?;
        assert!(buf.is_empty());

        let digest = hashing.into_digest();
        let expected: [u8; 32] = Sha256::digest(b"").into();
        assert_eq!(digest.sha256(), &expected);
        assert_eq!(digest.size(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn hashing_reader_bytes_read_tracks_progress() -> anyhow::Result<()> {
        let data = b"abcdefghij"; // 10 bytes
        let mut hashing = HashingReader::new(std::io::Cursor::new(data.as_slice()));
        assert_eq!(hashing.bytes_read, 0);

        let mut buf = [0u8; 5];
        let n = hashing.read(&mut buf).await?;
        assert_eq!(n, 5);
        assert_eq!(hashing.bytes_read, 5);

        let n = hashing.read(&mut buf).await?;
        assert_eq!(n, 5);
        assert_eq!(hashing.bytes_read, 10);
        Ok(())
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
        assert_eq!(hashing.bytes_read, 5);
    }

    #[tokio::test]
    async fn hashing_reader_matches_from_bytes() -> anyhow::Result<()> {
        // Verify streaming digest matches bulk digest for various data sizes.
        for size in [0, 1, 7, 64, 255, 1024, 65536] {
            let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();

            let bulk = NarDigest::from_bytes(&data);

            let mut hashing = HashingReader::new(std::io::Cursor::new(data.as_slice()));
            let mut buf = Vec::new();
            hashing.read_to_end(&mut buf).await?;
            let streaming = hashing.into_digest();

            assert_eq!(
                bulk, streaming,
                "digest mismatch for size {size}: {bulk:?} vs {streaming:?}"
            );
        }
        Ok(())
    }
}
