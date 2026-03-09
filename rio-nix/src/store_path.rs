//! Nix store path types, parsing, and nixbase32 encoding.

use std::fmt;

use thiserror::Error;

/// The Nix store directory.
pub const STORE_DIR: &str = "/nix/store";

/// The Nix store directory with trailing slash — the prefix to strip when
/// extracting a basename from a raw `&str` path. Prefer [`StorePath::basename`]
/// when you have a parsed `StorePath`.
pub const STORE_PREFIX: &str = "/nix/store/";

/// Length of the nixbase32-encoded hash part in a store path (32 chars = 20 bytes).
pub const HASH_CHARS: usize = 32;

/// Length of the raw hash bytes in a store path.
pub const HASH_BYTES: usize = 20;

/// Maximum length of the name component (from Nix source).
pub const MAX_NAME_LEN: usize = 211;

#[derive(Debug, Error)]
pub enum StorePathError {
    #[error("path does not start with {STORE_DIR}/")]
    InvalidPrefix,

    #[error("path is too short to contain a hash")]
    TooShort,

    #[error("missing dash separator after hash")]
    MissingDash,

    #[error("name is empty")]
    EmptyName,

    #[error("name starts with '.'")]
    NameStartsWithDot,

    #[error("name too long: {0} chars (max {MAX_NAME_LEN})")]
    NameTooLong(usize),

    #[error("name contains invalid character: {0:?}")]
    InvalidNameChar(char),

    #[error("invalid nixbase32 character: {0:?}")]
    InvalidBase32Char(char),

    #[error("invalid nixbase32 length: expected {expected}, got {got}")]
    InvalidBase32Length { expected: usize, got: usize },
}

/// The 20-byte hash part of a Nix store path.
#[must_use]
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct StorePathHash([u8; HASH_BYTES]);

impl StorePathHash {
    /// Access the raw hash bytes.
    pub fn as_bytes(&self) -> &[u8; HASH_BYTES] {
        &self.0
    }
}

impl fmt::Debug for StorePathHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StorePathHash({})", nixbase32::encode(&self.0))
    }
}

/// A parsed Nix store path.
#[must_use]
#[derive(Debug, Clone)]
pub struct StorePath {
    hash: StorePathHash,
    name: String,
    /// Full path string (`/nix/store/{hash}-{name}`). Cached for zero-cost
    /// `as_str()` / `Display` — avoids re-running `nixbase32::encode` on every
    /// `.to_string()` call.
    full: String,
}

impl StorePath {
    /// The hash part of the store path.
    pub fn hash(&self) -> &StorePathHash {
        &self.hash
    }

    /// The name component of the store path.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The full path as a string slice (`/nix/store/{hash}-{name}`).
    pub fn as_str(&self) -> &str {
        &self.full
    }

    /// Returns the basename (`{hash}-{name}`) without the `/nix/store/` prefix.
    ///
    /// Infallible: `StorePath` construction guarantees the prefix is present.
    /// Use this instead of `self.as_str().strip_prefix("/nix/store/").unwrap()`.
    pub fn basename(&self) -> &str {
        // STORE_PREFIX = "/nix/store/" (11 bytes). `full` is guaranteed to
        // start with it by all constructors (parse, from_fingerprint).
        &self.full[STORE_PREFIX.len()..]
    }

    /// SHA-256 digest of the full path string. Used as the store database
    /// primary key (fixed-width, collision-free in practice).
    pub fn sha256_digest(&self) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        Sha256::digest(self.full.as_bytes()).into()
    }
}

impl StorePath {
    /// Parse a string like `/nix/store/{hash}-{name}` into a `StorePath`.
    pub fn parse(s: &str) -> Result<Self, StorePathError> {
        let rest = s
            .strip_prefix(STORE_DIR)
            .and_then(|r| r.strip_prefix('/'))
            .ok_or(StorePathError::InvalidPrefix)?;

        // nixbase32 is always ASCII, so any multi-byte UTF-8 in the hash
        // region is already invalid — but we must check the char boundary
        // before split_at, which panics on non-boundaries.
        if rest.len() < HASH_CHARS + 1 || !rest.is_char_boundary(HASH_CHARS) {
            return Err(StorePathError::TooShort);
        }

        let (hash_str, remainder) = rest.split_at(HASH_CHARS);

        let remainder = remainder
            .strip_prefix('-')
            .ok_or(StorePathError::MissingDash)?;

        if remainder.is_empty() {
            return Err(StorePathError::EmptyName);
        }

        validate_name(remainder)?;

        let hash_bytes = nixbase32::decode(hash_str)?;
        let mut hash = [0u8; HASH_BYTES];
        hash.copy_from_slice(&hash_bytes);

        Ok(StorePath {
            hash: StorePathHash(hash),
            name: remainder.to_owned(),
            full: s.to_owned(),
        })
    }

