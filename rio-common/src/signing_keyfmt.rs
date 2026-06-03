//! Signing-key entry codec — the single owner of the `name:base64`
//! secret/public byte contract.
//!
//! One module owns parse/derive/encode for the narinfo signing-key
//! entry format so the producer (`rio-cli keygen`), the consumer
//! (`rio-store::signing`), and the bootstrap plumbing
//! (`nix/bootstrap-job.sh` via `rio-cli keygen derive-pub`) share ONE
//! population enum instead of three hand-rolled byte readers. The
//! bootstrap script previously re-implemented "derive the public half"
//! as `base64 -d | tail -c 32 | base64 -w0` — which, for a 32-byte
//! seed-only entry (a format [`rio-store`'s `Signer::parse`] accepts),
//! published the PRIVATE SEED verbatim onto the world-readable pub
//! secret and into the Job log (round-16 bug_023, critical). The codec
//! makes that unwritable: [`SecretEntry::derive_pub`] returns the
//! seed-DERIVED public key for every accepted format; no byte-window
//! slicing exists anywhere.
//!
//! Accepted secret-entry population (defined by the CONSUMING
//! contract, `rio-store/src/signing.rs::Signer::parse` — not by what
//! any one producer happens to emit):
//!
//! - `{name}:{base64(seed)}` — 32-byte seed-only ([`SecretKeyFormat::Seed32`]).
//! - `{name}:{base64(seed ++ pubkey)}` — 64-byte expanded
//!   ([`SecretKeyFormat::Expanded64`]); the trailing 32 bytes MUST
//!   equal the seed-derived public key. A mismatched tail
//!   ([`KeyFmtError::StaleTail`]) means the redundant copy disagrees
//!   with the actual signing key — publishing it would advertise a
//!   verification key that no signature ever matches, so the codec
//!   refuses (operator intervention beats silently choosing a side).
//!   This is deliberately STRICTER than the historical
//!   `Signer::parse`, which signed with the seed and never looked at
//!   the tail; see `rio-store/src/signing.rs` for the consumer-side
//!   adoption note.
//!
//! Canonical encodings are RFC 4648 STANDARD-alphabet base64 with
//! padding and NO trailing newline (round-16 merged_bug_004: the
//! shell re-derive appended `\n`, silently diverging from the keygen
//! byte format that `cmp`-based pair-consistency checks rely on).
//!
//! Base64 windows of the seed never appear in any public encoding:
//! ed25519 public keys are derived by point multiplication, so the
//! published payload is unrelated to the seed bytes (pinned by the
//! anti-disclosure test below, over both accepted formats).

use base64::Engine as _;
use ed25519_dalek::SigningKey;

/// 32-byte ed25519 seed length.
pub const SEED_LEN: usize = 32;
/// 64-byte expanded (seed ++ pubkey) length.
pub const EXPANDED_LEN: usize = 64;

/// The accepted secret-entry payload formats — the population enum
/// shared by producer and consumer (compile-shared so the two cannot
/// drift; round-16 R5 amendment: the population is defined by the
/// consuming contract, one crate away from the producer that
/// previously guessed it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretKeyFormat {
    /// `base64(seed)` — 32 bytes, seed only.
    Seed32,
    /// `base64(seed ++ pubkey)` — 64 bytes; tail verified against the
    /// seed-derived public key at parse time.
    Expanded64,
}

/// Errors from parsing/encoding signing-key entries.
///
/// Deliberately carries NO key material (no payload bytes, no
/// decoded windows) — these errors land in bootstrap Job logs.
#[derive(Debug, thiserror::Error)]
pub enum KeyFmtError {
    /// Entry has no `:` separator.
    #[error("missing ':' separator (expected name:base64)")]
    MissingSeparator,
    /// Empty key name.
    #[error("key name must not be empty")]
    EmptyName,
    /// Key name contains `:` (would corrupt the `name:base64` framing).
    #[error("key name must not contain ':' (it separates name from key material)")]
    NameContainsColon,
    /// The part after `:` is not valid standard-alphabet base64.
    #[error("key payload is not valid base64: {0}")]
    Base64(#[from] base64::DecodeError),
    /// Decoded payload is neither 32 (seed) nor 64 (expanded) bytes.
    #[error("secret key must be {SEED_LEN} or {EXPANDED_LEN} bytes, got {0}")]
    KeyLength(usize),
    /// Expanded entry whose trailing 32 bytes are NOT the seed-derived
    /// public key. The entry is internally inconsistent (corrupt or
    /// hand-assembled); refusing beats publishing a verification key
    /// that matches no signature.
    #[error(
        "expanded secret entry is internally inconsistent: trailing {SEED_LEN} bytes \
         do not equal the seed-derived public key (refusing to derive from a corrupt entry)"
    )]
    StaleTail,
    /// Public-entry payload decoded but is not 32 bytes.
    #[error("public key must be {SEED_LEN} bytes, got {0}")]
    PubKeyLength(usize),
    /// Public-entry payload is 32 bytes but not a valid ed25519 point.
    #[error("public key bytes are not a valid ed25519 curve point")]
    InvalidCurvePoint,
}

