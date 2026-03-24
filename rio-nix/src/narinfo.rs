//! Narinfo text format parser and generator.
//!
//! The narinfo format is used by Nix binary cache HTTP servers. Each `.narinfo`
//! file contains metadata about a store path:
//!
//! ```text
//! StorePath: /nix/store/abc...-hello
//! URL: nar/abc123.nar.zst
//! Compression: zstd
//! NarHash: sha256:abc123...
//! NarSize: 12345
//! References: dep1-hash-name dep2-hash-name
//! Deriver: xyz...-hello.drv
//! Sig: cache.example.com-1:base64sig...
//! CA: fixed:sha256:abc123...
//! ```
//!
//! Field order is not significant. `References` uses space-separated
//! basenames (not full store paths). Multiple `Sig:` lines are allowed.

use std::num::ParseIntError;

use thiserror::Error;

/// Errors from narinfo parsing.
#[derive(Debug, Error)]
pub enum NarInfoError {
    #[error("missing required field: {0}")]
    MissingField(&'static str),

    #[error("invalid NarSize value {value:?}: {source}")]
    InvalidNarSize {
        value: String,
        #[source]
        source: ParseIntError,
    },

    #[error("invalid FileSize value {value:?}: {source}")]
    InvalidFileSize {
        value: String,
        #[source]
        source: ParseIntError,
    },

    #[error("duplicate field: {0}")]
    DuplicateField(&'static str),
}

/// Parsed narinfo metadata.
///
/// All fields use raw string values as they appear in the narinfo text.
/// Hash parsing and store path validation are deferred to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NarInfo {
    /// Full store path (e.g., `/nix/store/abc...-hello`).
    pub store_path: String,
    /// URL to the NAR file (e.g., `nar/abc123.nar.zst`).
    pub url: String,
    /// Compression method (e.g., `zstd`, `xz`, `none`).
    pub compression: String,
    /// NAR hash (e.g., `sha256:abc123...`).
    pub nar_hash: String,
    /// NAR size in bytes.
    pub nar_size: u64,
    /// Referenced store path basenames, space-separated in text form.
    pub references: Vec<String>,
    /// Deriver basename (e.g., `xyz...-hello.drv`).
    pub deriver: Option<String>,
    /// Cryptographic signatures.
    pub sigs: Vec<String>,
    /// Content address (e.g., `fixed:sha256:abc...`).
    pub ca: Option<String>,
    /// Compressed file hash (e.g., `sha256:def...`).
    pub file_hash: Option<String>,
    /// Compressed file size in bytes.
    pub file_size: Option<u64>,
}

impl NarInfo {
    /// Parse a narinfo text blob.
    pub fn parse(text: &str) -> Result<Self, NarInfoError> {
        let mut store_path = None;
        let mut url = None;
        let mut compression = None;
        let mut nar_hash = None;
        let mut nar_size = None;
        let mut references = Vec::new();
        let mut deriver = None;
        let mut sigs = Vec::new();
        let mut ca = None;
        let mut file_hash = None;
        let mut file_size = None;
        let mut refs_seen = false;

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();

            match key {
                "StorePath" => {
                    if store_path.is_some() {
                        return Err(NarInfoError::DuplicateField("StorePath"));
                    }
                    store_path = Some(value.to_string());
                }
                "URL" => {
                    if url.is_some() {
                        return Err(NarInfoError::DuplicateField("URL"));
                    }
                    url = Some(value.to_string());
                }
                "Compression" => {
                    if compression.is_some() {
                        return Err(NarInfoError::DuplicateField("Compression"));
                    }
                    compression = Some(value.to_string());
                }
                "NarHash" => {
                    if nar_hash.is_some() {
                        return Err(NarInfoError::DuplicateField("NarHash"));
                    }
                    nar_hash = Some(value.to_string());
                }
                "NarSize" => {
                    if nar_size.is_some() {
                        return Err(NarInfoError::DuplicateField("NarSize"));
                    }
                    nar_size =
                        Some(
                            value
                                .parse::<u64>()
                                .map_err(|e| NarInfoError::InvalidNarSize {
                                    value: value.to_string(),
                                    source: e,
                                })?,
                        );
                }
                "References" => {
                    if refs_seen {
                        return Err(NarInfoError::DuplicateField("References"));
                    }
                    refs_seen = true;
                    if !value.is_empty() {
                        references = value.split_whitespace().map(String::from).collect();
                    }
                }
                "Deriver" => {
                    if deriver.is_some() {
                        return Err(NarInfoError::DuplicateField("Deriver"));
                    }
                    deriver = Some(value.to_string());
                }
                "Sig" => sigs.push(value.to_string()),
                "CA" => {
                    if ca.is_some() {
                        return Err(NarInfoError::DuplicateField("CA"));
                    }
                    ca = Some(value.to_string());
                }
                "FileHash" => {
                    if file_hash.is_some() {
                        return Err(NarInfoError::DuplicateField("FileHash"));
                    }
                    file_hash = Some(value.to_string());
                }
                "FileSize" => {
                    if file_size.is_some() {
                        return Err(NarInfoError::DuplicateField("FileSize"));
                    }
                    file_size =
                        Some(
                            value
                                .parse::<u64>()
                                .map_err(|e| NarInfoError::InvalidFileSize {
                                    value: value.to_string(),
                                    source: e,
                                })?,
                        );
                }
                _ => {} // Ignore unknown fields for forward compatibility
            }
        }