    /// Return the nixbase32-encoded hash part.
    pub fn hash_part(&self) -> String {
        nixbase32::encode(&self.hash.0)
    }

    /// Check if this store path is a derivation (`.drv` extension).
    pub fn is_derivation(&self) -> bool {
        self.name.ends_with(".drv")
    }

    /// Compute a store path from a content fingerprint.
    ///
    /// This is the core Nix store path computation: hash the fingerprint
    /// with SHA-256, XOR-fold to 20 bytes, and nixbase32-encode.
    fn from_fingerprint(name: &str, fingerprint: &str) -> Result<Self, StorePathError> {
        validate_name(name)?;
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(fingerprint.as_bytes());
        let mut compressed = [0u8; HASH_BYTES];
        for (i, &byte) in digest.iter().enumerate() {
            compressed[i % HASH_BYTES] ^= byte;
        }
        let hash_part = nixbase32::encode(&compressed);
        Ok(StorePath {
            hash: StorePathHash(compressed),
            name: name.to_owned(),
            full: format!("{STORE_DIR}/{hash_part}-{name}"),
        })
    }

    /// Compute the store path for a fixed-output (content-addressed) store object.
    ///
    /// `hash` is the content hash, `is_recursive` indicates NAR vs flat hashing.
    ///
    /// Nix algorithm: for fixed outputs, there's an inner hash step:
    ///   inner = SHA-256("fixed:out:{r:}{algo}:{hex(hash)}:")
    ///   fingerprint = "output:out:sha256:{hex(inner)}:/nix/store:{name}"
    ///   pathHash = compressHash(SHA-256(fingerprint))
    pub fn make_fixed_output(
        name: &str,
        hash: &crate::hash::NixHash,
        is_recursive: bool,
    ) -> Result<Self, StorePathError> {
        let r_prefix = if is_recursive { "r:" } else { "" };
        let inner = format!(
            "fixed:out:{r_prefix}{}:{}:",
            hash.algo(),
            hex::encode(hash.digest()),
        );
        use sha2::{Digest, Sha256};
        let inner_digest = Sha256::digest(inner.as_bytes());

        let fingerprint = format!(
            "output:out:sha256:{}:{STORE_DIR}:{name}",
            hex::encode(inner_digest),
        );
        Self::from_fingerprint(name, &fingerprint)
    }

    /// Compute the store path for a text file (used by `builtins.toFile`).
    ///
    /// `hash` is the SHA-256 of the text content.
    /// `references` are the store paths referenced by the text.
    ///
    /// Nix algorithm (single-level hash, no inner step):
    ///   type = "text" + ":" + ref1 + ":" + ref2 + ...
    ///   fingerprint = "{type}:sha256:{hex(hash)}:/nix/store:{name}"
    ///   pathHash = compressHash(SHA-256(fingerprint))
    pub fn make_text(
        name: &str,
        hash: &crate::hash::NixHash,
        references: &[StorePath],
    ) -> Result<Self, StorePathError> {
        let mut type_str = "text".to_string();
        for r in references {
            type_str.push(':');
            type_str.push_str(r.as_str());
        }

        let fingerprint = format!(
            "{type_str}:sha256:{}:{STORE_DIR}:{name}",
            hex::encode(hash.digest()),
        );
        Self::from_fingerprint(name, &fingerprint)
    }
}

impl std::str::FromStr for StorePath {
    type Err = StorePathError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl fmt::Display for StorePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.full)
    }
}

// --- Ergonomic string-like impls (match string_newtype! pattern) ---

impl std::ops::Deref for StorePath {
    type Target = str;
    fn deref(&self) -> &str {
        &self.full
    }
}

impl AsRef<str> for StorePath {
    fn as_ref(&self) -> &str {
        &self.full
    }
}

