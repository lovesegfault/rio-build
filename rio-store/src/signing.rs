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
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};

/// Parse a `name:base64(pubkey)` trusted-key entry. This is the format
/// Nix's `trusted-public-keys` uses, what [`Signer::trusted_key_entry`]
/// emits, and what `cluster_key_history.pubkey` stores.
///
/// Returns a distinguishable `&'static str` reason for each failure
/// point. Load-time validation ([`TenantSigner::load_prior_cluster`])
/// logs the reason; hot-path [`any_sig_trusted`] discards it via `.ok()`
/// (no error-string allocation per-verify-per-key).
///
/// The four failure points, in order:
/// 1. No `:` separator — entry isn't `name:b64` shaped at all
/// 2. Bad base64 — the part after `:` doesn't decode
/// 3. Wrong length — decoded but not 32 bytes (ed25519 pubkey length)
/// 4. Invalid curve point — 32 bytes but not a valid ed25519 point
///    (rare; ed25519-dalek rejects low-order points)
pub(crate) fn parse_trusted_key_entry(entry: &str) -> Result<(&str, VerifyingKey), &'static str> {
    let (name, pk_b64) = entry
        .split_once(':')
        .ok_or("missing ':' separator (expected name:base64(pubkey))")?;
    let pk_bytes = base64::engine::general_purpose::STANDARD
        .decode(pk_b64)
        .map_err(|_| "pubkey is not valid base64")?;
    let pk: [u8; 32] = pk_bytes
        .try_into()
        .map_err(|_| "pubkey is not 32 bytes (ed25519 public key length)")?;
    VerifyingKey::from_bytes(&pk)
        .map(|vk| (name, vk))
        .map_err(|_| "pubkey is not a valid ed25519 point")
}

// r[impl store.substitute.tenant-sig-visibility]
/// Check if any of `sigs` (narinfo `Sig:` format: `name:base64(sig)`)
/// verifies against any of `trusted_keys` (`name:base64(pubkey)`) for
/// the given `fingerprint`. Returns the matching key name or `None`.
///
/// This is the cross-tenant visibility gate: when tenant B queries a
/// path that tenant A substituted, B sees it IFF one of the stored
/// `narinfo.signatures` verifies against a key in B's trust-set
/// (`metadata::upstreams::tenant_trusted_keys(B)`). Called from
/// `query_path_info` with the REQUESTING tenant's keys — not the
/// substituting tenant's.
///
/// Thin wrapper over the same ed25519-verify loop as
/// [`rio_nix::narinfo::NarInfo::verify_sig`]. That method works on a
/// parsed `NarInfo` (it reconstructs the fingerprint from fields); this
/// one takes the fingerprint directly (the caller has the
/// `ValidatedPathInfo` and can call [`rio_nix::narinfo::fingerprint`]).
///
/// Malformed entries (bad base64, wrong key length) are skipped, not
/// errors — an attacker who can inject garbage sigs shouldn't be able
/// to DoS the gate.
pub fn any_sig_trusted(
    sigs: &[String],
    trusted_keys: &[String],
    fingerprint: &str,
) -> Option<String> {
    // Parse trusted_keys up front. O(keys) not O(keys×sigs) for the
    // base64-decode + VerifyingKey construction. .ok() discards the
    // reason — hot path, don't log per-verify-per-key. Load-time
    // validation (load_prior_cluster) uses the same function LOUDLY;
    // divergence is impossible.
    let keys: Vec<(&str, VerifyingKey)> = trusted_keys
        .iter()
        .filter_map(|k| parse_trusted_key_entry(k).ok())
        .collect();
    if keys.is_empty() {
        return None;
    }

    let b64 = base64::engine::general_purpose::STANDARD;
    for sig in sigs {
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
            .verify(fingerprint.as_bytes(), &Signature::from_bytes(&sig_arr))
            .is_ok()
        {
            return Some(sig_name.to_string());
        }
    }
    None
}