/// A parsed signing-key SECRET entry. Construction is the only way to
/// get one, and every constructor validates the full byte contract —
/// downstream code can no longer observe a malformed or internally
/// inconsistent entry.
// r[impl store.signing.entry-codec]
pub struct SecretEntry {
    name: String,
    seed: [u8; SEED_LEN],
    format: SecretKeyFormat,
}

impl SecretEntry {
    /// Parse a secret entry (`name:base64(seed)` or
    /// `name:base64(seed ++ pubkey)`).
    ///
    /// No whitespace trimming — callers own transport stripping (CLI
    /// `--output text` newlines, editor-appended newlines) so the
    /// codec stays a pure byte contract.
    pub fn parse(entry: &str) -> Result<Self, KeyFmtError> {
        let (name, b64) = entry.split_once(':').ok_or(KeyFmtError::MissingSeparator)?;
        if name.is_empty() {
            return Err(KeyFmtError::EmptyName);
        }
        // STANDARD (not URL_SAFE), with padding: Nix's nix-base64.cc
        // uses the RFC 4648 standard alphabet.
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;
        let (seed, format) = match bytes.len() {
            SEED_LEN => {
                let seed: [u8; SEED_LEN] = bytes.as_slice().try_into().expect("checked len == 32");
                (seed, SecretKeyFormat::Seed32)
            }
            EXPANDED_LEN => {
                let seed: [u8; SEED_LEN] = bytes[..SEED_LEN]
                    .try_into()
                    .expect("slice of len-64 at [..32] is 32 bytes");
                // Verify the redundant tail against the seed-derived
                // public key — the 64-byte stale-tail arm of bug_023.
                let derived = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
                if bytes[SEED_LEN..] != derived {
                    return Err(KeyFmtError::StaleTail);
                }
                (seed, SecretKeyFormat::Expanded64)
            }
            other => return Err(KeyFmtError::KeyLength(other)),
        };
        Ok(Self {
            name: name.to_string(),
            seed,
            format,
        })
    }

    /// Construct from a name and raw seed (the keygen path). Validates
    /// the name framing rules.
    pub fn from_seed(name: &str, seed: &[u8; SEED_LEN]) -> Result<Self, KeyFmtError> {
        if name.is_empty() {
            return Err(KeyFmtError::EmptyName);
        }
        if name.contains(':') {
            return Err(KeyFmtError::NameContainsColon);
        }
        Ok(Self {
            name: name.to_string(),
            seed: *seed,
            format: SecretKeyFormat::Expanded64,
        })
    }

    /// Key name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The 32-byte seed (the actual secret).
    pub fn seed(&self) -> &[u8; SEED_LEN] {
        &self.seed
    }

    /// Which payload format the entry was parsed from.
    pub fn format(&self) -> SecretKeyFormat {
        self.format
    }

    /// Derive the public entry. ALWAYS the seed-derived ed25519 public
    /// key — never a byte window of the stored payload. This is the
    /// only secret→public mapping in the workspace.
    // r[impl store.signing.entry-codec]
    pub fn derive_pub(&self) -> PublicEntry {
        PublicEntry {
            name: self.name.clone(),
            pubkey: SigningKey::from_bytes(&self.seed)
                .verifying_key()
                .to_bytes(),
        }
    }