impl std::borrow::Borrow<str> for StorePath {
    fn borrow(&self) -> &str {
        &self.full
    }
}

impl PartialEq for StorePath {
    fn eq(&self, other: &Self) -> bool {
        self.full == other.full
    }
}
impl Eq for StorePath {}

impl std::hash::Hash for StorePath {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.full.hash(state);
    }
}

impl PartialEq<str> for StorePath {
    fn eq(&self, other: &str) -> bool {
        self.full == other
    }
}

impl PartialEq<&str> for StorePath {
    fn eq(&self, other: &&str) -> bool {
        self.full == *other
    }
}

impl PartialEq<String> for StorePath {
    fn eq(&self, other: &String) -> bool {
        self.full == *other
    }
}

/// Validate a store path name component.
fn validate_name(name: &str) -> Result<(), StorePathError> {
    if name.is_empty() {
        return Err(StorePathError::EmptyName);
    }
    if name.len() > MAX_NAME_LEN {
        return Err(StorePathError::NameTooLong(name.len()));
    }
    // Match Nix C++ checkName: reject only ".", "..", ".-*", "..-*"
    // Other dot-prefixed names like ".gitignore" are valid.
    // Ref: src/libstore/path.cc:15-28
    if name.starts_with('.') {
        if name == "." || name == ".." {
            return Err(StorePathError::NameStartsWithDot);
        }
        if name.as_bytes().get(1) == Some(&b'-') {
            return Err(StorePathError::NameStartsWithDot);
        }
        if name.starts_with("..") && name.as_bytes().get(2) == Some(&b'-') {
            return Err(StorePathError::NameStartsWithDot);
        }
    }
    for ch in name.chars() {
        if !is_valid_name_char(ch) {
            return Err(StorePathError::InvalidNameChar(ch));
        }
    }
    Ok(())
}

/// Check if a character is valid in a store path name.
/// Valid: ASCII alphanumeric + `+` `-` `.` `_` `?` `=`
/// Ref: Nix C++ checkName() in src/libstore/path.cc
fn is_valid_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.' | '_' | '?' | '=')
}

/// Nix's custom base32 encoding/decoding.
///
/// Uses a non-standard alphabet (`0123456789abcdfghijklmnpqrsvwxyz`) and
/// a least-significant-digit-first encoding order.
pub mod nixbase32 {
    use super::StorePathError;

    /// The nixbase32 alphabet (note: missing `e`, `o`, `t`, `u`).
    const CHARS: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

    /// Encode raw bytes into a nixbase32 string.
    ///
    /// The output length for `n` input bytes is `ceil(n * 8 / 5)`.
    /// Characters are emitted least-significant-first.
    pub fn encode(input: &[u8]) -> String {
        if input.is_empty() {
            return String::new();
        }

        let out_len = (input.len() * 8).div_ceil(5);
        let mut out = vec![0u8; out_len];

        for (i, slot) in out.iter_mut().enumerate() {
            // Extract 5 bits starting at bit position i*5 (LSB first)
            let bit_pos = i * 5;
            let byte_idx = bit_pos / 8;
            let bit_offset = bit_pos % 8;

            let mut val = 0u16;
            if byte_idx < input.len() {
                val = u16::from(input[byte_idx]) >> bit_offset;
            }
            if bit_offset > 3 && byte_idx + 1 < input.len() {
                val |= u16::from(input[byte_idx + 1]) << (8 - bit_offset);
            }
            *slot = CHARS[(val & 0x1f) as usize];
        }

        // Reverse to match Nix's convention (most-significant-digit-first in output)
        out.reverse();
        String::from_utf8(out).expect("nixbase32 alphabet is ASCII")
    }

    /// Decode a nixbase32 string into raw bytes.
    pub fn decode(input: &str) -> Result<Vec<u8>, StorePathError> {
        let out_len = input.len() * 5 / 8;
        let mut out = vec![0u8; out_len];

        let input_bytes = input.as_bytes();
        let input_len = input_bytes.len();

        for i in 0..input_len {
            // Reversed order: input[0] is most significant
            let c = input_bytes[input_len - 1 - i];
            let digit = char_to_digit(c)?;

            let bit_pos = i * 5;
            let byte_idx = bit_pos / 8;
            let bit_offset = bit_pos % 8;

            if byte_idx < out_len {
                out[byte_idx] |= digit << bit_offset;
            }
            if bit_offset > 3 && byte_idx + 1 < out_len {
                out[byte_idx + 1] |= digit >> (8 - bit_offset);
            }
        }

        Ok(out)
    }