        Ok(NarInfo {
            store_path: store_path.ok_or(NarInfoError::MissingField("StorePath"))?,
            url: url.ok_or(NarInfoError::MissingField("URL"))?,
            compression: compression.ok_or(NarInfoError::MissingField("Compression"))?,
            nar_hash: nar_hash.ok_or(NarInfoError::MissingField("NarHash"))?,
            nar_size: nar_size.ok_or(NarInfoError::MissingField("NarSize"))?,
            references,
            deriver,
            sigs,
            ca,
            file_hash,
            file_size,
        })
    }

    /// Serialize to narinfo text format.
    pub fn serialize(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();

        writeln!(out, "StorePath: {}", self.store_path).unwrap();
        writeln!(out, "URL: {}", self.url).unwrap();
        writeln!(out, "Compression: {}", self.compression).unwrap();
        if let Some(ref fh) = self.file_hash {
            writeln!(out, "FileHash: {fh}").unwrap();
        }
        if let Some(fs) = self.file_size {
            writeln!(out, "FileSize: {fs}").unwrap();
        }
        writeln!(out, "NarHash: {}", self.nar_hash).unwrap();
        writeln!(out, "NarSize: {}", self.nar_size).unwrap();

        if self.references.is_empty() {
            writeln!(out, "References:").unwrap();
        } else {
            writeln!(out, "References: {}", self.references.join(" ")).unwrap();
        }

        if let Some(ref d) = self.deriver {
            writeln!(out, "Deriver: {d}").unwrap();
        }

        for sig in &self.sigs {
            writeln!(out, "Sig: {sig}").unwrap();
        }

        if let Some(ref c) = self.ca {
            writeln!(out, "CA: {c}").unwrap();
        }

        out
    }
}

/// Compute the narinfo signing fingerprint.
///
/// This is the canonical string that ed25519 signatures cover. A client
/// verifying a narinfo `Sig:` line reconstructs this string from the
/// narinfo fields, verifies the signature against it, and trusts the
/// path iff a signature from a key in `trusted-public-keys` validates.
///
/// # Format
///
/// `"1;{store_path};{nar_hash};{nar_size};{refs}"` where:
/// - `1` is the fingerprint version (only version Nix has)
/// - `store_path` is the full `/nix/store/...` path
/// - `nar_hash` is `sha256:{nixbase32}` (52-char nixbase32 of the raw
///   32-byte SHA-256 — NOT hex, NOT SRI base64)
/// - `nar_size` is the decimal byte count
/// - `refs` is comma-joined FULL store paths, sorted lexicographically
///   (not basenames — the narinfo text format uses basenames, but the
///   fingerprint uses full paths)
///
/// # Nix reference
///
/// Matches `ValidPathInfo::fingerprint()` in Nix's `path-info.cc`.
/// Getting this byte-for-byte right is load-bearing: one wrong
/// separator or encoding and EVERY signature we produce is invalid.
///
/// # Why a free function, not a method on NarInfo
///
/// The store's signer calls this with fields from `ValidatedPathInfo` (the
/// store's internal type), not from a `NarInfo` struct. Making it a
/// method would force constructing a NarInfo just to sign — wasteful
/// and backwards (we sign BEFORE building the narinfo text).
pub fn fingerprint(
    store_path: &str,
    nar_hash_sha256: &[u8; 32],
    nar_size: u64,
    references: &[String],
) -> String {
    use crate::store_path::nixbase32;

    // nixbase32-encode the raw SHA-256 bytes. 32 bytes → 52 chars.
    // Nix's printHash32 does exactly this; the `sha256:` prefix is
    // the type tag (same as narinfo's NarHash field).
    let hash_colon = format!("sha256:{}", nixbase32::encode(nar_hash_sha256));

    // Sort refs lexicographically. Nix's fingerprint() does this
    // internally (it uses a BTreeSet). We take a slice so the caller
    // doesn't have to pre-sort; we sort a clone.
    //
    // Full paths, not basenames. The narinfo TEXT format uses basenames
    // (saves space, store dir is implicit), but the fingerprint uses
    // full paths. Easy to get wrong if you've been staring at narinfo
    // text.
    let mut sorted_refs: Vec<&str> = references.iter().map(String::as_str).collect();
    sorted_refs.sort_unstable();
    let refs_joined = sorted_refs.join(",");

    format!("1;{store_path};{hash_colon};{nar_size};{refs_joined}")
}

