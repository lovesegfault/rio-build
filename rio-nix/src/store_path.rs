//! Nix store path types, parsing, and nixbase32 encoding.

use std::fmt;

use thiserror::Error;

/// The Nix store directory.
pub const STORE_DIR: &str = "/nix/store";

/// Length of the nixbase32-encoded hash part in a store path (32 chars = 20 bytes).
pub const HASH_CHARS: usize = 32;

/// Length of the raw hash bytes in a store path.
pub const HASH_BYTES: usize = 20;

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

    #[error("name contains invalid character: {0:?}")]
    InvalidNameChar(char),

    #[error("invalid nixbase32 character: {0:?}")]
    InvalidBase32Char(char),

    #[error("invalid nixbase32 length: expected {expected}, got {got}")]
    InvalidBase32Length { expected: usize, got: usize },
}

/// The 20-byte hash part of a Nix store path.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct StorePathHash(pub [u8; HASH_BYTES]);

impl fmt::Debug for StorePathHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StorePathHash({})", nixbase32::encode(&self.0))
    }
}

/// A parsed Nix store path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StorePath {
    pub hash: StorePathHash,
    pub name: String,
}

impl StorePath {
    /// Parse a string like `/nix/store/{hash}-{name}` into a `StorePath`.
    pub fn parse(s: &str) -> Result<Self, StorePathError> {
        let rest = s
            .strip_prefix(STORE_DIR)
            .and_then(|r| r.strip_prefix('/'))
            .ok_or(StorePathError::InvalidPrefix)?;

        if rest.len() < HASH_CHARS + 1 {
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
            name: remainder.to_string(),
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
}

impl fmt::Display for StorePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}-{}", STORE_DIR, self.hash_part(), self.name)
    }
}

/// Validate a store path name component.
fn validate_name(name: &str) -> Result<(), StorePathError> {
    if name.is_empty() {
        return Err(StorePathError::EmptyName);
    }
    if name.starts_with('.') {
        return Err(StorePathError::NameStartsWithDot);
    }
    for ch in name.chars() {
        if !is_valid_name_char(ch) {
            return Err(StorePathError::InvalidNameChar(ch));
        }
    }
    Ok(())
}

/// Check if a character is valid in a store path name.
/// Valid: ASCII alphanumeric + `+` `-` `.` `_`
fn is_valid_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.' | '_')
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
    fn test_parse_valid_store_path() {
        let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1";
        let sp = StorePath::parse(path).unwrap();
        assert_eq!(sp.name, "hello-2.12.1");
        assert!(!sp.is_derivation());
    }

    #[test]
    fn test_parse_derivation_path() {
        let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1.drv";
        let sp = StorePath::parse(path).unwrap();
        assert!(sp.is_derivation());
    }

    #[test]
    fn test_parse_roundtrip() {
        let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1";
        let sp = StorePath::parse(path).unwrap();
        assert_eq!(sp.to_string(), path);
    }

    #[test]
    fn test_invalid_prefix() {
        assert!(StorePath::parse("/tmp/store/abc-hello").is_err());
    }

    #[test]
    fn test_empty_name() {
        let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-";
        // This has empty name after the dash — the strip_prefix('-') leaves empty string
        // But actually the hash part is 32 chars, then we need '-' then name
        // so the path without name after dash should fail
        assert!(StorePath::parse(path).is_err());
    }

    #[test]
    fn test_name_with_dot_prefix() {
        let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-.hidden";
        assert!(matches!(
            StorePath::parse(path),
            Err(StorePathError::NameStartsWithDot)
        ));
    }

    #[test]
    fn test_nixbase32_roundtrip() {
        let data = vec![0u8; 20]; // 20 zero bytes
        let encoded = nixbase32::encode(&data);
        assert_eq!(encoded.len(), 32); // ceil(20*8/5) = 32
        let decoded = nixbase32::decode(&encoded).unwrap();
        assert_eq!(decoded, data);
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
        assert!(!is_valid_name_char(' '));
        assert!(!is_valid_name_char('/'));
        assert!(!is_valid_name_char('\0'));
    }
}
