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
        algo: &'static str,
        expected: usize,
        got: usize,
    },

    #[error(
        "hash {hash:?} has wrong length {len} for hash algorithm '{algo}': expected {b16} \
         (base16), {b32} (nixbase32), or {b64} (base64) characters"
    )]
    WrongEncodedLength {
        algo: &'static str,
        hash: String,
        len: usize,
        b16: usize,
        b32: usize,
        b64: usize,
    },
}

/// Supported hash algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
// r[impl nix.hash.algos+1]
pub enum HashAlgo {
    SHA256,
    SHA512,
    SHA1,
}

impl std::str::FromStr for HashAlgo {
    type Err = HashError;

    /// Parse an algorithm name CASE-EXACTLY, like the oracle.
    ///
    /// CppNix's `parseHashAlgoOpt` (`hash.cc:468-483`) compares the raw
    /// string against lowercase literals — `"SHA256"` is an error there,
    /// at every site that names an algorithm (SRI prefixes, colon
    /// prefixes, `outputHashAlgo`, content-address methods). A lax
    /// case-folding parse here is how two gates disagree about the same
    /// declaration: this function is the single string→`HashAlgo` parse,
    /// so its case posture IS the system's case posture.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "sha256" => Ok(HashAlgo::SHA256),
            "sha512" => Ok(HashAlgo::SHA512),
            "sha1" => Ok(HashAlgo::SHA1),
            _ => Err(HashError::UnknownAlgorithm(s.to_string())),
        }
    }
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

    /// Length of this algorithm's digest when base16 (lowercase hex) encoded.
    fn base16_len(&self) -> usize {
        self.digest_len() * 2
    }

    /// Length of this algorithm's digest when nixbase32 encoded.
    ///
    /// nixbase32 packs 5 bits per character: `ceil(bits / 5)`.
    fn nixbase32_len(&self) -> usize {
        (self.digest_len() * 8).div_ceil(5)
    }

    /// Length of this algorithm's digest when standard (padded) base64 encoded.
    fn base64_len(&self) -> usize {
        self.digest_len().div_ceil(3) * 4
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

/// A parsed `outputHashAlgo` declaration: the optional `r:` ingestion
/// prefix plus a case-exact algorithm.
///
/// This is THE constructor for FOD/floating-CA algo declarations — the
/// gateway's verifiability screen and declared-hash gate, the
/// scheduler's authoritative-content validation, the worker glue's FOD
/// declaration check, the fetch-verify pipeline, the floating-CA
/// finalizer spec, the modulo-hash fingerprint, and the hashed-mirror
/// env all parse through here, so no two components can read the same
/// declaration differently (a spelling accepted at one gate and
/// rejected at another is unrepresentable).
///
/// Oracle parity (`parseDerivationOutput`, `derivations.cc:306-317`):
/// the oracle first strips one ingestion-method prefix with
/// `ContentAddressMethod::parsePrefix` (`content-address.cc:84-95`) —
/// a case-sensitive `splitPrefix(m, "r:")` — then parses the REMAINDER
/// with case-exact `parseHashAlgo` (`hash.cc:468-490`). The `git:` and
/// `text:` method prefixes are experimental-feature-gated upstream
/// (`GitHashing` / `DynamicDerivations`); rio's feature posture is
/// fixed off, so both fall through to [`HashAlgo::from_str`]'s
/// unknown-algorithm rejection here, matching the xp-disabled oracle.
/// `md5` and `blake3` are accepted by the oracle but not by rio — the
/// registered divergence `nix.divergence.fod-fallback-fingerprint+1`.
// r[impl nix.hash.algos+1]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputHashAlgo {
    /// True for `r:`-prefixed declarations (NAR ingestion — the prefix
    /// selects the hash MODE, not the algorithm).
    pub recursive: bool,
    /// The case-exact algorithm named after the prefix.
    pub algo: HashAlgo,
}