/// A loaded signing key.
///
/// Constructed once at startup from a key file (`Signer::load`).
/// `sign()` is then called at PutPath-complete time with the narinfo
/// fingerprint.
///
/// `Clone` so batch callers ([`TenantSigner::resolve_once`]) can hold
/// an owned copy across N sign calls without re-querying the DB. Cheap:
/// `String` + 32-byte seed (`SigningKey` is `Clone`).
#[derive(Clone)]
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

    /// This signer's `name:base64(pubkey)` entry — what a client puts
    /// in `trusted-public-keys` to trust signatures from this key.
    ///
    /// The sig_visibility_gate uses this to union the cluster key into
    /// a tenant's trusted set: a freshly-built path (rio-signed,
    /// `path_tenants` not yet populated by the scheduler) must verify
    /// against the cluster key, not only the tenant's upstream keys.
    pub fn trusted_key_entry(&self) -> String {
        let pk = self.key.verifying_key();
        let pk_b64 = base64::engine::general_purpose::STANDARD.encode(pk.to_bytes());
        format!("{}:{}", self.key_name, pk_b64)
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
    // r[impl store.key.rotation-cluster-history]
    // Prior cluster keys in `name:base64(pubkey)` format (what
    // `Signer::trusted_key_entry` returns). Loaded once at startup from
    // `cluster_key_history WHERE retired_at IS NULL`. sig_visibility_gate
    // extends the trusted set with these so paths signed under a
    // rotated-out cluster key stay visible after CASCADE drops their
    // path_tenants rows.
    //
    // Not `Vec<VerifyingKey>` — `any_sig_trusted` matches by name first
    // (`keys.iter().find(|(n, _)| *n == sig_name)`), and the name is
    // only in the entry-format string. Storing the entry means zero
    // parsing at gate time.
    prior_cluster: Vec<String>,
    pool: sqlx::PgPool,
}

impl TenantSigner {
    pub fn new(cluster: Signer, pool: sqlx::PgPool) -> Self {
        Self {
            cluster,
            prior_cluster: Vec::new(),
            pool,
        }
    }

    /// Attach prior cluster keys (from `cluster_key_history`).
    ///
    /// Builder-style so the ~dozen test callsites that don't exercise
    /// rotation stay at `TenantSigner::new(cluster, pool)` with an
    /// empty prior set. Only main.rs startup (and the rotation test)
    /// chain this.
    pub fn with_prior_cluster(mut self, keys: Vec<String>) -> Self {
        self.prior_cluster = keys;
        self
    }

    /// Load prior cluster keys from `cluster_key_history WHERE
    /// retired_at IS NULL`. Call once at startup; pass to
    /// [`Self::with_prior_cluster`].
    ///
    /// Thin delegate to `crate::metadata` (kept `pub(crate)`) so
    /// main.rs reaches the query through the public `signing` module.
    /// Same cycle-local pattern as [`Self::resolve_once`]'s
    /// `get_active_signer` call.
    pub async fn load_prior_cluster(pool: &sqlx::PgPool) -> Result<Vec<String>, SignerError> {
        let entries = crate::metadata::load_cluster_key_history(pool)
            .await
            .map_err(|e| SignerError::TenantKeyLookup(e.to_string()))?;

        // r[impl store.key.rotation-cluster-history]
        // Validate at load. any_sig_trusted's filter_map silently
        // discards malformed entries — an operator typo during
        // rotation would make old-key paths go dark with ZERO signal.
        // Parse here, once, loudly.
        let total = entries.len();
        let mut valid = Vec::with_capacity(total);
        for entry in entries {
            match parse_trusted_key_entry(&entry) {
                Ok((name, _)) => {
                    tracing::debug!(key_name = name, "prior cluster key loaded");
                    valid.push(entry);
                }
                Err(reason) => {
                    tracing::warn!(
                        entry = %entry,
                        reason,
                        "malformed cluster_key_history entry — SKIPPED. \
                         Paths signed under this key will fail sig-visibility gate. \
                         Fix the cluster_key_history.pubkey column or retire the row."
                    );
                    // warn+skip, not fail-startup. A malformed OLD key
                    // shouldn't block store boot — but the warn is loud,
                    // and the skip is now OBSERVABLE (log + count mismatch).
                }
            }
        }
        if valid.len() != total {
            tracing::warn!(
                loaded = valid.len(),
                total,
                skipped = total - valid.len(),
                "cluster_key_history: some entries malformed — see above"
            );
        }
        Ok(valid)
    }