/// Builder for constructing [`NarInfo`] values.
pub struct NarInfoBuilder {
    store_path: String,
    url: String,
    compression: String,
    nar_hash: String,
    nar_size: u64,
    references: Vec<String>,
    deriver: Option<String>,
    sigs: Vec<String>,
    ca: Option<String>,
    file_hash: Option<String>,
    file_size: Option<u64>,
}

impl NarInfoBuilder {
    /// Create a builder with the required fields.
    pub fn new(
        store_path: impl Into<String>,
        url: impl Into<String>,
        compression: impl Into<String>,
        nar_hash: impl Into<String>,
        nar_size: u64,
    ) -> Self {
        NarInfoBuilder {
            store_path: store_path.into(),
            url: url.into(),
            compression: compression.into(),
            nar_hash: nar_hash.into(),
            nar_size,
            references: Vec::new(),
            deriver: None,
            sigs: Vec::new(),
            ca: None,
            file_hash: None,
            file_size: None,
        }
    }

    /// Set the references (store path basenames).
    pub fn references(mut self, refs: Vec<String>) -> Self {
        self.references = refs;
        self
    }

    /// Set the deriver basename.
    pub fn deriver(mut self, deriver: impl Into<String>) -> Self {
        self.deriver = Some(deriver.into());
        self
    }

    /// Add a signature.
    pub fn sig(mut self, sig: impl Into<String>) -> Self {
        self.sigs.push(sig.into());
        self
    }

    /// Set the content address.
    pub fn ca(mut self, ca: impl Into<String>) -> Self {
        self.ca = Some(ca.into());
        self
    }

    /// Set the compressed file hash.
    pub fn file_hash(mut self, file_hash: impl Into<String>) -> Self {
        self.file_hash = Some(file_hash.into());
        self
    }

    /// Set the compressed file size.
    pub fn file_size(mut self, file_size: u64) -> Self {
        self.file_size = Some(file_size);
        self
    }

