//! ed25519 narinfo signing.
//!
//! Signatures are computed at PutPath time and stored in
//! `narinfo.signatures`. The binary-cache HTTP server (cache_server.rs) serves them
//! as `Sig:` lines — it never touches the private key. This means:
// r[impl store.signing.fingerprint]
//! - Key rotation doesn't require re-serving anything (paths signed
//!   under the old key stay valid; new paths get the new key)
//! - The HTTP server can be a separate, less-privileged process
//! - A compromised HTTP server can't forge signatures for paths we
//!   didn't actually store
//!
//! # Key file format (Nix-compatible)
//!
//! `{name}:{base64(secret-key-bytes)}` — same as what
//! `nix-store --generate-binary-cache-key` emits. The name is a
//! human-readable identifier (e.g., `cache.example.org-1`); it goes
//! in the signature string so clients know which public key to verify
//! against.
//!
//! The secret key is the 64-byte ed25519 keypair encoding: 32 bytes of
//! seed (the actual secret) + 32 bytes of public key. Nix stores both
//! together; we only need the seed for signing, but we accept the full
//! 64-byte format for compatibility.

use base64::Engine;
use ed25519_dalek::{Signer as _, SigningKey};

/// A loaded signing key.
///
/// Constructed once at startup from a key file (`Signer::load`).
/// `sign()` is then called at PutPath-complete time with the narinfo
/// fingerprint.
pub struct Signer {
    /// The key name (e.g., `cache.example.org-1`). Goes in the signature
    /// string so clients know which key to verify against.
    key_name: String,
    /// The ed25519 signing key. 32 bytes of seed.
    key: SigningKey,
}

