//! The boundary type for NAR content identity.
//!
//! A NAR hash crosses this crate's boundaries in several spellings —
//! `sha256:<nixbase32>` from cache.nixos.org narinfos, `sha256:<hex>` from
//! rio-store-recorded narinfos, bare lowercase hex in the archive's
//! `outcomes.jsonl` (`nar_hash_hex`) and in rio-store query results, and SRI
//! (`sha256-<base64>`) in tooling output. Historically each consumer decoded
//! the value with its own grammar, so a spelling one layer legitimately
//! produced was rejected (or string-compared and never equal) by the next.
//!
//! [`NarHash`] is the single decode point: parse once at the boundary with
//! [`NarHash::parse`] (all spellings, one constructor), carry the 32-byte
//! digest, and compare digest-to-digest. There is deliberately no API for
//! comparing a `NarHash` against an unparsed string.

use std::fmt;

use anyhow::Context as _;

/// SHA-256 digest of an uncompressed NAR serialization, parsed once at the
/// boundary it crossed (narinfo field, archive record, store query result).
///
/// Equality is digest equality, so values parsed from different spellings of
/// the same hash compare equal. Serializes as bare lowercase hex (the archive
/// and results.jsonl wire form); deserializes through [`NarHash::parse`], so
/// every accepted spelling loads.
/// Ordering is over the digest bytes — a content-identity order,
/// arbitrary but total. Consumers use it for deterministic
/// content-keyed tiebreaks (the truth-collapse within-rank pick), never
/// as a semantic "newer/better" judgment.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NarHash([u8; 32]);

impl NarHash {
    /// Parse any accepted spelling of a SHA-256 NAR hash:
    ///
    /// - `sha256:<52-char nixbase32>` — narinfo form served by
    ///   cache.nixos.org and written by Nix tooling;
    /// - `sha256:<64-char hex>` — narinfo form recorded by rio-store;
    /// - bare `<64-char hex>` — the archive `nar_hash_hex` wire form and
    ///   rio-store's query encoding;
    /// - `sha256-<base64>` — SRI.
    ///
    /// Anything else — other algorithms (md5, sha1, sha512), wrong digest
    /// lengths, undecodable digits — is an error naming the value. Hex digits
    /// may be upper- or lowercase; the stored digest is canonical bytes.
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        if let Some(rest) = value.strip_prefix("sha256:") {
            let bytes = match rest.len() {
                52 => rio_nix::store_path::nixbase32::decode(rest)
                    .map_err(|e| anyhow::anyhow!("decode nixbase32 NarHash {value:?}: {e}"))?,
                64 => hex::decode(rest).with_context(|| format!("decode hex NarHash {value:?}"))?,
                n => anyhow::bail!("NarHash digest has unexpected length {n}: {value:?}"),
            };
            return Self::from_digest_slice(&bytes).with_context(|| format!("NarHash {value:?}"));
        }
        if value.starts_with("sha256-") {
            let hash = rio_nix::hash::NixHash::parse_sri(value)
                .map_err(|e| anyhow::anyhow!("decode SRI NarHash {value:?}: {e}"))?;
            return Self::from_digest_slice(hash.digest())
                .with_context(|| format!("NarHash {value:?}"));
        }
        if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
            let bytes =
                hex::decode(value).with_context(|| format!("decode hex NarHash {value:?}"))?;
            return Self::from_digest_slice(&bytes).with_context(|| format!("NarHash {value:?}"));
        }
        anyhow::bail!(
            "unsupported NarHash form {value:?} (expected sha256:<nixbase32>, sha256:<hex>, \
             sha256-<base64>, or 64 hex characters)"
        )
    }

    /// Wrap an already-raw SHA-256 digest (e.g. a freshly computed
    /// `Sha256::digest`).
    pub const fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Wrap a raw digest arriving as a byte slice (e.g. the store API's
    /// wire-encoded `nar_hash`); errors when the length is not 32 bytes.
    pub fn from_digest_slice(bytes: &[u8]) -> anyhow::Result<Self> {
        let digest: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("digest is {} bytes, want 32", bytes.len()))?;
        Ok(Self(digest))
    }

    /// The raw 32-byte digest.
    pub const fn digest(&self) -> &[u8; 32] {
        &self.0
    }

    /// Bare lowercase hex (the canonical serialization).
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Debug for NarHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NarHash({})", self.to_hex())
    }
}