    /// Canonical secret encoding: `{name}:{base64(seed ++ pubkey)}`,
    /// standard alphabet, padded, no trailing newline. Always the
    /// 64-byte expanded form (what `nix store sign` expects), with the
    /// tail freshly derived — encoding a [`SecretKeyFormat::Seed32`]
    /// entry canonicalizes it.
    pub fn encode(&self) -> String {
        let pubkey = SigningKey::from_bytes(&self.seed)
            .verifying_key()
            .to_bytes();
        let mut expanded = [0u8; EXPANDED_LEN];
        expanded[..SEED_LEN].copy_from_slice(&self.seed);
        expanded[SEED_LEN..].copy_from_slice(&pubkey);
        format!(
            "{}:{}",
            self.name,
            base64::engine::general_purpose::STANDARD.encode(expanded)
        )
    }
}

// SecretEntry holds key material; keep it out of Debug output entirely
// (a derived Debug would print the seed array).
impl std::fmt::Debug for SecretEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretEntry")
            .field("name", &self.name)
            .field("format", &self.format)
            .field("seed", &"[redacted]")
            .finish()
    }
}

/// A parsed signing-key PUBLIC entry (`name:base64(pubkey)` — the
/// `trusted-public-keys` format).
#[derive(Debug, Clone)]
pub struct PublicEntry {
    name: String,
    pubkey: [u8; SEED_LEN],
}

impl PublicEntry {
    /// Parse a public entry, validating base64, length, and that the
    /// bytes form a valid ed25519 curve point.
    pub fn parse(entry: &str) -> Result<Self, KeyFmtError> {
        let (name, b64) = entry.split_once(':').ok_or(KeyFmtError::MissingSeparator)?;
        if name.is_empty() {
            return Err(KeyFmtError::EmptyName);
        }
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;
        let pubkey: [u8; SEED_LEN] = bytes
            .try_into()
            .map_err(|b: Vec<u8>| KeyFmtError::PubKeyLength(b.len()))?;
        ed25519_dalek::VerifyingKey::from_bytes(&pubkey)
            .map_err(|_| KeyFmtError::InvalidCurvePoint)?;
        Ok(Self {
            name: name.to_string(),
            pubkey,
        })
    }

    /// Key name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The 32 public-key bytes.
    pub fn pubkey(&self) -> &[u8; SEED_LEN] {
        &self.pubkey
    }

