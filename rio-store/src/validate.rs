//! NAR data validation utilities.
//!
//! PutPath accumulates the full NAR before validation and calls
//! `validate_nar_digest(&NarDigest::from_bytes(data), ...)` — see
//! `put_path_impl` (avoids the double-Vec peak that a streaming
//! hasher would incur).
// r[impl sec.drv.validate]

use sha2::{Digest, Sha256};

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
}
