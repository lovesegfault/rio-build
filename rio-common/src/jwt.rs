//! JWT tenant tokens — ed25519 sign/verify for `x-rio-tenant-token`.
//!
//! The gateway mints a short-lived JWT on SSH auth and propagates it
//! through every internal gRPC call (scheduler, store). Downstream
//! services verify signature + expiry before processing any request —
//! invalid or expired → `UNAUTHENTICATED`.
//!
//! # Why JWT, not the hmac module's pattern
//!
//! [`crate::hmac`] is a symmetric-key assignment token: scheduler and
//! store share a secret. JWT here is asymmetric: the gateway holds the
//! ed25519 signing key (K8s Secret); everyone else gets only the public
//! verifying key (K8s ConfigMap, reloaded on SIGHUP). A compromised
//! scheduler or store can't mint tenant tokens.
//!
//! # Key format
//!
//! Keys pass through PKCS#8 DER on the way into `jsonwebtoken`. The
//! `ed25519-dalek` types ([`SigningKey`], [`VerifyingKey`]) are the
//! source of truth — callers load them from whatever storage (raw
//! seed bytes for tests, K8s Secret for prod); this module just
//! encodes to DER at sign/verify time. That keeps the key-loading
//! story (rotation, SIGHUP reload, ConfigMap mounts) out of this crate.

// r[impl gw.jwt.claims]

use ed25519_dalek::pkcs8::EncodePrivateKey;
use ed25519_dalek::{SigningKey, VerifyingKey};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Claims carried in a tenant JWT.
///
/// Field names match the RFC 7519 registered claim names — that's
/// what `jsonwebtoken`'s [`Validation`] expects when checking `exp`.
/// A renamed field (`#[serde(rename = "exp")] expires: i64`) would
/// work but obscures the wire format when debugging.
///
/// Named `TenantClaims` (not bare `Claims`) to disambiguate from
/// [`crate::hmac::AssignmentClaims`] — both appear together in PutPath
/// handlers, and `hmac::Claims` vs `jwt::Claims` was a recurring
/// source of confusion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantClaims {
    /// Tenant UUID. Server-resolved at mint time — the gateway
    /// matches the SSH key against `authorized_keys`, reads the
    /// tenant name from the comment, and resolves it to a UUID via
    /// the scheduler (see `r[sched.tenant.resolve]`). The client
    /// never chooses `sub`; it's bound by the SSH key.
    pub sub: Uuid,

    /// Issued-at, unix epoch seconds.
    ///
    /// Not validated on verify (jsonwebtoken doesn't check `iat`
    /// unless you set `validate_nbf` and `iat` doubles as `nbf`,
    /// which it doesn't here). Carried for audit: "when did the
    /// gateway mint this?"
    pub iat: i64,

    /// Expiry, unix epoch seconds.
    ///
    /// Spec says "SSH session duration + grace period". Validated
    /// on every verify — a token that outlived its session is
    /// rejected even if the signature is valid.
    pub exp: i64,

    /// Unique token ID. UUID-string form.
    ///
    /// Used for **revocation** (scheduler checks `jti NOT IN
    /// jwt_revoked` on every request — see `r[gw.jwt.verify]`)
    /// and **audit** (INSERTed into `builds.jwt_jti` per
    /// `r[gw.jwt.issue]`). The gateway stays PG-free; revocation
    /// is scheduler-side only.
    ///
    /// NOT the rate-limit partition key — rate-limit keys off `sub`
    /// (tenant UUID, bounded keyspace). `jti` is per-session and
    /// unbounded; partitioning a limiter on it would leak memory.
    pub jti: String,
}

#[derive(Debug, thiserror::Error)]
pub enum JwtError {
    /// Covers signature mismatch, malformed token, expired `exp`,
    /// wrong algorithm. `jsonwebtoken` collapses these into one
    /// error type with a `.kind()` discriminator — the Display
    /// impl is descriptive enough for logs. A caller that needs
    /// to distinguish "expired" from "bad signature" (e.g., to
    /// return a different gRPC status detail) can downcast via
    /// `.kind()`.
    #[error("JWT verify/encode failed: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),

