//! Nix hash types: SHA-256, SHA-512, SHA-1.
//!
//! Nix uses SHA-256 for most purposes (NAR hashes, store path computation,
//! content addressing). SHA-512 and SHA-1 appear in older derivations.
//! BLAKE3 is used internally by rio-build for chunk storage but never exposed
//! to Nix clients.

use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use thiserror::Error;

use crate::store_path::nixbase32;

#[derive(Debug, Error)]
pub enum HashError {
    #[error("unknown hash algorithm: {0}")]
    UnknownAlgorithm(String),

    #[error("invalid hash format: {0}")]
    InvalidFormat(String),

    #[error("invalid base64 encoding")]
    InvalidBase64,

    #[error("invalid nixbase32 encoding: {0}")]
    InvalidNixbase32(#[from] crate::store_path::StorePathError),

    #[error("wrong digest length for {algo}: expected {expected}, got {got}")]
    WrongDigestLength {
        algo: String,
        expected: usize,
        got: usize,
    },
}

/// Supported hash algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HashAlgo {
    SHA256,
    SHA512,
    SHA1,
}

impl HashAlgo {
    /// Expected digest length in bytes.
    pub fn digest_len(&self) -> usize {
        match self {
            HashAlgo::SHA256 => 32,
            HashAlgo::SHA512 => 64,
            HashAlgo::SHA1 => 20,
        }
    }

    /// Parse an algorithm name string (case-insensitive).
    pub fn parse(s: &str) -> Result<Self, HashError> {
        match s.to_lowercase().as_str() {
            "sha256" => Ok(HashAlgo::SHA256),
            "sha512" => Ok(HashAlgo::SHA512),
            "sha1" => Ok(HashAlgo::SHA1),
            _ => Err(HashError::UnknownAlgorithm(s.to_string())),
        }
    }

    /// Return the algorithm name as a lowercase string.
    pub fn as_str(&self) -> &'static str {
        match self {
            HashAlgo::SHA256 => "sha256",
            HashAlgo::SHA512 => "sha512",
            HashAlgo::SHA1 => "sha1",
        }
    }
}

impl std::fmt::Display for HashAlgo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A Nix hash value (algorithm + digest bytes).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NixHash {
    pub algo: HashAlgo,
    pub digest: Vec<u8>,
}

impl NixHash {
    /// Create a new hash from algorithm and raw digest bytes.
    pub fn new(algo: HashAlgo, digest: Vec<u8>) -> Result<Self, HashError> {
        if digest.len() != algo.digest_len() {
            return Err(HashError::WrongDigestLength {
                algo: algo.as_str().to_string(),
                expected: algo.digest_len(),
                got: digest.len(),
            });
        }
        Ok(NixHash { algo, digest })
    }

    /// Parse Nix colon format: `"sha256:aabb..."` where the digest is nixbase32-encoded.
    pub fn parse_colon(s: &str) -> Result<Self, HashError> {
        let (algo_str, digest_str) = s
            .split_once(':')
            .ok_or_else(|| HashError::InvalidFormat(format!("missing ':' in {s:?}")))?;

        let algo = HashAlgo::parse(algo_str)?;
        let digest = nixbase32::decode(digest_str)?;

        Self::new(algo, digest)
    }

    /// Parse SRI format: `"sha256-AAAA...="` where the digest is base64-encoded.
    pub fn parse_sri(s: &str) -> Result<Self, HashError> {
        let (algo_str, digest_str) = s
            .split_once('-')
            .ok_or_else(|| HashError::InvalidFormat(format!("missing '-' in {s:?}")))?;

        let algo = HashAlgo::parse(algo_str)?;

        use base64::Engine;
        let digest = base64::engine::general_purpose::STANDARD
            .decode(digest_str)
            .map_err(|_| HashError::InvalidBase64)?;

        Self::new(algo, digest)
    }

    /// Parse either colon format or SRI format, auto-detecting by separator.
    pub fn parse(s: &str) -> Result<Self, HashError> {
        if s.contains(':') {
            Self::parse_colon(s)
        } else if s.contains('-') {
            Self::parse_sri(s)
        } else {
            Err(HashError::InvalidFormat(format!(
                "unrecognized hash format: {s:?}"
            )))
        }
    }

    /// Render in Nix colon format: `sha256:aabb...` (nixbase32 digest).
    pub fn to_colon(&self) -> String {
        format!("{}:{}", self.algo, nixbase32::encode(&self.digest))
    }

