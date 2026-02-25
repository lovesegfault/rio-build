//! NAR data validation utilities shared by all Store backends.

use sha2::{Digest, Sha256};

use super::traits::PathInfo;

/// Validate that NAR data matches the [`PathInfo`]'s declared hash and size.
///
/// Called by all `Store` implementations after reading the full NAR stream.
/// Error messages contain the substrings "size mismatch" and "hash mismatch"
/// respectively, which existing protocol tests assert on.
///
// TODO: This takes `&[u8]`, requiring full buffering before validation. Streaming
// backends (S3, filesystem) will need an incremental validator that wraps
// `AsyncRead`, computing SHA-256 and counting bytes on the fly, then checking
// at EOF. See also the drain contract on `Store::add_path`.
pub fn validate_nar(data: &[u8], info: &PathInfo) -> anyhow::Result<()> {
    let actual_size = data.len() as u64;
    if actual_size != info.nar_size() {
        return Err(anyhow::anyhow!(
            "NAR size mismatch for '{}': declared {}, actual {}",
            info.path(),
            info.nar_size(),
            actual_size
        ));
    }

    let computed = Sha256::digest(data);
    if computed.as_slice() != info.nar_hash().digest() {
        return Err(anyhow::anyhow!(
            "NAR hash mismatch for '{}': declared {}, computed {}",
            info.path(),
            hex::encode(info.nar_hash().digest()),
            hex::encode(computed)
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::PathInfoBuilder;
    use rio_nix::hash::{HashAlgo, NixHash};
    use rio_nix::store_path::StorePath;

    fn test_path() -> StorePath {
        StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-test-1.0").unwrap()
    }

    fn make_info(data: &[u8]) -> PathInfo {
        let hash = NixHash::compute(HashAlgo::SHA256, data);
        PathInfoBuilder::new(test_path(), hash, data.len() as u64)
            .build()
            .unwrap()
    }

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
}