    /// Signing key → PKCS#8 DER encoding. Only reachable from
    /// [`sign`] (verify uses raw pubkey bytes). Never fails for a
    /// valid ed25519 key — it's a fixed-format serialization — so
    /// this variant firing means the key itself is corrupt. Kept as
    /// a variant rather than `.expect()` because key material comes
    /// from outside (K8s Secret) and a corrupt mount should error
    /// cleanly, not panic the gateway.
    #[error("ed25519 signing key → PKCS#8 DER encoding failed (corrupt key?): {0}")]
    KeyEncoding(String),
}

/// Sign claims into a compact JWT string (`header.payload.signature`).
///
/// Algorithm is fixed at EdDSA — not a parameter, because the spec
/// says ed25519 and algorithm agility in JWT has historically been a
/// footgun (`alg: none` downgrade attacks). If a caller passes an
/// RSA key here, that's a type error at compile time, not a runtime
/// surprise.
pub fn sign(claims: &TenantClaims, key: &SigningKey) -> Result<String, JwtError> {
    // PKCS#8 DER is what jsonwebtoken's from_ed_der expects. The
    // pkcs8 feature on ed25519-dalek gives us the EncodePrivateKey
    // trait. SecretDocument zeroizes on drop — the DER bytes don't
    // leak past this scope.
    let der = key
        .to_pkcs8_der()
        .map_err(|e| JwtError::KeyEncoding(e.to_string()))?;
    let encoding_key = EncodingKey::from_ed_der(der.as_bytes());

    jsonwebtoken::encode(&Header::new(Algorithm::EdDSA), claims, &encoding_key).map_err(Into::into)
}

/// Verify a JWT and return its claims.
///
/// Checks, in order: header algorithm is EdDSA, signature is valid
/// under `pubkey`, `exp` is in the future (with jsonwebtoken's
/// default 60s leeway — reasonable for cross-service clock skew).
/// Does NOT check `jti` against a revocation list — that's the
/// scheduler's job, needs PG access this crate doesn't have.
pub fn verify(token: &str, pubkey: &VerifyingKey) -> Result<TenantClaims, JwtError> {
    // Raw 32-byte compressed Edwards point — NOT the SPKI DER
    // wrapper (`to_public_key_der()`). The name `from_ed_der` is a
    // misnomer (carried over from v9 into v10/rust_crypto): it just
    // stores the bytes, and ed25519-dalek's verify expects the raw
    // 32-byte key. SPKI DER is ~44 bytes with ASN.1 framing; that
    // would be read as a malformed 44-byte key → InvalidSignature on
    // every verify. The sign-side IS genuinely PKCS#8 DER — the API
    // is asymmetric.
    let decoding_key = DecodingKey::from_ed_der(pubkey.as_bytes());

    // Validation::new(EdDSA) defaults: validate_exp=true,
    // required_spec_claims={"exp"}, leeway=60s, algorithms=[EdDSA].
    // The algorithms list matters — a token with `alg: HS256` in
    // the header is rejected even if someone found a way to make
    // the HMAC verify. No alg-confusion.
    //
    // We set nothing else: no `aud` (we don't use audiences),
    // no `iss` (single issuer — the gateway), no `sub` allowlist
    // (every tenant UUID is valid; authorization is downstream).
    let validation = Validation::new(Algorithm::EdDSA);

    let token_data = jsonwebtoken::decode::<TenantClaims>(token, &decoding_key, &validation)?;
    Ok(token_data.claims)
}