impl fmt::Display for NarHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl serde::Serialize for NarHash {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> serde::Deserialize<'de> for NarHash {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = std::borrow::Cow::<str>::deserialize(deserializer)?;
        Self::parse(&value).map_err(|e| serde::de::Error::custom(format!("{e:#}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sha256 of "hello" — a digest with hex digits outside the nixbase32
    /// alphabet, so a decoder accidentally applying the wrong grammar can
    /// never accept both spellings.
    const HELLO_HEX: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn all_spellings_parse_to_the_same_digest() {
        let digest = <[u8; 32]>::try_from(hex::decode(HELLO_HEX).unwrap().as_slice()).unwrap();
        let from_digest = NarHash::from_digest(digest);
        let spellings = [
            // Bare hex: archive nar_hash_hex / rio-store query encoding.
            HELLO_HEX.to_string(),
            // Colon hex: rio-store narinfo form (uppercase also accepted).
            format!("sha256:{HELLO_HEX}"),
            format!("sha256:{}", HELLO_HEX.to_ascii_uppercase()),
            // Colon nixbase32: cache.nixos.org narinfo form. The literal is
            // `nix-hash --type sha256 --to-base32` of the same digest, so an
            // encoder/decoder bug that cancels out in a roundtrip through
            // rio-nix's own encoder would still be caught.
            format!("sha256:{}", rio_nix::store_path::nixbase32::encode(&digest)),
            "sha256:094qif9n4cq4fdg459qzbhg1c6wywawwaaivx0k0x8xhbyx4vwic".to_string(),
            // SRI.
            rio_nix::hash::NixHash::new(rio_nix::hash::HashAlgo::SHA256, digest.to_vec())
                .unwrap()
                .to_sri(),
        ];
        for spelling in &spellings {
            let parsed = NarHash::parse(spelling)
                .unwrap_or_else(|e| panic!("{spelling:?} must parse: {e:#}"));
            // Cross-compare: every spelling is the same value.
            assert_eq!(parsed, from_digest, "spelling {spelling:?}");
            assert_eq!(parsed.to_hex(), HELLO_HEX, "spelling {spelling:?}");
            assert_eq!(parsed.digest(), &digest, "spelling {spelling:?}");
            // Round-trip: the canonical serialization re-parses to itself.
            assert_eq!(NarHash::parse(&parsed.to_hex()).unwrap(), parsed);
        }
    }

    #[test]
    fn serde_emits_hex_and_accepts_every_spelling() {
        let hash = NarHash::parse(HELLO_HEX).unwrap();
        assert_eq!(
            serde_json::to_value(hash).unwrap(),
            serde_json::Value::String(HELLO_HEX.to_string())
        );
        for spelling in [
            format!("\"{HELLO_HEX}\""),
            format!("\"sha256:{HELLO_HEX}\""),
            "\"sha256:094qif9n4cq4fdg459qzbhg1c6wywawwaaivx0k0x8xhbyx4vwic\"".to_string(),
        ] {
            let parsed: NarHash = serde_json::from_str(&spelling).unwrap();
            assert_eq!(parsed, hash, "spelling {spelling}");
        }
        let err = serde_json::from_str::<NarHash>("\"md5:0123456789abcdef\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("md5:0123456789abcdef"), "got: {err}");
    }

    #[test]
    fn unusable_values_are_rejected_naming_the_value() {
        let rejected: Vec<String> = vec![
            // Other algorithms in every spelling family.
            "md5:0123456789abcdef0123456789abcdef".into(),
            "sha1:0123456789abcdef0123456789abcdef01234567".into(),
            format!("sha512:{}", "ab".repeat(64)),
            format!("sha512-{}", "QUJD".repeat(21)),
            // Wrong digest lengths.
            "sha256:short".into(),
            format!("sha256:{}", "ab".repeat(16)),
            "ab".repeat(16),
            "ab".repeat(64),
            // Right length, wrong alphabet.
            format!("sha256:{}zz", &HELLO_HEX[..62]),
            format!("{}zz", &HELLO_HEX[..62]),
            // Not a hash at all.
            String::new(),
            "not-a-hash".into(),
        ];
        for value in &rejected {
            let err = NarHash::parse(value).expect_err(&format!("{value:?} must be rejected"));
            let msg = format!("{err:#}");
            assert!(
                value.is_empty() || msg.contains(value.as_str()),
                "error for {value:?} must name the value: {msg}"
            );
        }
    }

    #[test]
    fn digest_slice_constructor_enforces_length() {
        assert!(NarHash::from_digest_slice(&[7u8; 32]).is_ok());
        let err = NarHash::from_digest_slice(&[7u8; 20]).unwrap_err();
        assert!(format!("{err:#}").contains("20 bytes, want 32"));
    }
}
