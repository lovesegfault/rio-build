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

    /// Direct access to the cluster-fallback [`Signer`].
    ///
    /// ResignPaths (admin.rs) uses this for backfill re-signing:
    /// historical paths have no per-tenant attribution, so re-signing
    /// them always uses the cluster key. Sync, no DB hit — the
    /// cluster key is held by value.
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
        let tid = rio_test_support::seed_tenant(pool, tenant_name).await;
        sqlx::query(
            "INSERT INTO tenant_keys (tenant_id, key_name, ed25519_seed) \
             VALUES ($1, $2, $3)",
        )
        .bind(tid)
        .bind(key_name)
        .bind(seed.as_slice())
        .execute(pool)
        .await
        .unwrap();
        tid
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
}