    /// Build the [`NarInfo`], validating that required fields are non-empty.
    pub fn build(self) -> Result<NarInfo, NarInfoError> {
        if self.store_path.is_empty() {
            return Err(NarInfoError::MissingField("StorePath"));
        }
        if self.url.is_empty() {
            return Err(NarInfoError::MissingField("URL"));
        }
        if self.compression.is_empty() {
            return Err(NarInfoError::MissingField("Compression"));
        }
        if self.nar_hash.is_empty() {
            return Err(NarInfoError::MissingField("NarHash"));
        }
        Ok(NarInfo {
            store_path: self.store_path,
            url: self.url,
            compression: self.compression,
            nar_hash: self.nar_hash,
            nar_size: self.nar_size,
            references: self.references,
            deriver: self.deriver,
            sigs: self.sigs,
            ca: self.ca,
            file_hash: self.file_hash,
            file_size: self.file_size,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // fingerprint()
    // ========================================================================

    /// Exact-match test vector. This fingerprint string IS the contract
    /// with Nix — if it changes, every signature we've ever produced
    /// becomes invalid. The expected string below was cross-checked
    /// against Nix's `ValidPathInfo::fingerprint()` (path-info.cc).
    #[test]
    fn fingerprint_exact() {
        let store_path = "/nix/store/00000000000000000000000000000000-foo";
        // All-zero hash: nixbase32 of 32 zero bytes is 52 zero-digit chars.
        // (nixbase32 alphabet starts with '0', and all-zero input encodes
        // to all-first-alphabet-char output.)
        let hash = [0u8; 32];
        let refs = vec![
            "/nix/store/11111111111111111111111111111111-dep-b".to_string(),
            "/nix/store/22222222222222222222222222222222-dep-a".to_string(),
        ];

        let fp = fingerprint(store_path, &hash, 12345, &refs);

        // Refs are sorted lexicographically. "11...-dep-b" sorts before
        // "22...-dep-a" (the HASH part, not the name, determines order
        // for full paths).
        let expected = format!(
            "1;{};sha256:{};12345;{},{}",
            store_path,
            "0".repeat(52),
            "/nix/store/11111111111111111111111111111111-dep-b",
            "/nix/store/22222222222222222222222222222222-dep-a",
        );
        assert_eq!(fp, expected);
    }

    #[test]
    fn fingerprint_no_refs() {
        // Empty refs → empty string after the last semicolon.
        // NOT ";," or ";{}" — just ";". Easy to get wrong with a
        // join that doesn't handle empty input.
        let fp = fingerprint(
            "/nix/store/00000000000000000000000000000000-x",
            &[0xAB; 32],
            1,
            &[],
        );
        assert!(fp.ends_with(";1;"), "empty refs should end ';size;': {fp}");
        // And there's exactly one trailing semicolon region.
        assert_eq!(fp.matches(';').count(), 4, "exactly 4 semicolons");
    }

    #[test]
    fn fingerprint_sorts_refs() {
        // Caller passes unsorted; we sort. Nix uses BTreeSet internally.
        let refs = vec![
            "/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-z".to_string(),
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a".to_string(),
            "/nix/store/mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm-m".to_string(),
        ];
        let fp = fingerprint(
            "/nix/store/00000000000000000000000000000000-x",
            &[0; 32],
            1,
            &refs,
        );

        // Extract the refs part (after the 4th semicolon).
        let refs_part = fp.rsplit_once(';').unwrap().1;
        assert_eq!(
            refs_part,
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a,\
             /nix/store/mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm-m,\
             /nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-z"
        );
    }

    #[test]
    fn fingerprint_hash_is_nixbase32_not_hex() {
        // The single most likely bug: using hex instead of nixbase32.
        // hex(00..00) = 64 '0' chars. nixbase32(00..00) = 52 '0' chars.
        // The first-alphabet-char happens to be '0' for both, so count
        // the length to distinguish.
        let fp = fingerprint(
            "/nix/store/00000000000000000000000000000000-x",
            &[0; 32],
            1,
            &[],
        );

        // Find the "sha256:..." part.
        let hash_part = fp
            .split(';')
            .find(|s| s.starts_with("sha256:"))
            .unwrap()
            .strip_prefix("sha256:")
            .unwrap();

        assert_eq!(
            hash_part.len(),
            52,
            "hash must be nixbase32 (52 chars for SHA-256), not hex (64 chars): {hash_part}"
        );
    }

    /// Non-zero hash check: verify the nixbase32 encoding is the SAME
    /// one Nix uses (least-significant-digit-first, custom alphabet).
    /// A different base32 variant would produce 52 chars too but
    /// different content.
    #[test]
    fn fingerprint_hash_encoding_matches_nix() {
        use crate::store_path::nixbase32;

        // SHA-256 of "hello" — a well-known test vector.
        // hex: 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        let hash: [u8; 32] =
            hex::decode("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
                .unwrap()
                .try_into()
                .unwrap();

        let fp = fingerprint(
            "/nix/store/00000000000000000000000000000000-x",
            &hash,
            1,
            &[],
        );

        // The hash in the fingerprint must match what nixbase32::encode
        // produces independently. This is a tautology NOW (fingerprint
        // calls nixbase32::encode) but catches future refactors that
        // swap in a different encoder.
        let expected_hash = nixbase32::encode(&hash);
        assert!(
            fp.contains(&format!("sha256:{expected_hash}")),
            "fingerprint should contain nixbase32-encoded hash: {fp}"
        );
    }

    #[test]
    fn parse_with_ca() -> anyhow::Result<()> {
        let text = "\
StorePath: /nix/store/abc-test
URL: nar/abc.nar.zst
Compression: zstd
NarHash: sha256:0000
NarSize: 100
References:
CA: fixed:sha256:abcdef
";
        let info = NarInfo::parse(text)?;
        assert_eq!(info.ca.as_deref(), Some("fixed:sha256:abcdef"));
        Ok(())
    }

    #[test]
    fn parse_multiple_sigs() -> anyhow::Result<()> {
        let text = "\
StorePath: /nix/store/abc-test
URL: nar/abc.nar.zst
Compression: zstd
NarHash: sha256:0000
NarSize: 100
References:
Sig: key1:sig1
Sig: key2:sig2
";
        let info = NarInfo::parse(text)?;
        assert_eq!(info.sigs.len(), 2);
        assert_eq!(info.sigs[0], "key1:sig1");
        assert_eq!(info.sigs[1], "key2:sig2");
        Ok(())
    }

    #[test]
    fn parse_with_file_hash_and_size() -> anyhow::Result<()> {
        let text = "\
StorePath: /nix/store/abc-test
URL: nar/abc.nar.zst
Compression: zstd
FileHash: sha256:deadbeef
FileSize: 5000
NarHash: sha256:0000
NarSize: 10000
References:
";
        let info = NarInfo::parse(text)?;
        assert_eq!(info.file_hash.as_deref(), Some("sha256:deadbeef"));
        assert_eq!(info.file_size, Some(5000));
        Ok(())
    }

    #[test]
    fn missing_store_path() {
        let text = "\
URL: nar/abc.nar.zst
Compression: zstd
NarHash: sha256:0000
NarSize: 100
References:
";
        assert!(matches!(
            NarInfo::parse(text),
            Err(NarInfoError::MissingField("StorePath"))
        ));
    }

    #[test]
    fn missing_nar_hash() {
        let text = "\
StorePath: /nix/store/abc-test
URL: nar/abc.nar.zst
Compression: zstd
NarSize: 100
References:
";
        assert!(matches!(
            NarInfo::parse(text),
            Err(NarInfoError::MissingField("NarHash"))
        ));
    }

    #[test]
    fn invalid_nar_size() {
        let text = "\
StorePath: /nix/store/abc-test
URL: nar/abc.nar.zst
Compression: zstd
NarHash: sha256:0000
NarSize: not_a_number
References:
";
        assert!(matches!(
            NarInfo::parse(text),
            Err(NarInfoError::InvalidNarSize { .. })
        ));
    }

    #[test]
    fn duplicate_field() {
        let text = "\
StorePath: /nix/store/abc-test
StorePath: /nix/store/def-test
URL: nar/abc.nar.zst
Compression: zstd
NarHash: sha256:0000
NarSize: 100
References:
";
        assert!(matches!(
            NarInfo::parse(text),
            Err(NarInfoError::DuplicateField(_))
        ));
    }

    #[test]
    fn duplicate_deriver() {
        let text = "\
StorePath: /nix/store/abc-test
URL: nar/abc.nar.zst
Compression: zstd
NarHash: sha256:0000
NarSize: 100
References:
Deriver: first.drv
Deriver: second.drv
";
        assert!(matches!(
            NarInfo::parse(text),
            Err(NarInfoError::DuplicateField(_))
        ));
    }

    #[test]
    fn builder_constructs_valid_narinfo() -> anyhow::Result<()> {
        let info = NarInfoBuilder::new(
            "/nix/store/abc-test",
            "nar/abc.nar.zst",
            "zstd",
            "sha256:0000",
            100,
        )
        .references(vec!["abc-dep".to_string()])
        .deriver("xyz-test.drv")
        .sig("key:sig")
        .ca("fixed:sha256:beef")
        .build()?;

        assert_eq!(info.store_path, "/nix/store/abc-test");
        assert_eq!(info.references, ["abc-dep"]);
        assert_eq!(info.deriver.as_deref(), Some("xyz-test.drv"));
        assert_eq!(info.sigs, ["key:sig"]);
        assert_eq!(info.ca.as_deref(), Some("fixed:sha256:beef"));

        // Verify it roundtrips
        let serialized = info.serialize();
        let reparsed = NarInfo::parse(&serialized)?;
        assert_eq!(info, reparsed);
        Ok(())
    }

    #[test]
    fn serialize_omits_empty_optional_fields() -> anyhow::Result<()> {
        let info = NarInfoBuilder::new(
            "/nix/store/abc-test",
            "nar/abc.nar.zst",
            "none",
            "sha256:0000",
            100,
        )
        .build()?;

        let text = info.serialize();
        assert!(!text.contains("Deriver:"));
        assert!(!text.contains("Sig:"));
        assert!(!text.contains("CA:"));
        assert!(!text.contains("FileHash:"));
        assert!(!text.contains("FileSize:"));
        Ok(())
    }

    #[test]
    fn test_parse_minimal_narinfo() -> anyhow::Result<()> {
        let text = "\
StorePath: /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-test
URL: nar/test.nar.zst
Compression: zstd
NarHash: sha256:abc123
NarSize: 12345
";
        let info = NarInfo::parse(text)?;

        assert_eq!(
            info.store_path,
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-test"
        );
        assert_eq!(info.url, "nar/test.nar.zst");
        assert_eq!(info.compression, "zstd");
        assert_eq!(info.nar_hash, "sha256:abc123");
        assert_eq!(info.nar_size, 12345);
        assert!(info.references.is_empty());
        assert!(info.deriver.is_none());
        assert!(info.sigs.is_empty());
        assert!(info.ca.is_none());
        assert!(info.file_hash.is_none());
        assert!(info.file_size.is_none());
        Ok(())
    }

    #[test]
    fn test_parse_full_narinfo() -> anyhow::Result<()> {
        let text = "\
StorePath: /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.2
URL: nar/full-test.nar.zst
Compression: xz
NarHash: sha256:deadbeef0123456789abcdef
NarSize: 999999
References: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.2 bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-glibc-2.42
Deriver: cccccccccccccccccccccccccccccccc-hello-2.12.2.drv
Sig: cache.example.com-1:base64signaturedata==
CA: fixed:sha256:cafebabe
FileHash: sha256:f00dcafe
FileSize: 54321
";
        let info = NarInfo::parse(text)?;

        assert_eq!(
            info.store_path,
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.2"
        );
        assert_eq!(info.url, "nar/full-test.nar.zst");
        assert_eq!(info.compression, "xz");
        assert_eq!(info.nar_hash, "sha256:deadbeef0123456789abcdef");
        assert_eq!(info.nar_size, 999999);
        assert_eq!(
            info.references,
            [
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.2",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-glibc-2.42",
            ]
        );
        assert_eq!(
            info.deriver.as_deref(),
            Some("cccccccccccccccccccccccccccccccc-hello-2.12.2.drv")
        );
        assert_eq!(info.sigs, ["cache.example.com-1:base64signaturedata=="]);
        assert_eq!(info.ca.as_deref(), Some("fixed:sha256:cafebabe"));
        assert_eq!(info.file_hash.as_deref(), Some("sha256:f00dcafe"));
        assert_eq!(info.file_size, Some(54321));
        Ok(())
    }

    #[test]
    fn test_roundtrip_narinfo() -> anyhow::Result<()> {
        let original = NarInfoBuilder::new(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-roundtrip",
            "nar/roundtrip.nar.zst",
            "zstd",
            "sha256:abcdef0123456789",
            42000,
        )
        .references(vec![
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-dep1".to_string(),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-dep2".to_string(),
        ])
        .deriver("cccccccccccccccccccccccccccccccc-roundtrip.drv")
        .sig("test-key-1:sigdata1==")
        .sig("test-key-2:sigdata2==")
        .ca("fixed:sha256:cafef00d")
        .file_hash("sha256:compressed123")
        .file_size(8000)
        .build()?;

        let serialized = original.serialize();
        let reparsed = NarInfo::parse(&serialized)?;

        // PartialEq covers all 11 fields.
        assert_eq!(original, reparsed);
        Ok(())
    }

    #[test]
    fn test_duplicate_ca_rejected() {
        let text = "\
StorePath: /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-test
URL: nar/test.nar.zst
Compression: zstd
NarHash: sha256:abc123
NarSize: 12345
CA: fixed:sha256:first
CA: fixed:sha256:second
";
        let err = NarInfo::parse(text).unwrap_err();
        assert!(
            matches!(err, NarInfoError::DuplicateField(f) if f == "CA"),
            "expected DuplicateField(\"CA\"), got: {err:?}"
        );
    }

    #[test]
    fn test_duplicate_file_hash_rejected() {
        let text = "\
StorePath: /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-test
URL: nar/test.nar.zst
Compression: zstd
NarHash: sha256:abc123
NarSize: 12345
FileHash: sha256:first
FileHash: sha256:second
";
        let err = NarInfo::parse(text).unwrap_err();
        assert!(
            matches!(err, NarInfoError::DuplicateField(f) if f == "FileHash"),
            "expected DuplicateField(\"FileHash\"), got: {err:?}"
        );
    }

    #[test]
    fn test_missing_required_fields() {
        // Base narinfo text with all required fields present.
        let base_lines = [
            "StorePath: /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-test",
            "URL: nar/test.nar.zst",
            "Compression: zstd",
            "NarHash: sha256:abc123",
            "NarSize: 12345",
        ];

        let required_fields: [&str; 5] = ["StorePath", "URL", "Compression", "NarHash", "NarSize"];

        for omit in &required_fields {
            // Build text with the target field removed.
            let text: String = base_lines
                .iter()
                .filter(|line| !line.starts_with(omit))
                .map(|line| format!("{line}\n"))
                .collect();

            let err = NarInfo::parse(&text).unwrap_err();
            assert!(
                matches!(err, NarInfoError::MissingField(f) if f == *omit),
                "omitting {omit}: expected MissingField(\"{omit}\"), got: {err:?}"
            );
        }
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(4096))]
            #[test]
            fn narinfo_builder_roundtrip(
                hash_suffix in "[a-z0-9]{32}",
                pkg_name in "[a-z][a-z0-9-]{0,10}",
                url_name in "[a-z0-9]{1,20}",
                compression in prop_oneof!["zstd", "xz", "bzip2", "none"],
                nar_hash_hex in "[a-f0-9]{64}",
                nar_size in any::<u64>(),
                ref_count in 0_usize..4,
                ref_hashes in proptest::collection::vec("[a-z0-9]{32}", 4),
                ref_names in proptest::collection::vec("[a-z][a-z0-9-]{0,8}", 4),
                has_deriver in any::<bool>(),
                drv_hash in "[a-z0-9]{32}",
                drv_name in "[a-z][a-z0-9-]{0,8}",
                sig_count in 0_usize..3,
                sig_keys in proptest::collection::vec("[a-z.-]{1,15}", 3),
                sig_datas in proptest::collection::vec("[a-zA-Z0-9+/]{8,20}={0,2}", 3),
                has_ca in any::<bool>(),
                ca_hash in "[a-f0-9]{64}",
                has_file_hash in any::<bool>(),
                file_hash_hex in "[a-f0-9]{64}",
                file_size_val in any::<u64>(),
            ) {
                let store_path = format!("/nix/store/{hash_suffix}-{pkg_name}");
                let url = format!("nar/{url_name}.nar.{compression}");
                let nar_hash = format!("sha256:{nar_hash_hex}");

                let refs: Vec<String> = (0..ref_count)
                    .map(|i| format!("{}-{}", ref_hashes[i], ref_names[i]))
                    .collect();

                let mut builder = NarInfoBuilder::new(
                    &store_path,
                    &url,
                    &compression,
                    &nar_hash,
                    nar_size,
                )
                .references(refs);

                if has_deriver {
                    builder = builder.deriver(format!("{drv_hash}-{drv_name}.drv"));
                }

                for i in 0..sig_count {
                    builder = builder.sig(format!("{}:{}", sig_keys[i], sig_datas[i]));
                }

                if has_ca {
                    builder = builder.ca(format!("fixed:sha256:{ca_hash}"));
                }

                if has_file_hash {
                    builder = builder
                        .file_hash(format!("sha256:{file_hash_hex}"))
                        .file_size(file_size_val);
                }

                let original = builder.build()?;
                let serialized = original.serialize();
                let reparsed = NarInfo::parse(&serialized)?;

                prop_assert_eq!(&original, &reparsed);
            }
        }
    }
}
