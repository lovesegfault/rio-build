//! NAR data validation utilities.
//!
//! PutPath accumulates the full NAR before validation and calls
//! [`validate_nar_digest`] (avoids the double-Vec peak that a
//! streaming hasher would incur).
// r[impl sec.drv.validate]

use sha2::{Digest, Sha256};

/// Validate a buffered NAR against expected hash and size.
///
/// Computes the SHA-256 of `data` and compares it (and `data.len()`)
/// against the trailer-declared values. `expected_hash` must be a
/// 32-byte SHA-256 digest.
///
/// Error messages contain the substrings "size mismatch" and "hash mismatch"
/// respectively, which existing protocol tests assert on.
pub fn validate_nar_digest(
    data: &[u8],
    expected_hash: &[u8],
    expected_size: u64,
) -> anyhow::Result<()> {
    let actual_size = data.len() as u64;
    if actual_size != expected_size {
        return Err(anyhow::anyhow!(
            "NAR size mismatch: declared {}, actual {}",
            expected_size,
            actual_size
        ));
    }

    let actual_hash: [u8; 32] = Sha256::digest(data).into();
    if actual_hash.as_slice() != expected_hash {
        return Err(anyhow::anyhow!(
            "NAR hash mismatch: declared {}, computed {}",
            hex::encode(expected_hash),
            hex::encode(actual_hash)
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

    #[test]
    fn validate_nar_digest_accepts_valid() {
        let data = b"valid nar data";
        let hash = compute_sha256(data);
        assert!(validate_nar_digest(data, &hash, data.len() as u64).is_ok());
    }

    #[test]
    fn validate_nar_digest_rejects_size_mismatch() {
        let data = b"valid nar data";
        let hash = compute_sha256(data);
        let err = validate_nar_digest(b"short", &hash, data.len() as u64).unwrap_err();
        assert!(
            err.to_string().contains("size mismatch"),
            "expected 'size mismatch', got: {err}"
        );
    }

    #[test]
    fn validate_nar_digest_rejects_hash_mismatch() {
        let data = b"valid nar data";
        let wrong_hash = compute_sha256(b"wrong data");
        let err = validate_nar_digest(data, &wrong_hash, data.len() as u64).unwrap_err();
        assert!(
            err.to_string().contains("hash mismatch"),
            "expected 'hash mismatch', got: {err}"
        );
    }
}
