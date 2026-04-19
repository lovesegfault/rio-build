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

            // Set-once helper: most fields are `Option<_>` that must appear at
            // most once. The 1-arg form stores `value.to_string()`; the 2-arg
            // form lets NarSize/FileSize parse first.
            macro_rules! once {
                ($field:ident, $name:literal) => {
                    once!($field, $name, value.to_string())
                };
                ($field:ident, $name:literal, $val:expr) => {{
                    if $field.is_some() {
                        return Err(NarInfoError::DuplicateField($name));
                    }
                    $field = Some($val);
                }};
            }

            match key {
                "StorePath" => once!(store_path, "StorePath"),
                "URL" => once!(url, "URL"),
                "Compression" => once!(compression, "Compression"),
                "NarHash" => once!(nar_hash, "NarHash"),
                "NarSize" => once!(
                    nar_size,
                    "NarSize",
                    value
                        .parse::<u64>()
                        .map_err(|e| NarInfoError::InvalidNarSize {
                            value: value.to_string(),
                            source: e,
                        })?
                ),
                "References" => {
                    if refs_seen {
                        return Err(NarInfoError::DuplicateField("References"));
                    }
                    refs_seen = true;
                    if !value.is_empty() {
                        references = value.split_whitespace().map(String::from).collect();
                    }
                }
                "Deriver" => once!(deriver, "Deriver"),
                "Sig" => sigs.push(value.to_string()),
                "CA" => once!(ca, "CA"),
                "FileHash" => once!(file_hash, "FileHash"),
                "FileSize" => once!(
                    file_size,
                    "FileSize",
                    value
                        .parse::<u64>()
                        .map_err(|e| NarInfoError::InvalidFileSize {
                            value: value.to_string(),
                            source: e,
                        })?
                ),
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

    /// Verify that at least one `Sig:` entry is signed by a trusted key.
    ///
    /// `trusted_keys` is a slice of `name:base64(pubkey)` strings —
    /// same format as Nix's `trusted-public-keys` setting and the
    /// `tenant_upstreams.trusted_keys` column. Returns the name of
    /// the first matching trusted key, or `None` if no signature
    /// verifies.
    ///
    /// The fingerprint reconstruction uses `self.nar_hash` verbatim
    /// (already `sha256:nixbase32` from the `NarHash:` line) and
    /// prepends the store dir (derived from `self.store_path`) to
    /// each reference basename — the narinfo text format stores
    /// basenames but the fingerprint signs full paths.
    ///
    /// Malformed inputs (bad base64, wrong key length, unparseable
    /// sig) are treated as non-matching, not errors: an attacker
    /// who can inject a malformed `Sig:` line shouldn't be able to
    /// make verification crash — they just don't verify.
    // r[impl store.signing.fingerprint]
    // r[impl nix.narinfo.verify-sig]
    pub fn verify_sig(&self, trusted_keys: &[String]) -> Option<String> {
        use base64::Engine as _;
        use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};

        let b64 = base64::engine::general_purpose::STANDARD;

        // Parse trusted_keys into (name, VerifyingKey). Skip malformed
        // entries — a typo in one key shouldn't disable the rest.
        let keys: Vec<(&str, VerifyingKey)> = trusted_keys
            .iter()
            .filter_map(|k| {
                let (name, pk_b64) = k.split_once(':')?;
                let pk_bytes: [u8; 32] = b64.decode(pk_b64).ok()?.try_into().ok()?;
                Some((name, VerifyingKey::from_bytes(&pk_bytes).ok()?))
            })
            .collect();
        if keys.is_empty() {
            return None;
        }

        // Reconstruct the fingerprint from narinfo fields. Can't call
        // the free `fingerprint()` — that wants raw [u8; 32] hash
        // bytes, but `self.nar_hash` is the already-encoded
        // `sha256:nixbase32` string. Rebuilding from the string is
        // correct (and what a Nix client does).
        //
        // Store dir: everything up to and including the last '/' of
        // store_path. E.g. "/nix/store/abc-foo" → "/nix/store/".
        // references are basenames → prepend store_dir for full paths.
        let store_dir = &self.store_path[..=self.store_path.rfind('/')?];
        let mut full_refs: Vec<String> = self
            .references
            .iter()
            .map(|r| format!("{store_dir}{r}"))
            .collect();
        full_refs.sort_unstable();
        full_refs.dedup();
        let fp = format!(
            "1;{};{};{};{}",
            self.store_path,
            self.nar_hash,
            self.nar_size,
            full_refs.join(","),
        );

        // For each Sig entry, find a trusted key with matching name
        // and verify. First success wins.
        for sig in &self.sigs {
            let Some((sig_name, sig_b64)) = sig.split_once(':') else {
                continue;
            };
            let Some((_, vk)) = keys.iter().find(|(n, _)| *n == sig_name) else {
                continue;
            };
            let Ok(sig_bytes) = b64.decode(sig_b64) else {
                continue;
            };
            let Ok(sig_arr): Result<[u8; 64], _> = sig_bytes.try_into() else {
                continue;
            };
            if vk
                .verify(fp.as_bytes(), &Signature::from_bytes(&sig_arr))
                .is_ok()
            {
                return Some(sig_name.to_string());
            }
        }
        None
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
    sorted_refs.dedup();
    let refs_joined = sorted_refs.join(",");

    format!("1;{store_path};{hash_colon};{nar_size};{refs_joined}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

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

    /// Nix uses a BTreeSet → sorted AND deduped. An untrusted worker
    /// (proto `repeated string`) can send `[A, A, B]`; rio must sign the
    /// same fingerprint a Nix client recomputes from its `StorePathSet`.
    #[test]
    fn fingerprint_dedups_references() {
        let a = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a".to_string();
        let b = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b".to_string();
        let p = "/nix/store/00000000000000000000000000000000-x";
        let h = [0u8; 32];

        let with_dup = fingerprint(p, &h, 1, &[a.clone(), a.clone(), b.clone()]);
        let without = fingerprint(p, &h, 1, &[a, b]);
        assert_eq!(with_dup, without);
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

    /// All set-once fields reject a second occurrence with
    /// `DuplicateField(<name>)`. Builds a valid base text then appends a
    /// second copy of one line.
    #[rstest]
    #[case("StorePath", "StorePath: /nix/store/def-test")]
    #[case("Deriver", "Deriver: second.drv")]
    #[case("CA", "CA: fixed:sha256:second")]
    #[case("FileHash", "FileHash: sha256:second")]
    fn duplicate_field_rejected(#[case] field: &str, #[case] dup_line: &str) {
        let text = format!(
            "StorePath: /nix/store/abc-test\n\
             URL: nar/abc.nar.zst\n\
             Compression: zstd\n\
             NarHash: sha256:0000\n\
             NarSize: 100\n\
             References:\n\
             Deriver: first.drv\n\
             CA: fixed:sha256:first\n\
             FileHash: sha256:first\n\
             {dup_line}\n"
        );
        let err = NarInfo::parse(&text).unwrap_err();
        assert!(
            matches!(err, NarInfoError::DuplicateField(f) if f == field),
            "expected DuplicateField({field:?}), got: {err:?}"
        );
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

    // ========================================================================
    // verify_sig()
    // ========================================================================

    /// Test fixture: generate a keypair, sign a narinfo fingerprint,
    /// return (narinfo, trusted_key_entry). Sig uses the same format
    /// nix-store --generate-binary-cache-key + nix store sign produces.
    fn signed_narinfo(key_name: &str, seed: [u8; 32]) -> (NarInfo, String) {
        use base64::Engine as _;
        use ed25519_dalek::{Signer as _, SigningKey};

        let b64 = base64::engine::general_purpose::STANDARD;
        let sk = SigningKey::from_bytes(&seed);
        let pk = sk.verifying_key();

        let store_path = "/nix/store/00000000000000000000000000000000-hello";
        // nixbase32 of all-zero SHA-256 → 52 '0' chars (see fingerprint_exact).
        let nar_hash = format!("sha256:{}", "0".repeat(52));
        let refs = vec!["11111111111111111111111111111111-dep".to_string()];

        // Fingerprint: full paths (store_dir prefix), sorted, comma-joined.
        let fp = format!(
            "1;{store_path};{nar_hash};1234;/nix/store/11111111111111111111111111111111-dep"
        );
        let sig = sk.sign(fp.as_bytes());
        let sig_str = format!("{key_name}:{}", b64.encode(sig.to_bytes()));
        let trusted = format!("{key_name}:{}", b64.encode(pk.to_bytes()));

        let ni = NarInfo {
            store_path: store_path.into(),
            url: "nar/x.nar.zst".into(),
            compression: "zstd".into(),
            nar_hash,
            nar_size: 1234,
            references: refs,
            deriver: None,
            sigs: vec![sig_str],
            ca: None,
            file_hash: None,
            file_size: None,
        };
        (ni, trusted)
    }

    // r[verify store.signing.fingerprint]
    #[test]
    fn verify_sig_accepts_valid() {
        let (ni, trusted) = signed_narinfo("test-key-1", [7u8; 32]);
        assert_eq!(
            ni.verify_sig(&[trusted]).as_deref(),
            Some("test-key-1"),
            "valid sig from trusted key should verify"
        );
    }

    #[test]
    fn verify_sig_rejects_tampered_nar_size() {
        let (mut ni, trusted) = signed_narinfo("test-key-1", [7u8; 32]);
        ni.nar_size = 9999; // tamper
        assert_eq!(
            ni.verify_sig(&[trusted]),
            None,
            "tampered narinfo must not verify"
        );
    }

    #[test]
    fn verify_sig_rejects_tampered_references() {
        let (mut ni, trusted) = signed_narinfo("test-key-1", [7u8; 32]);
        ni.references.push("evil-injected-ref".into());
        assert_eq!(ni.verify_sig(&[trusted]), None);
    }

    /// A narinfo carrying duplicate references must verify identically to
    /// the deduped form — Nix's `StorePathSet` collapses duplicates before
    /// fingerprinting.
    #[test]
    fn verify_sig_dedups_references() {
        let (mut ni, trusted) = signed_narinfo("test-key-1", [7u8; 32]);
        // Fixture signs over a single dep ref. Duplicating it must NOT
        // change the recomputed fingerprint.
        ni.references
            .push("11111111111111111111111111111111-dep".into());
        assert_eq!(
            ni.verify_sig(&[trusted]).as_deref(),
            Some("test-key-1"),
            "duplicate refs must not invalidate the signature"
        );
    }

    #[test]
    fn verify_sig_rejects_untrusted_key() {
        // Sign with seed A, trust only seed B's pubkey.
        let (ni, _trusted_a) = signed_narinfo("key-a", [1u8; 32]);
        let (_, trusted_b) = signed_narinfo("key-b", [2u8; 32]);
        assert_eq!(
            ni.verify_sig(&[trusted_b]),
            None,
            "sig from untrusted key must not verify"
        );
    }

    #[test]
    fn verify_sig_handles_multiple_sigs_and_keys() {
        // narinfo signed by key-a; trusted list has [key-c, key-a].
        // Should find key-a even though it's not first in trusted list.
        let (ni, trusted_a) = signed_narinfo("key-a", [1u8; 32]);
        let (_, trusted_c) = signed_narinfo("key-c", [3u8; 32]);
        assert_eq!(
            ni.verify_sig(&[trusted_c, trusted_a]).as_deref(),
            Some("key-a")
        );
    }

    #[test]
    fn verify_sig_empty_inputs() {
        let (ni, trusted) = signed_narinfo("k", [0u8; 32]);
        assert_eq!(ni.verify_sig(&[]), None, "empty trusted_keys");

        let mut ni2 = ni.clone();
        ni2.sigs.clear();
        assert_eq!(ni2.verify_sig(&[trusted]), None, "empty sigs");
    }

    #[test]
    fn verify_sig_skips_malformed_entries() {
        let (mut ni, trusted) = signed_narinfo("good-key", [5u8; 32]);
        // Prepend garbage sigs — should be skipped, good one still verifies.
        ni.sigs.insert(0, "no-colon-here".into());
        ni.sigs.insert(0, "bad:not_base64!!".into());
        ni.sigs.insert(0, "short:AAAA".into()); // 3 bytes decoded, not 64
        // Prepend garbage trusted key too.
        let keys = vec!["malformed-key-no-colon".into(), trusted];
        assert_eq!(ni.verify_sig(&keys).as_deref(), Some("good-key"));
    }
}