    /// Prior cluster keys as `name:base64(pubkey)` entries, ready for
    /// `Vec::extend` into a trusted-key set at the sig-visibility gate.
    pub fn prior_cluster_entries(&self) -> &[String] {
        &self.prior_cluster
    }

    /// Direct access to the cluster-fallback [`Signer`].
    ///
    /// Sync, no DB hit — the cluster key is held by value.
    pub fn cluster(&self) -> &Signer {
        &self.cluster
    }

    // r[impl store.tenant.sign-key]
    /// Resolve the signer for a tenant once — cluster-fallback applied.
    ///
    /// For callers that sign N times with the same `tenant_id` (e.g.,
    /// PutPathBatch signs each output with the same tenant's key).
    /// Returns (cloned [`Signer`], `was_tenant_key`). The `Signer` is
    /// cheap to clone (String + 32-byte seed). Calling [`Signer::sign`]
    /// on the returned value is sync + infallible — no further DB hits.
    ///
    /// Three paths to cluster-key fallback:
    /// - `tenant_id = None` — path has no tenant attribution. No DB hit.
    /// - `Some(tid)` + no `tenant_keys` row — tenant never set a key.
    /// - `Some(tid)` + all rows revoked — tenant rotated to cluster.
    ///
    /// DB failure ([`SignerError::TenantKeyLookup`]) propagates — the
    /// caller decides whether to fall back (PutPathBatch does, matching
    /// `maybe_sign`'s behavior). The `None` path is infallible.
    pub async fn resolve_once(
        &self,
        tenant_id: Option<uuid::Uuid>,
    ) -> Result<(Signer, bool), SignerError> {
        if let Some(tid) = tenant_id {
            // Fully-qualified `crate::metadata::` path keeps the
            // signing↔metadata cycle local to this one call site
            // (metadata → signing for `Signer`, signing → metadata
            // for the query) instead of a top-level `use`.
            if let Some(tenant_signer) = crate::metadata::get_active_signer(&self.pool, tid)
                .await
                .map_err(|e| SignerError::TenantKeyLookup(e.to_string()))?
            {
                return Ok((tenant_signer, true));
            }
        }
        Ok((self.cluster.clone(), false))
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

    // ------------------------------------------------------------------------
    // TenantSigner: per-tenant key with cluster fallback
    // ------------------------------------------------------------------------

    // r[verify store.tenant.sign-key]
    /// Cluster seed for TenantSigner tests. Intentionally distinct from
    /// the tenant seed below — the whole point of the "verifies with
    /// tenant NOT cluster" assertion is the seeds differ. If they
    /// matched, both verifications would pass and the test proves nothing.
    const CLUSTER_SEED: [u8; 32] = [0xAA; 32];
    const TENANT_SEED: [u8; 32] = [0xBB; 32];

    /// Parse a `{name}:{b64}` signature string into a raw ed25519
    /// `Signature`. Shared by both verification-property tests.
    fn parse_sig(sig_str: &str) -> Signature {
        let (_, sig_b64) = sig_str.split_once(':').unwrap();
        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(sig_b64)
            .unwrap();
        let arr: [u8; 64] = sig_bytes.as_slice().try_into().unwrap();
        Signature::from_bytes(&arr)
    }

    async fn seed_tenant_key(
        pool: &sqlx::PgPool,
        tenant_name: &str,
        key_name: &str,
        seed: &[u8; 32],
    ) -> uuid::Uuid {
        rio_test_support::TenantSeed::new(tenant_name)
            .with_ed25519_key(*seed)
            .with_key_name(key_name)
            .seed(pool)
            .await
    }

    // r[verify store.tenant.sign-key]
    /// A tenant WITH a key produces a signature that verifies under the
    /// TENANT pubkey and does NOT verify under the cluster pubkey. This
    /// is the security property: a tenant's trust chain is independent —
    /// `nix store verify --trusted-public-keys tenant-foo:<pk>` accepts
    /// that tenant's paths, NOT paths signed by the cluster key.
    #[tokio::test]
    async fn tenant_with_key_signs_with_tenant_key() {
        // Precondition guard: if these match, the verify-under-tenant-NOT-
        // cluster assertion below is vacuous (both would pass). Asserting
        // this at runtime means the test can't silently become a no-op if
        // someone refactors the consts to share a seed.
        assert_ne!(CLUSTER_SEED, TENANT_SEED, "test precondition: seeds differ");

        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant_key(&db.pool, "ts-with-key", "tenant-foo-1", &TENANT_SEED).await;

        let ts = TenantSigner::new(
            Signer::from_seed("cluster-1", &CLUSTER_SEED),
            db.pool.clone(),
        );
        let fp = "1;/nix/store/x;sha256:y;42;";
        let (signer, was_tenant) = ts.resolve_once(Some(tid)).await.unwrap();
        let sig_str = signer.sign(fp);

        assert!(was_tenant, "should have used tenant key");
        assert!(
            sig_str.starts_with("tenant-foo-1:"),
            "key name in sig should be the tenant's, got {sig_str}"
        );

        // THE assertion: tenant pubkey verifies, cluster pubkey rejects.
        let tenant_pk = SigningKey::from_bytes(&TENANT_SEED).verifying_key();
        let cluster_pk = SigningKey::from_bytes(&CLUSTER_SEED).verifying_key();
        let sig = parse_sig(&sig_str);

        tenant_pk
            .verify(fp.as_bytes(), &sig)
            .expect("tenant-signed narinfo MUST verify under tenant pubkey");
        assert!(
            cluster_pk.verify(fp.as_bytes(), &sig).is_err(),
            "tenant-signed narinfo MUST NOT verify under cluster pubkey \
             (independent trust chain)"
        );
    }

    // r[verify store.tenant.sign-key]
    /// A tenant WITHOUT a key falls back to the cluster key. Signature
    /// verifies under cluster pubkey. This is the "most tenants don't
    /// bother setting a key" default.
    #[tokio::test]
    async fn tenant_without_key_falls_back_to_cluster() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;

        // Tenant exists but has NO tenant_keys row.
        let tid = rio_test_support::seed_tenant(&db.pool, "ts-no-key").await;

        let ts = TenantSigner::new(
            Signer::from_seed("cluster-1", &CLUSTER_SEED),
            db.pool.clone(),
        );
        let fp = "1;/nix/store/x;sha256:y;42;";
        let (signer, was_tenant) = ts.resolve_once(Some(tid)).await.unwrap();
        let sig_str = signer.sign(fp);

        assert!(!was_tenant, "no tenant key → cluster fallback");
        assert!(
            sig_str.starts_with("cluster-1:"),
            "key name should be cluster's, got {sig_str}"
        );

        let cluster_pk = SigningKey::from_bytes(&CLUSTER_SEED).verifying_key();
        cluster_pk
            .verify(fp.as_bytes(), &parse_sig(&sig_str))
            .expect("fallback-signed narinfo MUST verify under cluster pubkey");
    }