    fn char_to_digit(c: u8) -> Result<u8, StorePathError> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'd' => Ok(c - b'a' + 10),
            b'f'..=b'n' => Ok(c - b'f' + 14),
            b'p'..=b's' => Ok(c - b'p' + 23),
            b'v'..=b'z' => Ok(c - b'v' + 27),
            _ => Err(StorePathError::InvalidBase32Char(c as char)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_store_path() -> anyhow::Result<()> {
        let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1";
        let sp = StorePath::parse(path)?;
        assert_eq!(sp.name, "hello-2.12.1");
        assert!(!sp.is_derivation());
        Ok(())
    }

    #[test]
    fn test_parse_derivation_path() -> anyhow::Result<()> {
        let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1.drv";
        let sp = StorePath::parse(path)?;
        assert!(sp.is_derivation());
        Ok(())
    }

    #[test]
    fn test_parse_roundtrip() -> anyhow::Result<()> {
        let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1";
        let sp = StorePath::parse(path)?;
        assert_eq!(sp.to_string(), path);
        Ok(())
    }

    #[test]
    fn test_basename() -> anyhow::Result<()> {
        let sp = StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1")?;
        assert_eq!(
            sp.basename(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1"
        );
        // Invariant: as_str() = STORE_PREFIX + basename()
        assert_eq!(sp.as_str(), format!("{STORE_PREFIX}{}", sp.basename()));
        Ok(())
    }

    #[test]
    fn test_basename_from_fingerprint() -> anyhow::Result<()> {
        // from_fingerprint is the other constructor — verify it also
        // produces a `full` that basename() can slice into correctly.
        let hash = crate::hash::NixHash::new(crate::hash::HashAlgo::SHA256, vec![0u8; 32])?;
        let sp = StorePath::make_text("via-fingerprint", &hash, &[])?;
        assert!(sp.basename().ends_with("-via-fingerprint"));
        assert!(!sp.basename().starts_with('/'));
        assert_eq!(
            sp.basename().len(),
            HASH_CHARS + 1 + "via-fingerprint".len()
        );
        Ok(())
    }

    #[test]
    fn test_sha256_digest() -> anyhow::Result<()> {
        let sp = StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello")?;
        let digest = sp.sha256_digest();
        // Must match direct hashing of the full path string — this is the
        // DB primary key, so stability matters.
        use sha2::{Digest, Sha256};
        let expected: [u8; 32] = Sha256::digest(sp.as_str().as_bytes()).into();
        assert_eq!(digest, expected);
        // Different paths → different digests (not just hashing a constant).
        let sp2 = StorePath::parse("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-hello")?;
        assert_ne!(sp.sha256_digest(), sp2.sha256_digest());
        Ok(())
    }

    #[test]
    fn test_invalid_prefix() {
        assert!(StorePath::parse("/tmp/store/abc-hello").is_err());
    }

    #[test]
    fn test_empty_name() {
        let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-";
        assert!(StorePath::parse(path).is_err());
    }

    #[test]
    fn test_name_dot_prefix_rejected() {
        // "." and ".." are rejected
        assert!(StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-.").is_err());
        assert!(StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-..").is_err());
        // ".-foo" and "..-bar" are rejected (first dash-separated component is . or ..)
        assert!(StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-.-foo").is_err());
        assert!(StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-..-bar").is_err());
    }

    #[test]
    fn test_name_dot_prefix_accepted() {
        // Other dot-prefixed names are valid (e.g., .gitignore)
        assert!(StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-.gitignore").is_ok());
        assert!(StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-.hidden").is_ok());
        assert!(
            StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-..something").is_ok()
        );
    }

    #[test]
    fn test_nixbase32_roundtrip() -> anyhow::Result<()> {
        let data = vec![0u8; 20];
        let encoded = nixbase32::encode(&data);
        assert_eq!(encoded.len(), 32); // ceil(20*8/5) = 32
        let decoded = nixbase32::decode(&encoded)?;
        assert_eq!(decoded, data);
        Ok(())
    }

    #[test]
    fn test_nixbase32_known_value() {
        // Known test vector: all zeros should encode to all '0's
        let zeros = vec![0u8; 20];
        let encoded = nixbase32::encode(&zeros);
        assert_eq!(encoded, "00000000000000000000000000000000");
    }

    #[test]
    fn test_nixbase32_invalid_chars() {
        // 'e', 'o', 't', 'u' are not in the nixbase32 alphabet
        assert!(nixbase32::decode("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeee0").is_err());
    }

    #[test]
    fn test_valid_name_chars() {
        assert!(is_valid_name_char('a'));
        assert!(is_valid_name_char('Z'));
        assert!(is_valid_name_char('0'));
        assert!(is_valid_name_char('+'));
        assert!(is_valid_name_char('-'));
        assert!(is_valid_name_char('.'));
        assert!(is_valid_name_char('_'));
        assert!(is_valid_name_char('?'));
        assert!(is_valid_name_char('='));
        assert!(!is_valid_name_char(' '));
        assert!(!is_valid_name_char('/'));
        assert!(!is_valid_name_char('\0'));
    }

    /// Single test exercising all the string-like trait impls. These were
    /// 10× 3-line blocks at 0% each in lcov (Deref, AsRef, Borrow,
    /// PartialEq×3, FromStr, Hash, StorePathHash::as_bytes + Debug).
    #[test]
    fn test_string_like_trait_impls() -> anyhow::Result<()> {
        let full = "/nix/store/7rjj86p2cgcvwb5zrcvxl0nh2lq3b53y-hello-2.12.1";
        let p = StorePath::parse(full)?;

        // Deref<Target=str>
        let s: &str = &p;
        assert_eq!(s, full);
        // AsRef<str>
        assert_eq!(<StorePath as AsRef<str>>::as_ref(&p), full);
        // Borrow<str>
        assert_eq!(<StorePath as std::borrow::Borrow<str>>::borrow(&p), full);
        // PartialEq<str>
        assert_eq!(p, *full);
        // PartialEq<&str>
        assert_eq!(p, full);
        // PartialEq<String>
        assert_eq!(p, full.to_string());
        // FromStr
        let p2: StorePath = full.parse()?;
        assert_eq!(p, p2);
        // Hash + Borrow<str> → HashSet lookup by &str works
        let mut set = std::collections::HashSet::new();
        set.insert(p.clone());
        assert!(set.contains(full));
        // StorePathHash::as_bytes
        assert_eq!(p.hash().as_bytes().len(), 20);
        // StorePathHash Debug shows the nixbase32-encoded hash
        let dbg = format!("{:?}", p.hash());
        assert!(dbg.starts_with("StorePathHash("));
        assert!(dbg.contains("7rjj86p2cgcvwb5zrcvxl0nh2lq3b53y"));
        Ok(())
    }

    /// make_text with non-empty references exercises the ref-append loop.
    #[test]
    fn test_make_text_with_references() -> anyhow::Result<()> {
        let r1 = StorePath::parse("/nix/store/00000000000000000000000000000000-ref1")?;
        let r2 = StorePath::parse("/nix/store/11111111111111111111111111111111-ref2")?;
        let hash = crate::hash::NixHash::new(crate::hash::HashAlgo::SHA256, vec![0u8; 32])?;
        let p = StorePath::make_text("mytext", &hash, &[r1, r2])?;
        // Exact hash is tested elsewhere (golden conformance); here we just
        // verify the function succeeds with multiple refs.
        assert_eq!(p.name(), "mytext");
        Ok(())
    }

    #[test]
    fn test_name_length_limit() {
        let name = "a".repeat(MAX_NAME_LEN);
        let path = format!("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-{name}");
        assert!(StorePath::parse(&path).is_ok());

        let name = "a".repeat(MAX_NAME_LEN + 1);
        let path = format!("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-{name}");
        assert!(matches!(
            StorePath::parse(&path),
            Err(StorePathError::NameTooLong(_))
        ));
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(4096))]
            #[test]
            fn nixbase32_roundtrip(data in proptest::collection::vec(any::<u8>(), 1..=32)) {
                let encoded = nixbase32::encode(&data);
                let decoded = nixbase32::decode(&encoded)?;
                prop_assert_eq!(decoded, data);
            }
        }
    }
}