// r[verify gw.jwt.claims]
#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::errors::ErrorKind;
    use proptest::prelude::*;

    /// Fixed seed → deterministic keypair. Matches the pattern in
    /// `rio-store/src/signing.rs`. We never call `SigningKey::generate`
    /// in tests: that needs a `rand_core` 0.6 RNG, but the workspace
    /// is on `rand` 0.9 (→ `rand_core` 0.9). Building from seed bytes
    /// sidesteps the trait-version mismatch entirely AND makes
    /// proptest shrinking meaningful (same seed → same key → same
    /// failure).
    fn key_from_seed(seed: [u8; 32]) -> SigningKey {
        SigningKey::from_bytes(&seed)
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock before 1970")
            .as_secs() as i64
    }

    /// Claims with `exp` offset from now. Negative offset → expired.
    fn test_claims(exp_offset_secs: i64) -> TenantClaims {
        let n = now();
        TenantClaims {
            sub: Uuid::from_u128(0xdead_beef_cafe_0000_0000_0000_0000_0001),
            iat: n,
            exp: n + exp_offset_secs,
            jti: "test-jti-fixed".into(),
        }
    }

    // ------------------------------------------------------------------------
    // Positive path
    // ------------------------------------------------------------------------

    #[test]
    fn sign_verify_roundtrip() {
        let key = key_from_seed([0x42; 32]);
        let claims = test_claims(3600);

        let token = sign(&claims, &key).expect("sign");
        let decoded = verify(&token, &key.verifying_key()).expect("verify");

        assert_eq!(decoded, claims);
    }

    /// A JWT is three base64url parts joined by '.'. Operators
    /// debugging a failing token can base64-decode the middle part
    /// and get readable JSON. Same "human-debuggable" property as
    /// hmac.rs's token format — documented-by-example here.
    #[test]
    fn token_is_compact_jwt_format() {
        let key = key_from_seed([0x01; 32]);
        let claims = test_claims(3600);
        let token = sign(&claims, &key).expect("sign");

        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3, "header.payload.signature");

        // Middle part decodes to JSON containing our claims.
        // jsonwebtoken uses URL_SAFE_NO_PAD — same as we do in hmac.rs.
        let payload_json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[1])
            .expect("payload is base64url");
        let payload: serde_json::Value =
            serde_json::from_slice(&payload_json).expect("payload is JSON");

        // sub serializes to the hyphenated UUID string form (uuid's
        // serde impl). Verify the wire format directly — if uuid
        // switches to a 128-bit-integer serialization someday, this
        // test catches it before it breaks downstream verifiers.
        assert_eq!(
            payload["sub"].as_str().expect("sub is a string"),
            claims.sub.to_string()
        );
        assert_eq!(
            payload["jti"].as_str().expect("jti is a string"),
            claims.jti
        );
    }

    // ------------------------------------------------------------------------
    // Negative path — every rejection mode the spec cares about
    // ------------------------------------------------------------------------

    #[test]
    fn expired_jwt_rejected() {
        let key = key_from_seed([0x42; 32]);
        // 1h in the past. jsonwebtoken has 60s default leeway; 3600s
        // is well past it. Using -30 here would be flaky under the
        // leeway.
        let claims = test_claims(-3600);

        let token = sign(&claims, &key).expect("sign never checks exp");
        let err = verify(&token, &key.verifying_key()).expect_err("expired → reject");

        // Assert the SPECIFIC failure mode. A bug that made verify()
        // accept expired tokens but reject on some other axis
        // (malformed, wrong alg) would still fail the expect_err
        // above — this line pins it to ExpiredSignature.
        let JwtError::Jwt(inner) = &err else {
            panic!("expected Jwt variant, got {err:?}");
        };
        assert!(
            matches!(inner.kind(), ErrorKind::ExpiredSignature),
            "expected ExpiredSignature, got {:?}",
            inner.kind()
        );
    }

    #[test]
    fn wrong_key_rejected() {
        let signer = key_from_seed([0xAA; 32]);
        let other = key_from_seed([0xBB; 32]);
        let claims = test_claims(3600);

        let token = sign(&claims, &signer).expect("sign");
        let err =
            verify(&token, &other.verifying_key()).expect_err("different key → signature mismatch");

        let JwtError::Jwt(inner) = &err else {
            panic!("expected Jwt variant, got {err:?}");
        };
        assert!(
            matches!(inner.kind(), ErrorKind::InvalidSignature),
            "expected InvalidSignature, got {:?}",
            inner.kind()
        );
    }

    /// The payload is signed, not encrypted. An attacker who edits
    /// the payload (e.g., swaps `sub` to another tenant's UUID) MUST
    /// fail signature verification.
    #[test]
    fn tampered_payload_rejected() {
        let key = key_from_seed([0x42; 32]);
        let claims = test_claims(3600);
        let token = sign(&claims, &key).expect("sign");

        let parts: Vec<&str> = token.split('.').collect();
        // Swap in a different sub. Keep the original signature.
        let evil = TenantClaims {
            sub: Uuid::from_u128(0xEEEE_EEEE_EEEE_EEEE_EEEE_EEEE_EEEE_EEEE),
            ..claims
        };
        let evil_payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&evil).unwrap());
        let tampered = format!("{}.{}.{}", parts[0], evil_payload, parts[2]);

        let err = verify(&tampered, &key.verifying_key())
            .expect_err("tampered payload → signature mismatch");
        let JwtError::Jwt(inner) = &err else {
            panic!("expected Jwt variant, got {err:?}");
        };
        assert!(
            matches!(inner.kind(), ErrorKind::InvalidSignature),
            "expected InvalidSignature, got {:?}",
            inner.kind()
        );
    }

    /// Alg-confusion defense: a token with `alg: none` in the header
    /// MUST be rejected even though its (empty) signature "verifies".
    /// This is the textbook JWT vulnerability; Validation::new(EdDSA)
    /// pins the algorithm list.
    #[test]
    fn alg_none_rejected() {
        let key = key_from_seed([0x42; 32]);
        let claims = test_claims(3600);

        // Build a token with alg:none by hand. Empty signature part.
        let header = r#"{"alg":"none","typ":"JWT"}"#;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let payload = serde_json::to_vec(&claims).unwrap();
        let unsecured = format!("{}.{}.", b64.encode(header), b64.encode(&payload));

        verify(&unsecured, &key.verifying_key())
            .expect_err("alg:none → reject regardless of signature");
    }

    // ------------------------------------------------------------------------
    // Proptest
    // ------------------------------------------------------------------------

    proptest! {
        /// Sign→verify roundtrip over arbitrary claims and keys.
        ///
        /// `exp` is always future (now + [60s..24h]) so the expiry
        /// check passes — expiry is covered by the dedicated unit
        /// test above; here we want serialization fidelity.
        ///
        /// Key is built from a proptest-generated seed rather than
        /// `SigningKey::generate` — see `key_from_seed` for why.
        /// 32 random bytes is a valid ed25519 seed (every 256-bit
        /// value is; the clamping happens inside the scalar
        /// derivation).
        #[test]
        fn prop_jwt_roundtrip(
            sub_bytes: [u8; 16],
            iat in 0i64..4_000_000_000i64,
            exp_delta in 60i64..86_400i64,
            jti in "[a-zA-Z0-9-]{8,64}",
            key_seed: [u8; 32],
        ) {
            let claims = TenantClaims {
                sub: Uuid::from_bytes(sub_bytes),
                iat,
                // exp is future-relative to NOW, not to iat. iat is
                // just a payload field; jsonwebtoken doesn't cross-
                // check it against exp.
                exp: now() + exp_delta,
                jti,
            };
            let key = key_from_seed(key_seed);

            let token = sign(&claims, &key)?;
            let decoded = verify(&token, &key.verifying_key())?;

            prop_assert_eq!(decoded.sub, claims.sub);
            prop_assert_eq!(decoded.iat, claims.iat);
            prop_assert_eq!(decoded.exp, claims.exp);
            prop_assert_eq!(decoded.jti, claims.jti);
        }

        /// No key collisions across the seed space: token signed
        /// under seed A never verifies under seed B ≠ A.
        ///
        /// The self-precondition assert (seeds differ) is load-
        /// bearing: proptest COULD generate the same 32 bytes twice
        /// (2^-256 odds, but shrinking might converge there). Without
        /// it, the test could spuriously fail on a seed collision and
        /// we'd chase a non-bug.
        #[test]
        fn prop_wrong_key_always_rejected(
            seed_a: [u8; 32],
            seed_b: [u8; 32],
        ) {
            prop_assume!(seed_a != seed_b);

            let key_a = key_from_seed(seed_a);
            let key_b = key_from_seed(seed_b);
            let claims = test_claims(3600);

            let token = sign(&claims, &key_a)?;
            prop_assert!(verify(&token, &key_b.verifying_key()).is_err());
        }
    }

    // base64::Engine trait — in scope for .decode()/.encode() on the
    // engine constants. Importing at module top would lint as unused
    // for the prod code (which doesn't touch base64 directly —
    // jsonwebtoken handles it).
    use base64::Engine;
}