impl OutputHashAlgo {
    /// Parse a raw `outputHashAlgo` string (`"sha256"`, `"r:sha512"`, …).
    ///
    /// Strips at most one case-sensitive `"r:"` prefix, then parses the
    /// remainder case-exactly. See the type docs for the oracle parity
    /// argument.
    pub fn parse(raw: &str) -> Result<Self, HashError> {
        let (recursive, algo_str) = match raw.strip_prefix("r:") {
            Some(rest) => (true, rest),
            None => (false, raw),
        };
        Ok(OutputHashAlgo {
            recursive,
            algo: algo_str.parse()?,
        })
    }
}

/// A Nix hash value (algorithm + digest bytes).
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NixHash {
    algo: HashAlgo,
    digest: Vec<u8>,
}

impl NixHash {
    /// The hash algorithm.
    pub fn algo(&self) -> HashAlgo {
        self.algo
    }

    /// The raw digest bytes.
    pub fn digest(&self) -> &[u8] {
        &self.digest
    }
}

impl NixHash {
    /// Create a new hash from algorithm and raw digest bytes.
    pub fn new(algo: HashAlgo, digest: Vec<u8>) -> Result<Self, HashError> {
        if digest.len() != algo.digest_len() {
            return Err(HashError::WrongDigestLength {
                algo: algo.as_str(),
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

        let algo = algo_str.parse::<HashAlgo>()?;
        let digest = nixbase32::decode(digest_str)?;

        Self::new(algo, digest)
    }

    /// Parse SRI format: `"sha256-AAAA...="` where the digest is base64-encoded.
    // r[impl nix.hash.sri]
    pub fn parse_sri(s: &str) -> Result<Self, HashError> {
        let (algo_str, digest_str) = s
            .split_once('-')
            .ok_or_else(|| HashError::InvalidFormat(format!("missing '-' in {s:?}")))?;

        let algo = algo_str.parse::<HashAlgo>()?;

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

    /// Parse a bare digest string (no algorithm prefix, no SRI separator),
    /// discriminating the encoding — base16, nixbase32, or base64 — by its
    /// length for the given algorithm.
    ///
    /// This is CppNix `Hash::parseNonSRIUnprefixed` (`hash.cc`:
    /// `baseFromSize` + `parseLowLevel`) and is the ONLY decoder for
    /// declared `outputHash` values: a fixed-output derivation may declare
    /// its hash in any of the three encodings and every component must
    /// decode it identically. Within one algorithm the three encoded
    /// lengths are always distinct, so the discrimination is unambiguous.
    ///
    /// NOT for wire `narHash` fields — those are hex-only by protocol
    /// (`gw.wire.narhash-hex`) and keep using `hex::decode` + `NixHash::new`.
    // r[impl nix.hash.fod-decode+1]
    // r[impl nix.divergence.fod-base64-strict]
    pub fn parse_nonsri_unprefixed(algo: HashAlgo, s: &str) -> Result<Self, HashError> {
        let digest = if s.len() == algo.base16_len() {
            hex::decode(s)
                .map_err(|_| HashError::InvalidFormat(format!("hash {s:?} is not valid base16")))?
        } else if s.len() == algo.nixbase32_len() {
            nixbase32::decode(s)?
        } else if s.len() == algo.base64_len() {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(s)
                .map_err(|_| HashError::InvalidBase64)?
        } else {
            return Err(HashError::WrongEncodedLength {
                algo: algo.as_str(),
                hash: s.to_owned(),
                len: s.len(),
                b16: algo.base16_len(),
                b32: algo.nixbase32_len(),
                b64: algo.base64_len(),
            });
        };
        Self::new(algo, digest)
    }

    /// Render as lowercase hex digest (no algorithm prefix).
    ///
    /// This is the format used by nix-daemon on the wire for `wopQueryPathInfo`.
    pub fn to_hex(&self) -> String {
        hex::encode(&self.digest)
    }

    /// Render in Nix colon format: `sha256:aabb...` (nixbase32 digest).
    pub fn to_colon(&self) -> String {
        format!("{}:{}", self.algo, nixbase32::encode(&self.digest))
    }

    /// Render in SRI format: `sha256-AAAA...=` (base64 digest).
    // r[impl nix.hash.sri]
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

        Self::new(algo, digest).expect("crypto library returned wrong digest length")
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
    fn test_algo_parse() -> anyhow::Result<()> {
        assert_eq!("sha256".parse::<HashAlgo>()?, HashAlgo::SHA256);
        assert_eq!("sha512".parse::<HashAlgo>()?, HashAlgo::SHA512);
        assert_eq!("sha1".parse::<HashAlgo>()?, HashAlgo::SHA1);
        assert!("md5".parse::<HashAlgo>().is_err());
        // Case-exact like the oracle's `parseHashAlgoOpt`
        // (`hash.cc:468-483`): a folded spelling is an error, not an
        // alias — otherwise gates that parse here and gates that
        // compare exactly disagree about the same declaration.
        assert!("SHA256".parse::<HashAlgo>().is_err());
        assert!("Sha256".parse::<HashAlgo>().is_err());
        assert!("sHa512".parse::<HashAlgo>().is_err());
        assert!("SHA1".parse::<HashAlgo>().is_err());
        assert!(" sha256".parse::<HashAlgo>().is_err());
        assert!("sha256 ".parse::<HashAlgo>().is_err());
        Ok(())
    }

    /// Spelled-string differential for the algorithm parse: rio's
    /// [`HashAlgo::from_str`] / [`OutputHashAlgo::parse`] vs the
    /// line-by-line `parseHashAlgoOpt` / `ContentAddressMethod::
    /// parsePrefix` port (`hash_oracle.rs`), exhaustively over the case
    /// axis (every case mask of every algorithm name, bare and behind
    /// every method prefix) plus whitespace/affix junk.
    ///
    /// Laws:
    /// 1. rio accepts ⇒ the xp-disabled oracle accepts, with the same
    ///    algorithm and the same recursive bit;
    /// 2. the xp-disabled oracle accepts but rio rejects ⇒ the
    ///    algorithm is exactly md5 — the registered divergence
    ///    `nix.divergence.fod-fallback-fingerprint+1` (blake3 joins only
    ///    under the xp-enabled oracle, asserted separately);
    /// 3. on the case axis specifically: any non-lowercase mask is
    ///    rejected by BOTH sides (a case-fold regression in rio
    ///    violates law 1; one in the port violates law 3).
    // r[verify nix.hash.algos+1]
    // r[verify nix.divergence.fod-fallback-fingerprint+1]
    #[test]
    fn algo_parse_differential_spelled_strings() {
        use crate::hash_oracle::{
            OracleAlgo, parse_hash_algo_oracle, parse_hash_algo_oracle_xp,
            parse_output_hash_algo_oracle,
        };

        fn case_masks(name: &str) -> Vec<String> {
            // Letters only — digits have one form. 2^letters spellings.
            let letters: Vec<usize> = name
                .char_indices()
                .filter(|(_, c)| c.is_ascii_alphabetic())
                .map(|(i, _)| i)
                .collect();
            let mut out = Vec::with_capacity(1 << letters.len());
            for mask in 0u32..(1 << letters.len()) {
                let mut s: Vec<u8> = name.as_bytes().to_vec();
                for (bit, &idx) in letters.iter().enumerate() {
                    if mask & (1 << bit) != 0 {
                        s[idx] = s[idx].to_ascii_uppercase();
                    }
                }
                out.push(String::from_utf8(s).unwrap());
            }
            out
        }

        let mut corpus: Vec<String> = Vec::new();
        for name in ["sha1", "sha256", "sha512", "md5", "blake3"] {
            for spelling in case_masks(name) {
                corpus.push(spelling.clone());
                corpus.push(format!("r:{spelling}"));
                corpus.push(format!("R:{spelling}"));
                corpus.push(format!("git:{spelling}"));
                corpus.push(format!("text:{spelling}"));
                corpus.push(format!("r:r:{spelling}"));
                corpus.push(format!(" {spelling}"));
                corpus.push(format!("{spelling} "));
                corpus.push(format!("{spelling}\n"));
            }
        }
        corpus.extend(
            [
                "", ":", "r:", "sha", "sha2", "sha-256", "sha256:", "ssha256", "sha2566",
            ]
            .map(str::to_owned),
        );

        let to_oracle = |a: HashAlgo| match a {
            HashAlgo::SHA1 => OracleAlgo::Sha1,
            HashAlgo::SHA256 => OracleAlgo::Sha256,
            HashAlgo::SHA512 => OracleAlgo::Sha512,
        };

        for s in &corpus {
            // Bare algorithm names: FromStr vs parseHashAlgoOpt.
            let rio = s.parse::<HashAlgo>();
            let oracle = parse_hash_algo_oracle(s);
            match (&rio, &oracle) {
                (Ok(r), Some(o)) => assert_eq!(to_oracle(*r), *o, "algo disagreement on {s:?}"),
                (Ok(r), None) => panic!("rio accepts {s:?} as {r:?}; the oracle rejects it"),
                (Err(_), Some(o)) => assert_eq!(
                    *o,
                    OracleAlgo::Md5,
                    "{s:?}: oracle-accepts/rio-rejects outside the registered md5 divergence"
                ),
                (Err(_), None) => {}
            }

            // Full outputHashAlgo strings: OutputHashAlgo::parse vs
            // parsePrefix + parseHashAlgoOpt.
            let rio_out = OutputHashAlgo::parse(s);
            let oracle_out = parse_output_hash_algo_oracle(s);
            match (&rio_out, &oracle_out) {
                (Ok(r), Some((rec, o))) => {
                    assert_eq!(r.recursive, *rec, "recursive bit disagreement on {s:?}");
                    assert_eq!(to_oracle(r.algo), *o, "algo disagreement on {s:?}");
                }
                (Ok(r), None) => {
                    panic!("rio accepts outputHashAlgo {s:?} as {r:?}; the oracle rejects it")
                }
                (Err(_), Some((_, o))) => assert_eq!(
                    *o,
                    OracleAlgo::Md5,
                    "{s:?}: oracle-accepts/rio-rejects outside the registered md5 divergence"
                ),
                (Err(_), None) => {}
            }

            // The xp-enabled oracle additionally accepts blake3; the
            // divergence set under THAT oracle is exactly {md5, blake3}.
            if let (Err(_), Some(o)) = (&rio, &parse_hash_algo_oracle_xp(s)) {
                assert!(
                    matches!(o, OracleAlgo::Md5 | OracleAlgo::Blake3),
                    "{s:?}: xp-oracle-accepts/rio-rejects outside {{md5, blake3}}"
                );
            }
        }
    }

    #[test]
    fn output_hash_algo_parse_table() {
        let ok = OutputHashAlgo::parse("sha256").unwrap();
        assert!(!ok.recursive);
        assert_eq!(ok.algo, HashAlgo::SHA256);

        let rec = OutputHashAlgo::parse("r:sha512").unwrap();
        assert!(rec.recursive);
        assert_eq!(rec.algo, HashAlgo::SHA512);

        // One prefix strip only; case-sensitive prefix; xp-gated
        // method prefixes rejected (rio's features are fixed off).
        for bad in [
            "r:r:sha256",
            "R:sha256",
            "git:sha256",
            "text:sha256",
            "r:SHA256",
            "SHA256",
            "",
            "r:",
        ] {
            assert!(
                OutputHashAlgo::parse(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
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
        assert_eq!(hash.algo(), HashAlgo::SHA256);
        assert_eq!(hash.digest().len(), 32);
        // SHA-256 of empty string is well-known
        assert_eq!(
            hex::encode(hash.digest()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_compute_sha1() {
        let hash = NixHash::compute(HashAlgo::SHA1, b"");
        assert_eq!(hash.algo(), HashAlgo::SHA1);
        assert_eq!(hash.digest().len(), 20);
        // SHA-1 of empty string
        assert_eq!(
            hex::encode(hash.digest()),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
    }

    #[test]
    fn test_colon_format_roundtrip() -> anyhow::Result<()> {
        let hash = NixHash::compute(HashAlgo::SHA256, b"hello");
        let colon = hash.to_colon();
        assert!(colon.starts_with("sha256:"));
        let parsed = NixHash::parse_colon(&colon)?;
        assert_eq!(parsed, hash);
        Ok(())
    }

    #[test]
    fn test_sri_format_roundtrip() -> anyhow::Result<()> {
        let hash = NixHash::compute(HashAlgo::SHA256, b"hello");
        let sri = hash.to_sri();
        assert!(sri.starts_with("sha256-"));
        let parsed = NixHash::parse_sri(&sri)?;
        assert_eq!(parsed, hash);
        Ok(())
    }

    #[test]
    fn test_auto_detect_format() -> anyhow::Result<()> {
        let hash = NixHash::compute(HashAlgo::SHA256, b"hello");
        let colon = hash.to_colon();
        let sri = hash.to_sri();

        assert_eq!(NixHash::parse(&colon)?, hash);
        assert_eq!(NixHash::parse(&sri)?, hash);
        Ok(())
    }

    #[test]
    fn test_wrong_digest_length() {
        assert!(NixHash::new(HashAlgo::SHA256, vec![0u8; 31]).is_err());
        assert!(NixHash::new(HashAlgo::SHA256, vec![0u8; 33]).is_err());
        assert!(NixHash::new(HashAlgo::SHA256, vec![0u8; 32]).is_ok());
    }

    /// CppNix `Hash::parseNonSRIUnprefixed` parity: every algorithm accepts
    /// its digest in all three encodings, discriminated by length, and the
    /// three decode to the same digest.
    // r[verify nix.hash.fod-decode+1]
    #[test]
    fn test_parse_nonsri_unprefixed_encoding_matrix() -> anyhow::Result<()> {
        use base64::Engine;
        for algo in [HashAlgo::SHA256, HashAlgo::SHA512, HashAlgo::SHA1] {
            let reference = NixHash::compute(algo, b"rio parity probe");

            let b16 = hex::encode(reference.digest());
            let b32 = nixbase32::encode(reference.digest());
            let b64 = base64::engine::general_purpose::STANDARD.encode(reference.digest());

            // The three encoded lengths are pairwise distinct for every algo,
            // making the length discrimination unambiguous.
            assert_ne!(b16.len(), b32.len());
            assert_ne!(b16.len(), b64.len());
            assert_ne!(b32.len(), b64.len());

            for encoded in [&b16, &b32, &b64] {
                let parsed = NixHash::parse_nonsri_unprefixed(algo, encoded)?;
                assert_eq!(
                    parsed, reference,
                    "{algo} digest must decode identically from {encoded:?}"
                );
            }
        }
        Ok(())
    }

    /// The decode for each discriminated encoding is strict: a string of the
    /// right LENGTH but invalid alphabet for that encoding is rejected, never
    /// reinterpreted as another encoding.
    // r[verify nix.hash.fod-decode+1]
    #[test]
    fn test_parse_nonsri_unprefixed_strict_per_encoding() {
        // 64 chars (sha256 base16 length) but not valid hex.
        let bad_b16 = "zz".repeat(32);
        assert!(matches!(
            NixHash::parse_nonsri_unprefixed(HashAlgo::SHA256, &bad_b16),
            Err(HashError::InvalidFormat(_))
        ));

        // 52 chars (sha256 nixbase32 length) but contains 'e' (not in the
        // nixbase32 alphabet).
        let bad_b32 = "e".repeat(52);
        assert!(NixHash::parse_nonsri_unprefixed(HashAlgo::SHA256, &bad_b32).is_err());

        // 44 chars (sha256 base64 length) but invalid base64 bytes.
        let bad_b64 = "!".repeat(44);
        assert!(matches!(
            NixHash::parse_nonsri_unprefixed(HashAlgo::SHA256, &bad_b64),
            Err(HashError::InvalidBase64)
        ));
    }

    /// A digest whose length matches none of the three encodings for the
    /// algorithm is rejected with the oracle's "wrong length" error shape.
    // r[verify nix.hash.fod-decode+1]
    #[test]
    fn test_parse_nonsri_unprefixed_wrong_length_rejected() {
        for len in [0usize, 1, 43, 45, 51, 53, 63, 65, 129] {
            let s = "a".repeat(len);
            let result = NixHash::parse_nonsri_unprefixed(HashAlgo::SHA256, &s);
            assert!(
                matches!(result, Err(HashError::WrongEncodedLength { .. })),
                "length {len} must be rejected as wrong-length for sha256, got {result:?}"
            );
        }
    }

    /// Round-trip: parse each encoding then re-render canonical hex — the
    /// canonicalization every consumer (modulo fingerprint, hashed-mirror
    /// env) relies on.
    // r[verify nix.hash.fod-decode+1]
    #[test]
    fn test_parse_nonsri_unprefixed_canonical_hex() -> anyhow::Result<()> {
        let reference = NixHash::compute(HashAlgo::SHA256, b"canonical");
        let b32 = nixbase32::encode(reference.digest());
        let parsed = NixHash::parse_nonsri_unprefixed(HashAlgo::SHA256, &b32)?;
        assert_eq!(parsed.to_hex(), hex::encode(reference.digest()));
        Ok(())
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        fn arb_hash_algo() -> impl Strategy<Value = HashAlgo> {
            prop_oneof![
                Just(HashAlgo::SHA256),
                Just(HashAlgo::SHA512),
                Just(HashAlgo::SHA1),
            ]
        }

        pub(super) fn arb_nixhash() -> impl Strategy<Value = NixHash> {
            arb_hash_algo().prop_flat_map(|algo| {
                proptest::collection::vec(any::<u8>(), algo.digest_len()).prop_map(move |digest| {
                    NixHash::new(algo, digest).expect("length matches algo")
                })
            })
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(4096))]

            #[test]
            fn colon_roundtrip(h in arb_nixhash()) {
                let s = h.to_colon();
                let parsed = NixHash::parse_colon(&s)?;
                prop_assert_eq!(parsed, h);
            }

            #[test]
            fn sri_roundtrip(h in arb_nixhash()) {
                let s = h.to_sri();
                let parsed = NixHash::parse_sri(&s)?;
                prop_assert_eq!(parsed, h);
            }

            /// Any digest round-trips through all three non-SRI encodings via
            /// the length-discriminated parser.
            // r[verify nix.hash.fod-decode+1]
            #[test]
            fn nonsri_unprefixed_roundtrip_all_encodings(h in arb_nixhash()) {
                use base64::Engine;
                let encodings = [
                    hex::encode(h.digest()),
                    nixbase32::encode(h.digest()),
                    base64::engine::general_purpose::STANDARD.encode(h.digest()),
                ];
                for s in encodings {
                    let parsed = NixHash::parse_nonsri_unprefixed(h.algo(), &s)?;
                    prop_assert_eq!(&parsed, &h);
                }
            }

            /// Any byte sequence of wrong length must be rejected by NixHash::new.
            #[test]
            fn new_rejects_wrong_length(
                algo in arb_hash_algo(),
                digest in proptest::collection::vec(any::<u8>(), 0..128),
            ) {
                let result = NixHash::new(algo, digest.clone());
                let expect_ok = digest.len() == algo.digest_len();
                // prop_assert! stringifies its arg as a format string, so
                // `{ .. }` in a matches! pattern would be parsed as a format
                // placeholder — bind the bool first.
                let got_ok = result.is_ok();
                let got_wrong_len = matches!(result, Err(HashError::WrongDigestLength { .. }));
                if expect_ok {
                    prop_assert!(got_ok);
                } else {
                    prop_assert!(got_wrong_len);
                }
            }
        }
    }

    /// ADVERSARIAL pins (not oracle-derived — `nix-instantiate` emits
    /// canonical base16 only, so no oracle emitter produces these).
    /// Each input is built at the exact sha256 base64 length (44) so it
    /// reaches the base64 arm, and each pins a divergence from CppNix
    /// `base64::decode` (`base-n.cc:75-112`):
    ///
    /// - trailing-bit garbage: the oracle DISCARDS leftover bits
    ///   (`base-n.cc:103-107`), so `…A=` and `…B=` alias one digest —
    ///   the oracle ACCEPTS the non-canonical spelling, rio rejects it.
    /// - embedded newline: the oracle SKIPS `\n` (`base-n.cc:96-97`),
    ///   shortening the payload; its own length check then rejects, so
    ///   both sides reject — only the error class diverges (rio: base64
    ///   decode error; oracle: BadHash length, with the decoder's
    ///   FormatError swallowed by `parseLowLevel`'s catch,
    ///   `hash.cc:145-166`).
    /// - data after `=`: the oracle STOPS at the first `=`
    ///   (`base-n.cc:94-95`); same both-reject, error-class-only delta.
    // r[verify nix.divergence.fod-base64-strict]
    #[test]
    fn base64_arm_pins_oracle_lax_divergences() {
        use base64::Engine;
        let canonical = base64::engine::general_purpose::STANDARD.encode([0u8; 32]);
        assert_eq!(canonical.len(), 44);
        assert!(canonical.ends_with("AA=")); // 4 unused trailing bits

        // Canonical spelling decodes.
        assert!(NixHash::parse_nonsri_unprefixed(HashAlgo::SHA256, &canonical).is_ok());

        // Trailing-bit garbage: flip the final data symbol 'A'→'B'
        // (low bits 000001 land in the discarded region). Oracle:
        // same digest as canonical. rio: rejected.
        let trailing = format!("{}B=", &canonical[..42]);
        assert!(matches!(
            NixHash::parse_nonsri_unprefixed(HashAlgo::SHA256, &trailing),
            Err(HashError::InvalidBase64)
        ));

        // Embedded newline at base64 length.
        let newline = format!("{}\n{}", &canonical[..20], &canonical[21..]);
        assert_eq!(newline.len(), 44);
        assert!(matches!(
            NixHash::parse_nonsri_unprefixed(HashAlgo::SHA256, &newline),
            Err(HashError::InvalidBase64)
        ));

        // Early '=' with data after it, still 44 chars.
        let early_eq = format!("{}={}", &canonical[..20], &canonical[21..]);
        assert_eq!(early_eq.len(), 44);
        assert!(matches!(
            NixHash::parse_nonsri_unprefixed(HashAlgo::SHA256, &early_eq),
            Err(HashError::InvalidBase64)
        ));
    }

    /// ADVERSARIAL pin: the oracle's `parseHashAlgo` accepts `md5`
    /// (`hash.cc:468-490`); rio's `HashAlgo` does not — an md5
    /// declaration is undecodable here and survives only through the
    /// raw-string fingerprint fallback
    /// (`nix.divergence.fod-fallback-fingerprint+1`).
    // r[verify nix.divergence.fod-fallback-fingerprint+1]
    #[test]
    fn md5_spelling_is_a_registered_divergence() {
        assert!("md5".parse::<HashAlgo>().is_err());
    }

    /// Differential properties against the vendored CppNix decode port
    /// (`hash_oracle.rs`, line-by-line from the pinned 2.34.7 source).
    /// 4096 cases each — the merge-gate evidence for
    /// `nix.divergence.fod-base64-strict`'s containment claim: rio's
    /// accept-set is a SUBSET of the oracle's with identical digests
    /// (fail-closed in exactly one direction), the canonical encodings
    /// are accepted identically by both, and the length-discrimination
    /// arm selection is the same function.
    mod oracle_differential {
        use super::proptests::arb_nixhash;
        use super::*;
        use crate::hash_oracle::{self, OracleReject};
        use proptest::prelude::*;

        fn arb_algo() -> impl Strategy<Value = HashAlgo> {
            prop_oneof![
                Just(HashAlgo::SHA1),
                Just(HashAlgo::SHA256),
                Just(HashAlgo::SHA512),
            ]
        }

        /// Inputs biased toward near-misses: canonical encodings of
        /// random digests, single-character mutations of them, and raw
        /// junk of plausible lengths.
        fn arb_input() -> impl Strategy<Value = (HashAlgo, String)> {
            use base64::Engine;
            (
                arb_algo(),
                proptest::collection::vec(any::<u8>(), 0..70),
                0usize..3,
                any::<u8>(),
                any::<u16>(),
            )
                .prop_map(|(algo, bytes, enc, mutate_to, pos)| {
                    let digest: Vec<u8> = bytes
                        .iter()
                        .copied()
                        .chain(std::iter::repeat(0))
                        .take(algo.digest_len())
                        .collect();
                    let mut s = match enc {
                        0 => hex::encode(&digest),
                        1 => nixbase32::encode(&digest),
                        _ => base64::engine::general_purpose::STANDARD.encode(&digest),
                    };
                    // Half the cases: mutate one byte to an arbitrary
                    // ASCII char (possibly '=', '\n', uppercase, junk).
                    if mutate_to >= 0x80 {
                        let i = (pos as usize) % s.len().max(1);
                        let c = (mutate_to & 0x7f) as char;
                        s.replace_range(i..i + 1, &c.to_string());
                    }
                    (algo, s)
                })
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(4096))]

            /// Containment + length-identity. Named by the
            /// `nix.divergence.fod-base64-strict` rule rationale.
            #[test]
            fn oracle_containment_4096((algo, s) in arb_input()) {
                let rio = NixHash::parse_nonsri_unprefixed(algo, &s);
                let oracle = hash_oracle::parse_nonsri_unprefixed_oracle(algo, &s);
                match (&rio, &oracle) {
                    // Containment: rio accepts ⇒ oracle accepts, same digest.
                    (Ok(h), Ok(d)) => prop_assert_eq!(h.digest(), &d[..]),
                    (Ok(h), Err(rej)) => prop_assert!(
                        false,
                        "rio accepted {:?} (digest {}) but the oracle rejected with {:?}",
                        s, hex::encode(h.digest()), rej
                    ),
                    // Length-discrimination identity: the arm SELECTION
                    // is the same function on both sides.
                    (Err(HashError::WrongEncodedLength { .. }), rej) => {
                        prop_assert_eq!(rej, &Err(OracleReject::WrongLength));
                    }
                    // rio rejected inside a codec: fail-closed strictness
                    // is allowed (that IS the registered divergence), but
                    // the oracle must have selected the same arm — i.e.
                    // not have called it a wrong-length input.
                    (Err(_), rej) => {
                        prop_assert!(
                            !matches!(rej, Err(OracleReject::WrongLength)),
                            "rio decoded an input the oracle length-rejects: {:?}", s
                        );
                    }
                }
            }

            /// Canonical completeness: every digest's three canonical
            /// encodings are accepted by BOTH sides with the digest
            /// round-tripping exactly.
            #[test]
            fn oracle_canonical_completeness_4096(h in arb_nixhash()) {
                use base64::Engine;
                let encodings = [
                    hex::encode(h.digest()),
                    nixbase32::encode(h.digest()),
                    base64::engine::general_purpose::STANDARD.encode(h.digest()),
                ];
                for s in encodings {
                    let rio = NixHash::parse_nonsri_unprefixed(h.algo(), &s);
                    let oracle =
                        hash_oracle::parse_nonsri_unprefixed_oracle(h.algo(), &s);
                    prop_assert_eq!(rio.as_ref().map(|x| x.digest()).ok(), Some(h.digest()));
                    prop_assert_eq!(oracle.as_deref().ok(), Some(h.digest()));
                }
            }
        }
    }
}