    /// Render in SRI format: `sha256-AAAA...=` (base64 digest).
    pub fn to_sri(&self) -> String {
        use base64::Engine;
        format!(
            "{}-{}",
            self.algo,
            base64::engine::general_purpose::STANDARD.encode(&self.digest)
        )
    }

    /// Compute a hash of the given data.
    pub fn compute(algo: HashAlgo, data: &[u8]) -> Self {
        let digest = match algo {
            HashAlgo::SHA256 => Sha256::digest(data).to_vec(),
            HashAlgo::SHA512 => Sha512::digest(data).to_vec(),
            HashAlgo::SHA1 => Sha1::digest(data).to_vec(),
        };

        NixHash { algo, digest }
    }

    /// Truncate to 20 bytes for store path hash computation.
    /// This is used for store path fingerprinting (compress-hash in Nix).
    pub fn truncate_for_store_path(&self) -> [u8; 20] {
        assert_eq!(self.algo, HashAlgo::SHA256, "only SHA-256 can be truncated");
        let mut out = [0u8; 20];
        // Nix compresses by XOR-folding: fold the 32-byte digest into 20 bytes
        for (i, &byte) in self.digest.iter().enumerate() {
            out[i % 20] ^= byte;
        }
        out
    }
}

impl std::fmt::Display for NixHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_sri())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_algo_parse() {
        assert_eq!(HashAlgo::parse("sha256").unwrap(), HashAlgo::SHA256);
        assert_eq!(HashAlgo::parse("SHA256").unwrap(), HashAlgo::SHA256);
        assert_eq!(HashAlgo::parse("sha512").unwrap(), HashAlgo::SHA512);
        assert_eq!(HashAlgo::parse("sha1").unwrap(), HashAlgo::SHA1);
        assert!(HashAlgo::parse("md5").is_err());
    }

    #[test]
    fn test_algo_digest_len() {
        assert_eq!(HashAlgo::SHA256.digest_len(), 32);
        assert_eq!(HashAlgo::SHA512.digest_len(), 64);
        assert_eq!(HashAlgo::SHA1.digest_len(), 20);
    }

    #[test]
    fn test_compute_sha256() {
        let hash = NixHash::compute(HashAlgo::SHA256, b"");
        assert_eq!(hash.algo, HashAlgo::SHA256);
        assert_eq!(hash.digest.len(), 32);
        // SHA-256 of empty string is well-known
        assert_eq!(
            hex::encode(&hash.digest),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_compute_sha1() {
        let hash = NixHash::compute(HashAlgo::SHA1, b"");
        assert_eq!(hash.algo, HashAlgo::SHA1);
        assert_eq!(hash.digest.len(), 20);
        // SHA-1 of empty string
        assert_eq!(
            hex::encode(&hash.digest),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
    }

    #[test]
    fn test_colon_format_roundtrip() {
        let hash = NixHash::compute(HashAlgo::SHA256, b"hello");
        let colon = hash.to_colon();
        assert!(colon.starts_with("sha256:"));
        let parsed = NixHash::parse_colon(&colon).unwrap();
        assert_eq!(parsed, hash);
    }

    #[test]
    fn test_sri_format_roundtrip() {
        let hash = NixHash::compute(HashAlgo::SHA256, b"hello");
        let sri = hash.to_sri();
        assert!(sri.starts_with("sha256-"));
        let parsed = NixHash::parse_sri(&sri).unwrap();
        assert_eq!(parsed, hash);
    }

    #[test]
    fn test_truncate_for_store_path() {
        let hash = NixHash::compute(HashAlgo::SHA256, b"test");
        let truncated = hash.truncate_for_store_path();
        assert_eq!(truncated.len(), 20);
        // Verify XOR fold: bytes[0] ^ bytes[20], bytes[1] ^ bytes[21], etc.
        for (i, &actual) in truncated.iter().enumerate() {
            let mut expected = 0u8;
            for j in (0..32).filter(|j| j % 20 == i) {
                expected ^= hash.digest[j];
            }
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn test_auto_detect_format() {
        let hash = NixHash::compute(HashAlgo::SHA256, b"hello");
        let colon = hash.to_colon();
        let sri = hash.to_sri();

        assert_eq!(NixHash::parse(&colon).unwrap(), hash);
        assert_eq!(NixHash::parse(&sri).unwrap(), hash);
    }

    #[test]
    fn test_wrong_digest_length() {
        assert!(NixHash::new(HashAlgo::SHA256, vec![0u8; 31]).is_err());
        assert!(NixHash::new(HashAlgo::SHA256, vec![0u8; 33]).is_err());
        assert!(NixHash::new(HashAlgo::SHA256, vec![0u8; 32]).is_ok());
    }
}
