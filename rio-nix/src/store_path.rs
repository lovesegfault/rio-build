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

    #[error("invalid nixbase32 string: non-zero padding bits (non-canonical encoding)")]
    InvalidBase32Padding,

    #[error(
        "fixed-output path with references requires recursive SHA-256 (got {algo}, recursive={recursive})"
    )]
    FixedOutputRefsNotAllowed { algo: &'static str, recursive: bool },
}

/// The 20-byte hash part of a Nix store path.
#[must_use]
#[derive(Clone, PartialEq, Eq, Hash)]
struct StorePathHash([u8; HASH_BYTES]);

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
    /// Nix has two cases (`StoreDirConfig::makeFixedOutputPath`):
    ///
    /// **Recursive SHA-256** — single-level `source` fingerprint, references
    /// allowed (sorted into the type-string):
    ///   type = "source" + ":" + ref1 + ":" + ref2 + ...
    ///   fingerprint = "{type}:sha256:{hex(hash)}:/nix/store:{name}"
    ///
    /// **Everything else** — references must be empty; inner-hash step:
    ///   inner = SHA-256("fixed:out:{r:}{algo}:{hex(hash)}:")
    ///   fingerprint = "output:out:sha256:{hex(inner)}:/nix/store:{name}"
    pub fn make_fixed_output(
        name: &str,
        hash: &crate::hash::NixHash,
        is_recursive: bool,
        references: &[StorePath],
    ) -> Result<Self, StorePathError> {
        use crate::hash::HashAlgo;

        if is_recursive && hash.algo() == HashAlgo::SHA256 {
            let mut sorted: Vec<&str> = references.iter().map(|r| r.as_str()).collect();
            sorted.sort_unstable();
            let mut type_str = "source".to_string();
            for r in sorted {
                type_str.push(':');
                type_str.push_str(r);
            }
            let fingerprint = format!(
                "{type_str}:sha256:{}:{STORE_DIR}:{name}",
                hex::encode(hash.digest()),
            );
            return Self::from_fingerprint(name, &fingerprint);
        }

        if !references.is_empty() {
            return Err(StorePathError::FixedOutputRefsNotAllowed {
                algo: hash.algo().as_str(),
                recursive: is_recursive,
            });
        }

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
        // Nix uses StorePathSet (a BTreeSet), so references are always
        // iterated in sorted order. The wire protocol does not mandate
        // sorted references from the client, so we must sort here.
        // narinfo.rs fingerprint() does the same thing for the same reason.
        let mut sorted: Vec<&str> = references.iter().map(|r| r.as_str()).collect();
        sorted.sort_unstable();

        let mut type_str = "text".to_string();
        for r in sorted {
            type_str.push(':');
            type_str.push_str(r);
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
    pub const CHARS: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

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

            // Low bits of this digit land in out[byte_idx].
            let lo = digit << bit_offset;
            if byte_idx < out_len {
                out[byte_idx] |= lo;
            } else if lo != 0 {
                // These bits have nowhere to go — non-canonical encoding.
                // Nix's parseHash32 throws here.
                return Err(StorePathError::InvalidBase32Padding);
            }

            // High bits (if bit_offset > 3) spill into out[byte_idx + 1].
            if bit_offset > 3 {
                let hi = digit >> (8 - bit_offset);
                if byte_idx + 1 < out_len {
                    out[byte_idx + 1] |= hi;
                } else if hi != 0 {
                    return Err(StorePathError::InvalidBase32Padding);
                }
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
    /// PartialEq×3, FromStr, Hash, StorePathHash Debug).
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
        // StorePathHash Debug shows the nixbase32-encoded hash (private field
        // access ok — we're in the same module)
        let dbg = format!("{:?}", p.hash);
        assert!(dbg.starts_with("StorePathHash("));
        assert!(dbg.contains("7rjj86p2cgcvwb5zrcvxl0nh2lq3b53y"));
        Ok(())
    }

    /// Golden: `nix-store --add-fixed --recursive sha256` on a 5-byte file
    /// containing `"hello"`. Recursive-SHA256 takes the `source` type-string
    /// path (single-level fingerprint, no inner hash).
    #[test]
    fn test_make_fixed_output_recursive_sha256_golden() -> anyhow::Result<()> {
        let nar_hash = crate::hash::NixHash::new(
            crate::hash::HashAlgo::SHA256,
            hex::decode("0a430879c266f8b57f4092a0f935cf3facd48bbccde5760d4748ca405171e969")?,
        )?;
        let p = StorePath::make_fixed_output("rio-golden-src", &nar_hash, true, &[])?;
        assert_eq!(
            p.as_str(),
            "/nix/store/3qn89hl6wq39p2ixq28c7iqq2ifcy3i3-rio-golden-src"
        );
        Ok(())
    }

    /// Golden: `nix-store --add-fixed sha256` (flat) on the same `"hello"` file.
    /// Non-recursive takes the `output:out` + inner-hash path.
    #[test]
    fn test_make_fixed_output_flat_sha256_golden() -> anyhow::Result<()> {
        let flat_hash = crate::hash::NixHash::new(
            crate::hash::HashAlgo::SHA256,
            hex::decode("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")?,
        )?;
        let p = StorePath::make_fixed_output("rio-golden-flat", &flat_hash, false, &[])?;
        assert_eq!(
            p.as_str(),
            "/nix/store/bgl87mmq51gwdn64nvbnxp8vql0164vn-rio-golden-flat"
        );
        Ok(())
    }

    #[test]
    fn test_make_fixed_output_recursive_sha256_sorts_references() -> anyhow::Result<()> {
        let r_a = StorePath::parse("/nix/store/00000000000000000000000000000000-a-ref")?;
        let r_b = StorePath::parse("/nix/store/11111111111111111111111111111111-b-ref")?;
        let hash = crate::hash::NixHash::new(crate::hash::HashAlgo::SHA256, vec![0u8; 32])?;

        let p_sorted =
            StorePath::make_fixed_output("src", &hash, true, &[r_a.clone(), r_b.clone()])?;
        let p_rev = StorePath::make_fixed_output("src", &hash, true, &[r_b, r_a.clone()])?;
        assert_eq!(p_sorted, p_rev, "ref ordering must not affect path");

        let p_noref = StorePath::make_fixed_output("src", &hash, true, &[])?;
        assert_ne!(p_sorted, p_noref, "refs must change the path");
        Ok(())
    }

    #[test]
    fn test_make_fixed_output_refs_rejected_unless_recursive_sha256() -> anyhow::Result<()> {
        let r = StorePath::parse("/nix/store/00000000000000000000000000000000-a-ref")?;
        let sha256 = crate::hash::NixHash::new(crate::hash::HashAlgo::SHA256, vec![0u8; 32])?;
        let sha512 = crate::hash::NixHash::new(crate::hash::HashAlgo::SHA512, vec![0u8; 64])?;

        assert!(matches!(
            StorePath::make_fixed_output("x", &sha256, false, std::slice::from_ref(&r)),
            Err(StorePathError::FixedOutputRefsNotAllowed { .. })
        ));
        assert!(matches!(
            StorePath::make_fixed_output("x", &sha512, true, std::slice::from_ref(&r)),
            Err(StorePathError::FixedOutputRefsNotAllowed { .. })
        ));
        Ok(())
    }

    /// make_text must sort references before hashing. Nix uses a StorePathSet
    /// (BTreeSet), so iteration order is always sorted; the wire protocol does
    /// not mandate sorted order from the client.
    #[test]
    fn test_make_text_sorts_references() -> anyhow::Result<()> {
        let r_a = StorePath::parse("/nix/store/00000000000000000000000000000000-a-ref")?;
        let r_b = StorePath::parse("/nix/store/11111111111111111111111111111111-b-ref")?;
        let hash = crate::hash::NixHash::new(crate::hash::HashAlgo::SHA256, vec![0u8; 32])?;

        // Same reference set, two orderings. Must produce identical path.
        let p_sorted = StorePath::make_text("mytext", &hash, &[r_a.clone(), r_b.clone()])?;
        let p_rev = StorePath::make_text("mytext", &hash, &[r_b, r_a])?;

        assert_eq!(
            p_sorted, p_rev,
            "make_text must be insensitive to caller ref ordering"
        );
        assert_eq!(p_sorted.name(), "mytext");
        Ok(())
    }

    /// nixbase32 encoding is NOT surjective onto the full input-char space when
    /// output_len*8 is not a multiple of 5. For a 32-byte (SHA-256) decode,
    /// 52 chars * 5 = 260 bits but only 256 fit — the top 4 bits of the
    /// most-significant (first) char are padding and must be zero.
    #[test]
    fn test_nixbase32_rejects_nonzero_padding() {
        // 52 chars decode to 32 bytes. The first char's top 4 bits are overflow.
        // 'z' = 31 (0b11111) → all 4 overflow bits set.
        let bad = format!("z{}", "0".repeat(51));
        assert!(matches!(
            nixbase32::decode(&bad),
            Err(StorePathError::InvalidBase32Padding)
        ));

        // '0' = 0 → zero overflow bits. Must still decode cleanly.
        let good = "0".repeat(52);
        let decoded = nixbase32::decode(&good).expect("zero padding is canonical");
        assert_eq!(decoded, vec![0u8; 32]);

        // '1' = 1 (0b00001) → only bit 0 set, which lands in out[31] bit 7.
        // That bit IS in-bounds for 32 bytes (bit 255). No padding, must succeed.
        let one_in_lsb = format!("{}1", "0".repeat(51));
        assert!(nixbase32::decode(&one_in_lsb).is_ok());
    }

    #[test]
    fn test_nixbase32_20byte_no_padding() {
        // 20 bytes = 160 bits = 32 chars * 5 exactly. No padding exists,
        // so every valid-alphabet input is canonical. Exhaustively verify
        // the first char can be anything.
        for c in "0123456789abcdfghijklmnpqrsvwxyz".chars() {
            let s = format!("{c}{}", "0".repeat(31));
            assert!(
                nixbase32::decode(&s).is_ok(),
                "rejected valid first char {c:?}"
            );
        }
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

            /// encode is canonical: decode∘encode = id for 20-byte store-path
            /// hashes (no padding bits exist at 160/5=32 chars exactly).
            #[test]
            fn nixbase32_roundtrip_20(data in proptest::array::uniform20(any::<u8>())) {
                let encoded = nixbase32::encode(&data);
                let decoded = nixbase32::decode(&encoded)?;
                prop_assert_eq!(decoded.as_slice(), &data[..]);
            }

            /// 32-byte SHA-256 hashes have 4 padding bits; padding rejection
            /// enforces canonicality.
            #[test]
            fn nixbase32_roundtrip_32(data in proptest::array::uniform32(any::<u8>())) {
                let encoded = nixbase32::encode(&data);
                let decoded = nixbase32::decode(&encoded)?;
                prop_assert_eq!(decoded.as_slice(), &data[..]);
            }

            /// Mutating a single char in a canonical encoding either fails to
            /// decode (bad char / bad padding) or decodes to DIFFERENT bytes.
            /// This is the injectivity property the padding check restores.
            #[test]
            fn nixbase32_decode_injective(
                data in proptest::array::uniform32(any::<u8>()),
                mutate_idx in 0usize..52,
                mutate_to in 0u8..32,
            ) {
                let encoded = nixbase32::encode(&data);
                let orig_char = encoded.as_bytes()[mutate_idx];
                let new_char = b"0123456789abcdfghijklmnpqrsvwxyz"[mutate_to as usize];
                prop_assume!(orig_char != new_char);

                let mut mutated = encoded.into_bytes();
                mutated[mutate_idx] = new_char;
                let mutated = String::from_utf8(mutated).unwrap();

                match nixbase32::decode(&mutated) {
                    Err(_) => {} // padding rejection — good
                    Ok(decoded) => prop_assert_ne!(decoded.as_slice(), &data[..],
                        "two distinct inputs decoded to identical output: injectivity violated"),
                }
            }
        }
    }
}