    /// Canonical public encoding: `{name}:{base64(pubkey)}`, standard
    /// alphabet, padded, no trailing newline.
    pub fn encode(&self) -> String {
        format!(
            "{}:{}",
            self.name,
            base64::engine::general_purpose::STANDARD.encode(self.pubkey)
        )
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use ed25519_dalek::{Signer as _, Verifier as _};

    use super::*;

    fn b64() -> base64::engine::general_purpose::GeneralPurpose {
        base64::engine::general_purpose::STANDARD
    }

    fn seed_entry(name: &str, seed: &[u8; 32]) -> String {
        format!("{name}:{}", b64().encode(seed))
    }

    fn expanded_entry(name: &str, seed: &[u8; 32]) -> String {
        SecretEntry::from_seed(name, seed).unwrap().encode()
    }

    /// Both accepted formats parse to the same seed; derive_pub is the
    /// seed-derived key for both; encodings are canonical and
    /// newline-free; signatures made with the seed verify against the
    /// derived public key.
    // r[verify store.signing.entry-codec]
    #[test]
    fn round_trip_both_formats() {
        let seed = [7u8; 32];
        for entry in [seed_entry("rio-t", &seed), expanded_entry("rio-t", &seed)] {
            let parsed = SecretEntry::parse(&entry).unwrap();
            assert_eq!(parsed.name(), "rio-t");
            assert_eq!(parsed.seed(), &seed);

            let public = parsed.derive_pub();
            let expected = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
            assert_eq!(public.pubkey(), &expected);

            let sec_enc = parsed.encode();
            let pub_enc = public.encode();
            assert!(!sec_enc.ends_with('\n') && !pub_enc.ends_with('\n'));
            // Canonical secret re-parses as Expanded64 with a valid tail.
            assert_eq!(
                SecretEntry::parse(&sec_enc).unwrap().format(),
                SecretKeyFormat::Expanded64
            );
            // Public entry round-trips.
            let re = PublicEntry::parse(&pub_enc).unwrap();
            assert_eq!(re.pubkey(), &expected);

            // Sign with the seed, verify with the derived public key.
            let sig = SigningKey::from_bytes(&seed).sign(b"fingerprint");
            ed25519_dalek::VerifyingKey::from_bytes(public.pubkey())
                .unwrap()
                .verify(b"fingerprint", &sig)
                .expect("seed signature verifies against derived public key");
        }
    }

    /// bug_023 anti-disclosure pin: across random seeds and BOTH
    /// accepted secret formats, the derived public encoding never
    /// contains the seed — not as decoded bytes and not as any base64
    /// window of the entry. The 32-byte seed-only format is the arm
    /// the shell `tail -c 32` re-derive published verbatim.
    // r[verify store.signing.entry-codec]
    #[test]
    fn derive_pub_never_discloses_seed_windows() {
        use rand::Rng as _;
        let mut rng = rand::rng();
        for _ in 0..256 {
            let mut seed = [0u8; 32];
            rng.fill_bytes(&mut seed);
            let seed_b64 = b64().encode(seed);
            for entry in [seed_entry("rio-t", &seed), expanded_entry("rio-t", &seed)] {
                let public = SecretEntry::parse(&entry).unwrap().derive_pub();
                assert_ne!(
                    public.pubkey(),
                    &seed,
                    "derived public key must never equal the seed"
                );
                let pub_enc = public.encode();
                assert!(
                    !pub_enc.contains(&seed_b64),
                    "public encoding must not contain the seed's base64"
                );
                assert_ne!(
                    b64().decode(pub_enc.split_once(':').unwrap().1).unwrap(),
                    seed.to_vec(),
                    "public payload must not decode to the seed"
                );
            }
        }
    }

    /// The 64-byte stale-tail arm: an expanded entry whose trailing 32
    /// bytes are not the seed-derived public key is refused at parse —
    /// it can never reach derive_pub or be published.
    // r[verify store.signing.entry-codec]
    #[test]
    fn stale_tail_refused() {
        let seed = [9u8; 32];
        let mut expanded = [0u8; 64];
        expanded[..32].copy_from_slice(&seed);
        expanded[32..].copy_from_slice(&[0xAB; 32]); // not derive(seed)
        let entry = format!("rio-t:{}", b64().encode(expanded));
        assert!(matches!(
            SecretEntry::parse(&entry),
            Err(KeyFmtError::StaleTail)
        ));
    }

    /// Malformed populations are rejected with typed errors and no
    /// key material in the messages.
    #[test]
    fn malformed_entries_rejected() {
        // Wrong length (48 bytes).
        let e48 = format!("n:{}", b64().encode([1u8; 48]));
        assert!(matches!(
            SecretEntry::parse(&e48),
            Err(KeyFmtError::KeyLength(48))
        ));
        // No separator / empty name / bad base64.
        assert!(matches!(
            SecretEntry::parse("nocolon"),
            Err(KeyFmtError::MissingSeparator)
        ));
        assert!(matches!(
            SecretEntry::parse(":abcd"),
            Err(KeyFmtError::EmptyName)
        ));
        assert!(matches!(
            SecretEntry::parse("n:!!!"),
            Err(KeyFmtError::Base64(_))
        ));
        // from_seed framing rules.
        assert!(matches!(
            SecretEntry::from_seed("", &[0u8; 32]),
            Err(KeyFmtError::EmptyName)
        ));
        assert!(matches!(
            SecretEntry::from_seed("a:b", &[0u8; 32]),
            Err(KeyFmtError::NameContainsColon)
        ));
        // Public entry: wrong length and invalid point.
        let p16 = format!("n:{}", b64().encode([1u8; 16]));
        assert!(matches!(
            PublicEntry::parse(&p16),
            Err(KeyFmtError::PubKeyLength(16))
        ));
        // Errors never carry payload bytes.
        let msg = format!("{}", SecretEntry::parse(&e48).unwrap_err());
        assert!(!msg.contains(&b64().encode([1u8; 48])));
    }

    /// Debug never prints the seed.
    #[test]
    fn debug_redacts_seed() {
        let e = SecretEntry::from_seed("rio-t", &[3u8; 32]).unwrap();
        let dbg = format!("{e:?}");
        assert!(dbg.contains("[redacted]"));
        assert!(!dbg.contains(&b64().encode([3u8; 32])));
    }
}