    // r[verify store.tenant.sign-key]
    /// `tenant_id = None` (path has no tenant attribution) → cluster
    /// key, NO DB query. Distinct from the "tenant exists but no key"
    /// case — this is the fast path, no roundtrip.
    #[tokio::test]
    async fn no_tenant_id_uses_cluster_without_db_hit() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;

        // Seed a tenant WITH a key — proves we're NOT accidentally
        // hitting the DB and picking up an unrelated tenant's key.
        // Without this seeded row, the test would pass even if
        // `None` was mistakenly hitting the DB (empty table → None
        // → cluster fallback anyway).
        seed_tenant_key(&db.pool, "ts-decoy", "decoy-1", &TENANT_SEED).await;

        let ts = TenantSigner::new(
            Signer::from_seed("cluster-1", &CLUSTER_SEED),
            db.pool.clone(),
        );
        let (signer, was_tenant) = ts.resolve_once(None).await.unwrap();
        let sig_str = signer.sign("fp");

        assert!(!was_tenant);
        assert!(sig_str.starts_with("cluster-1:"));
    }

    // ------------------------------------------------------------------------
    // resolve_once: one-shot signer resolution for batch callers
    // ------------------------------------------------------------------------

    /// `resolve_once` with a tenant that has a key returns the tenant's
    /// `Signer`. The returned signer is a clone — calling `.sign()` on it
    /// is sync + no-DB. Verifies under the TENANT pubkey, NOT cluster.
    #[tokio::test]
    async fn resolve_once_tenant_with_key_returns_tenant_signer() {
        assert_ne!(CLUSTER_SEED, TENANT_SEED, "test precondition: seeds differ");

        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant_key(&db.pool, "ro-with-key", "tenant-ro-1", &TENANT_SEED).await;

        let ts = TenantSigner::new(
            Signer::from_seed("cluster-1", &CLUSTER_SEED),
            db.pool.clone(),
        );
        let (signer, was_tenant) = ts.resolve_once(Some(tid)).await.unwrap();

        assert!(was_tenant, "tenant has a key → was_tenant true");
        assert_eq!(signer.key_name(), "tenant-ro-1");

        // The returned Signer is a standalone clone — signing N times is
        // N sync calls, zero DB hits. Sign TWO different fingerprints to
        // prove it's not caching a single result, just the key material.
        let fp1 = "1;/nix/store/a;sha256:x;1;";
        let fp2 = "1;/nix/store/b;sha256:y;2;";
        let sig1 = signer.sign(fp1);
        let sig2 = signer.sign(fp2);
        assert_ne!(sig1, sig2);

        // Both verify under the TENANT pubkey — the resolved signer holds
        // the tenant's seed, not the cluster's.
        let tenant_pk = SigningKey::from_bytes(&TENANT_SEED).verifying_key();
        let cluster_pk = SigningKey::from_bytes(&CLUSTER_SEED).verifying_key();
        tenant_pk
            .verify(fp1.as_bytes(), &parse_sig(&sig1))
            .expect("resolved tenant signer MUST produce tenant-verifiable sigs");
        assert!(
            cluster_pk
                .verify(fp1.as_bytes(), &parse_sig(&sig1))
                .is_err(),
            "resolved tenant signer MUST NOT produce cluster-verifiable sigs"
        );
    }

    /// `resolve_once` with no tenant, or tenant without a key → cluster
    /// signer. `was_tenant` false.
    #[tokio::test]
    async fn resolve_once_fallback_returns_cluster_signer() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;

        // Tenant with NO key (fallback path #2).
        let tid = rio_test_support::seed_tenant(&db.pool, "ro-no-key").await;

        let ts = TenantSigner::new(
            Signer::from_seed("cluster-1", &CLUSTER_SEED),
            db.pool.clone(),
        );

        // Fallback path #1: None tenant_id → cluster, no DB hit.
        let (signer_none, wt_none) = ts.resolve_once(None).await.unwrap();
        assert!(!wt_none);
        assert_eq!(signer_none.key_name(), "cluster-1");

        // Fallback path #2: Some(tid) but no tenant_keys row → cluster.
        let (signer_nokey, wt_nokey) = ts.resolve_once(Some(tid)).await.unwrap();
        assert!(!wt_nokey);
        assert_eq!(signer_nokey.key_name(), "cluster-1");

        // The clone is a distinct value (cluster.clone(), not a borrow).
        // Sign with it and verify under cluster pubkey.
        let cluster_pk = SigningKey::from_bytes(&CLUSTER_SEED).verifying_key();
        cluster_pk
            .verify(b"fp", &parse_sig(&signer_none.sign("fp")))
            .expect("cluster clone signs cluster-verifiable sigs");
    }

    // ------------------------------------------------------------------------
    // any_sig_trusted — cross-tenant sig-visibility gate
    // ------------------------------------------------------------------------

    fn make_trusted_key(name: &str, seed: &[u8; 32]) -> String {
        let pk = SigningKey::from_bytes(seed).verifying_key();
        format!(
            "{name}:{}",
            base64::engine::general_purpose::STANDARD.encode(pk.as_bytes())
        )
    }

    // r[verify store.substitute.tenant-sig-visibility]
    #[test]
    fn any_sig_trusted_accepts_matching() {
        let seed = [0x11u8; 32];
        let signer = Signer::from_seed("key-a", &seed);
        let fp = "1;/nix/store/x;sha256:y;1;";
        let sig = signer.sign(fp);
        let trusted = make_trusted_key("key-a", &seed);

        assert_eq!(
            any_sig_trusted(&[sig], &[trusted], fp).as_deref(),
            Some("key-a")
        );
    }

    #[test]
    fn any_sig_trusted_rejects_untrusted() {
        let signer = Signer::from_seed("key-a", &[0x11u8; 32]);
        let fp = "1;/nix/store/x;sha256:y;1;";
        let sig = signer.sign(fp);
        // Different key in trust set — key-b, not key-a.
        let trusted = make_trusted_key("key-b", &[0x22u8; 32]);

        assert_eq!(any_sig_trusted(&[sig], &[trusted], fp), None);
    }

    #[test]
    fn any_sig_trusted_rejects_tampered_fingerprint() {
        let seed = [0x11u8; 32];
        let signer = Signer::from_seed("key-a", &seed);
        let sig = signer.sign("1;/nix/store/REAL;sha256:y;1;");
        let trusted = make_trusted_key("key-a", &seed);

        // Sig was over REAL; verify against TAMPERED → None.
        assert_eq!(
            any_sig_trusted(&[sig], &[trusted], "1;/nix/store/TAMPERED;sha256:y;1;"),
            None
        );
    }

    #[test]
    fn any_sig_trusted_multi_sig_multi_key() {
        // Two sigs: one by key-a (trusted), one by key-c (untrusted).
        // trusted_keys has key-a and key-b. Expect match on key-a.
        let seed_a = [0x11u8; 32];
        let fp = "1;/nix/store/x;sha256:y;1;";
        let sig_a = Signer::from_seed("key-a", &seed_a).sign(fp);
        let sig_c = Signer::from_seed("key-c", &[0x33u8; 32]).sign(fp);

        let trusted = vec![
            make_trusted_key("key-a", &seed_a),
            make_trusted_key("key-b", &[0x22u8; 32]),
        ];

        assert_eq!(
            any_sig_trusted(&[sig_c, sig_a], &trusted, fp).as_deref(),
            Some("key-a"),
            "first matching key name returned"
        );
    }

    #[test]
    fn any_sig_trusted_empty_inputs() {
        let fp = "1;/nix/store/x;sha256:y;1;";
        assert_eq!(any_sig_trusted(&[], &["k:abc".into()], fp), None);
        assert_eq!(any_sig_trusted(&["k:abc".into()], &[], fp), None);
    }

    #[test]
    fn any_sig_trusted_skips_malformed() {
        let seed = [0x11u8; 32];
        let fp = "1;/nix/store/x;sha256:y;1;";
        let good_sig = Signer::from_seed("good", &seed).sign(fp);
        let good_key = make_trusted_key("good", &seed);

        // Garbage entries mixed in — shouldn't break the valid match.
        let sigs = vec!["no-colon".into(), "bad:!!not-base64!!".into(), good_sig];
        let keys = vec!["malformed-key-no-colon".into(), good_key];

        assert_eq!(any_sig_trusted(&sigs, &keys, fp).as_deref(), Some("good"));
    }

    // ------------------------------------------------------------------------
    // parse_trusted_key_entry — load-time validation helper
    // ------------------------------------------------------------------------

    #[test]
    fn parse_trusted_key_entry_error_reasons() {
        // Each failure point gets a distinct reason — operator can tell
        // WHAT's wrong without comparing against a spec. These reasons
        // end up verbatim in the load_prior_cluster warn!.
        assert_eq!(
            parse_trusted_key_entry("no-colon").unwrap_err(),
            "missing ':' separator (expected name:base64(pubkey))"
        );
        assert_eq!(
            parse_trusted_key_entry("name:!!!").unwrap_err(),
            "pubkey is not valid base64"
        );
        // "test" → 4 bytes, not 32
        assert_eq!(
            parse_trusted_key_entry("name:dGVzdA==").unwrap_err(),
            "pubkey is not 32 bytes (ed25519 public key length)"
        );
        // 31 bytes — off-by-one the operator would actually hit.
        let thirty_one = base64::engine::general_purpose::STANDARD.encode([0u8; 31]);
        assert_eq!(
            parse_trusted_key_entry(&format!("name:{thirty_one}")).unwrap_err(),
            "pubkey is not 32 bytes (ed25519 public key length)"
        );

        // Roundtrip: what trusted_key_entry() emits, this parses.
        let entry = Signer::from_seed("roundtrip", &[0x55u8; 32]).trusted_key_entry();
        let (name, _vk) = parse_trusted_key_entry(&entry).expect("roundtrip parses");
        assert_eq!(name, "roundtrip");
    }
}