#[derive(Debug, thiserror::Error)]
pub enum SignerError {
    #[error("key file I/O: {0}")]
    Io(#[from] std::io::Error),

    #[error("key file format: expected 'name:base64', got {0} ':'-separated parts")]
    Format(usize),

    #[error("key name is empty")]
    EmptyName,

    #[error("base64 decode failed: {0}")]
    Base64(#[from] base64::DecodeError),

    /// Nix's format is 64 bytes (seed + pubkey). We also accept just the
    /// 32-byte seed. Anything else is malformed.
    #[error("secret key must be 32 or 64 bytes, got {0}")]
    KeyLength(usize),

    /// DB lookup of a tenant's signing key failed. Stringified
    /// `MetadataError` — `metadata` is `pub(crate)`, so we can't put
    /// the typed error on a `pub` signature. The distinction between
    /// retriable (Connection) and permanent (InvariantViolation) is
    /// lost here; callers that care should hit `metadata::get_active_signer`
    /// directly. The cluster-key fallback (`tenant_id = None`) never
    /// touches the DB, so it never hits this variant.
    #[error("tenant key lookup: {0}")]
    TenantKeyLookup(String),
}

impl Signer {
    /// Load a signing key from a file in Nix's format.
    ///
    /// Returns `Ok(None)` if `path` is `None` — signing disabled, not
    /// an error. The store still works; paths just won't have our
    /// signature. Useful for dev/test where nobody's verifying sigs.
    pub fn load(path: Option<&std::path::Path>) -> Result<Option<Self>, SignerError> {
        let Some(path) = path else {
            return Ok(None);
        };

        let content = std::fs::read_to_string(path)?;
        // Trim: key files often have trailing newlines (echo, most
        // editors). A stray newline would become part of the base64
        // input and break decoding with a cryptic "invalid character".
        Self::parse(content.trim()).map(Some)
    }

    /// Parse a key string (`name:base64`). Extracted from `load` so
    /// tests can construct a Signer without touching the filesystem.
    pub fn parse(content: &str) -> Result<Self, SignerError> {
        // split_once: exactly one ':'. A key name CAN contain dashes
        // and dots (e.g., `cache.example.org-1`) but not colons — the
        // colon is THE separator.
        let (name, b64) = content
            .split_once(':')
            .ok_or_else(|| SignerError::Format(content.matches(':').count() + 1))?;

        if name.is_empty() {
            return Err(SignerError::EmptyName);
        }

        // STANDARD (not URL_SAFE): Nix's nix-base64.cc uses the RFC
        // 4648 standard alphabet with '+' and '/', not '-' and '_'.
        // Getting this wrong means every real key file fails to load
        // with "invalid byte" on the first '+' or '/'.
        let key_bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;

        // Nix stores seed + pubkey (64 bytes). We only need the seed.
        // Accept both formats: 64 bytes (take first 32) or 32 bytes
        // (use as-is). ed25519-dalek derives the pubkey from the seed
        // anyway, so the stored pubkey is redundant for us.
        let seed: [u8; 32] = match key_bytes.len() {
            64 => key_bytes[..32]
                .try_into()
                .expect("slice of len-64 at [..32] is 32 bytes"),
            32 => key_bytes.as_slice().try_into().expect("checked len == 32"),
            other => return Err(SignerError::KeyLength(other)),
        };

        Ok(Self {
            key_name: name.to_string(),
            key: SigningKey::from_bytes(&seed),
        })
    }

    /// Construct from a raw 32-byte seed + key name. For loading from
    /// the DB (`tenant_keys.ed25519_seed`) where we already have the
    /// seed as bytes — [`load`](Self::load)/[`parse`](Self::parse) handle
    /// the file format (`name:base64`).
    ///
    /// Infallible: a 32-byte seed is always a valid ed25519 key (every
    /// 256-bit string is a valid scalar for ed25519).
    pub fn from_seed(key_name: impl Into<String>, seed: &[u8; 32]) -> Self {
        Self {
            key_name: key_name.into(),
            key: SigningKey::from_bytes(seed),
        }
    }

    /// Sign a fingerprint, returning a Nix-format signature string.
    ///
    /// Format: `{key_name}:{base64(ed25519_signature)}`. This goes
    /// directly into `narinfo.signatures` and the `Sig:` line.
    ///
    /// The input is the fingerprint from `rio_nix::narinfo::fingerprint()`.
    /// We sign the UTF-8 bytes of that string as-is — no extra framing,
    /// no prepended length. Nix's verifier does exactly `verify(pubkey,
    /// fingerprint_bytes, sig_bytes)`.
    pub fn sign(&self, fingerprint: &str) -> String {
        let signature = self.key.sign(fingerprint.as_bytes());
        // STANDARD encoding again — must match what Nix expects to
        // decode. A mismatch here would produce syntactically-valid
        // signatures that always fail verification.
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
        format!("{}:{}", self.key_name, sig_b64)
    }

    /// The key name (for logging which key is active).
    pub fn key_name(&self) -> &str {
        &self.key_name
    }
}

/// Tenant-aware signing: per-tenant key with cluster-key fallback.
///
/// Wraps the cluster [`Signer`] and a DB pool. When a tenant has an
/// active (unrevoked) key in `tenant_keys`, signs with that; otherwise
/// falls back to the cluster key. A tenant with its own key produces
/// narinfo that `nix store verify --trusted-public-keys tenant-foo:<pk>`
/// accepts for that tenant's paths only — independent trust chain per
/// tenant.
///
/// The cluster `Signer` is held by value (not `Arc`) because `Signer`
/// is cheap (String + 32-byte seed) and this struct is constructed once
/// at startup, not cloned per-request.
pub struct TenantSigner {
    cluster: Signer,
    pool: sqlx::PgPool,
}

impl TenantSigner {
    pub fn new(cluster: Signer, pool: sqlx::PgPool) -> Self {
        Self { cluster, pool }
    }

    /// The cluster fallback key's name (for logging which key signed
    /// when the tenant had none).
    pub fn cluster_key_name(&self) -> &str {
        self.cluster.key_name()
    }

    // r[impl store.tenant.sign-key]
    /// Sign with the tenant's active key if present, else the cluster key.
    ///
    /// Three paths to cluster-key fallback:
    /// - `tenant_id = None` — path has no tenant attribution. No DB hit.
    /// - `Some(tid)` + no `tenant_keys` row — tenant never set a key.
    /// - `Some(tid)` + all rows revoked — tenant rotated to cluster.
    ///
    /// Returns `(signature, signed_with_tenant_key)` so callers can log
    /// which branch fired without re-querying. The bool is cheap and
    /// lets `maybe_sign` emit `key=tenant-foo-1` vs `key=cluster` in
    /// the debug line.
    ///
    /// DB failure (`TenantKeyLookup`) only happens when `tenant_id` is
    /// `Some` — the `None` path is infallible modulo the return type.
    pub async fn sign_for_tenant(
        &self,
        tenant_id: Option<uuid::Uuid>,
        fingerprint: &str,
    ) -> Result<(String, bool), SignerError> {
        if let Some(tid) = tenant_id {
            // Fully-qualified path to avoid `use crate::metadata` at
            // the top of this file — keeps the module-cycle (metadata
            // → signing for `Signer`, signing → metadata for the query)
            // scoped to this one line instead of the whole file.
            if let Some(tenant_signer) = crate::metadata::get_active_signer(&self.pool, tid)
                .await
                .map_err(|e| SignerError::TenantKeyLookup(e.to_string()))?
            {
                return Ok((tenant_signer.sign(fingerprint), true));
            }
        }
        Ok((self.cluster.sign(fingerprint), false))
    }
}

// r[verify store.signing.fingerprint]
#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    /// A known seed → known key pair. Using zeros is simplest; any
    /// fixed seed works. The pubkey is derived deterministically.
    const TEST_SEED: [u8; 32] = [0x42; 32];

    fn test_signer() -> Signer {
        Signer {
            key_name: "test.example.org-1".into(),
            key: SigningKey::from_bytes(&TEST_SEED),
        }
    }

    #[test]
    fn sign_produces_verifiable_signature() {
        let signer = test_signer();
        let fingerprint = "1;/nix/store/test;sha256:abc;123;";
        let sig_str = signer.sign(fingerprint);

        // Parse the signature string back.
        let (name, sig_b64) = sig_str.split_once(':').unwrap();
        assert_eq!(name, "test.example.org-1");

        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(sig_b64)
            .unwrap();
        let sig_arr: [u8; 64] = sig_bytes.as_slice().try_into().unwrap();
        let signature = Signature::from_bytes(&sig_arr);

        // Verify with the DERIVED public key. This is what a Nix
        // client does — it has the pubkey from trusted-public-keys,
        // reconstructs the fingerprint, verifies.
        let verifying_key = SigningKey::from_bytes(&TEST_SEED).verifying_key();
        verifying_key
            .verify(fingerprint.as_bytes(), &signature)
            .expect("signature should verify against the derived pubkey");
    }

    #[test]
    fn different_fingerprints_different_signatures() {
        // Sanity: ed25519 is deterministic, so same fingerprint → same
        // sig. But DIFFERENT fingerprints must produce DIFFERENT sigs.
        // If this fails, we're signing a constant (terrifying bug).
        let signer = test_signer();
        let sig_a = signer.sign("1;/nix/store/a;sha256:x;1;");
        let sig_b = signer.sign("1;/nix/store/b;sha256:x;1;");
        assert_ne!(sig_a, sig_b);
    }

    #[test]
    fn tampered_fingerprint_fails_verification() {
        // A signature over fingerprint A must NOT verify against
        // fingerprint B. This is the security property.
        let signer = test_signer();
        let sig_str = signer.sign("1;/nix/store/real;sha256:x;1;");

        let (_, sig_b64) = sig_str.split_once(':').unwrap();
        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(sig_b64)
            .unwrap();
        let sig_arr: [u8; 64] = sig_bytes.as_slice().try_into().unwrap();
        let signature = Signature::from_bytes(&sig_arr);

        let verifying_key = SigningKey::from_bytes(&TEST_SEED).verifying_key();
        let result = verifying_key.verify(b"1;/nix/store/TAMPERED;sha256:x;1;", &signature);
        assert!(
            result.is_err(),
            "tampered fingerprint must fail verification"
        );
    }

    // ------------------------------------------------------------------------
    // parse()
    // ------------------------------------------------------------------------

    #[test]
    fn parse_64_byte_nix_format() {
        // Nix's format: seed (32) + pubkey (32) = 64 bytes.
        // We should take the first 32 (seed) and ignore the pubkey.
        let seed = [0x11u8; 32];
        // The pubkey bytes can be anything — we derive our own.
        let fake_pubkey = [0xFFu8; 32];
        let mut combined = Vec::from(seed);
        combined.extend_from_slice(&fake_pubkey);

        let b64 = base64::engine::general_purpose::STANDARD.encode(&combined);
        let signer = Signer::parse(&format!("my-key:{b64}")).unwrap();
        assert_eq!(signer.key_name(), "my-key");

        // Verify the seed was extracted correctly by signing something
        // and checking it verifies against the CORRECT derived pubkey
        // (not the fake one we stored).
        let expected_pubkey = SigningKey::from_bytes(&seed).verifying_key();
        let sig_str = signer.sign("test");
        let (_, sig_b64) = sig_str.split_once(':').unwrap();
        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(sig_b64)
            .unwrap();
        let sig_arr: [u8; 64] = sig_bytes.as_slice().try_into().unwrap();
        expected_pubkey
            .verify(b"test", &Signature::from_bytes(&sig_arr))
            .expect("should verify against pubkey derived from seed, not stored fake");
    }

    #[test]
    fn parse_32_byte_seed_only() {
        let seed = [0x22u8; 32];
        let b64 = base64::engine::general_purpose::STANDARD.encode(seed);
        let signer = Signer::parse(&format!("seed-only:{b64}")).unwrap();
        assert_eq!(signer.key_name(), "seed-only");
    }

    #[test]
    fn parse_rejects_no_colon() {
        let result = Signer::parse("nocolon");
        assert!(matches!(result, Err(SignerError::Format(1))));
    }

    #[test]
    fn parse_rejects_empty_name() {
        let b64 = base64::engine::general_purpose::STANDARD.encode([0; 32]);
        let result = Signer::parse(&format!(":{b64}"));
        assert!(matches!(result, Err(SignerError::EmptyName)));
    }

    #[test]
    fn parse_rejects_wrong_key_length() {
        // 16 bytes — neither 32 nor 64.
        let b64 = base64::engine::general_purpose::STANDARD.encode([0; 16]);
        let result = Signer::parse(&format!("name:{b64}"));
        assert!(matches!(result, Err(SignerError::KeyLength(16))));
    }

    #[test]
    fn parse_rejects_bad_base64() {
        let result = Signer::parse("name:not!valid!base64!");
        assert!(matches!(result, Err(SignerError::Base64(_))));
    }

    // ------------------------------------------------------------------------
    // load()
    // ------------------------------------------------------------------------

    #[test]
    fn load_none_returns_none() {
        // No path = signing disabled, not an error.
        let result = Signer::load(None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_trims_trailing_newline() {
        // echo 'name:base64' > keyfile produces a trailing \n. If we
        // don't trim, base64 decode fails cryptically.
        let seed = [0x33u8; 32];
        let b64 = base64::engine::general_purpose::STANDARD.encode(seed);
        let content = format!("trimtest:{b64}\n");

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), content).unwrap();

        let signer = Signer::load(Some(tmp.path())).unwrap().unwrap();
        assert_eq!(signer.key_name(), "trimtest");
    }

    #[test]
    fn load_missing_file_errors() {
        let result = Signer::load(Some(std::path::Path::new("/nonexistent/key/file")));
        assert!(matches!(result, Err(SignerError::Io(_))));
    }

    // ------------------------------------------------------------------------
    // Roundtrip with nix-store --generate-binary-cache-key format
    // ------------------------------------------------------------------------

    /// Generate a key the way Nix would, then parse it, then verify a
    /// signature. This proves we're Nix-compatible end-to-end.
    #[test]
    fn nix_compatible_keygen_roundtrip() {
        // This is what nix-store --generate-binary-cache-key does:
        // generate a keypair, emit seed+pubkey concatenated.
        let signing_key = SigningKey::from_bytes(&TEST_SEED);
        let pubkey_bytes = signing_key.verifying_key().to_bytes();

        let mut nix_format = Vec::with_capacity(64);
        nix_format.extend_from_slice(&TEST_SEED);
        nix_format.extend_from_slice(&pubkey_bytes);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&nix_format);
        let secret_key_file_content = format!("cache.test.org-1:{b64}");

        // Parse it as if loaded from a file.
        let signer = Signer::parse(&secret_key_file_content).unwrap();

        // Sign something.
        let fp = "1;/nix/store/test;sha256:x;1;";
        let sig_str = signer.sign(fp);

        // Verify with the pubkey from the "nix-store" output. This is
        // what a real Nix client would do: read trusted-public-keys,
        // find the matching key name, verify.
        let pubkey = VerifyingKey::from_bytes(&pubkey_bytes).unwrap();
        let (_, sig_b64) = sig_str.split_once(':').unwrap();
        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(sig_b64)
            .unwrap();
        let sig_arr: [u8; 64] = sig_bytes.as_slice().try_into().unwrap();
        pubkey
            .verify(fp.as_bytes(), &Signature::from_bytes(&sig_arr))
            .expect("nix-format key should produce nix-verifiable signatures");
    }
}
