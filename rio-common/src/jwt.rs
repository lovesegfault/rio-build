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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
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
pub fn sign(claims: &Claims, key: &SigningKey) -> Result<String, JwtError> {
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
pub fn verify(token: &str, pubkey: &VerifyingKey) -> Result<Claims, JwtError> {
    // Raw 32-byte compressed Edwards point — NOT the SPKI DER
    // wrapper (`to_public_key_der()`). The name `from_ed_der` is a
    // misnomer in jsonwebtoken v9: it just stores the bytes, and
    // ring's `ED25519.verify` expects raw bytes. SPKI DER is ~44
    // bytes with ASN.1 framing; ring would try to read that as a
    // malformed 44-byte key → InvalidSignature on every verify. The
    // sign-side IS genuinely PKCS#8 DER (ring parses it via
    // `Ed25519KeyPair::from_pkcs8`) — the API is asymmetric.
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

    let token_data = jsonwebtoken::decode::<Claims>(token, &decoding_key, &validation)?;
    Ok(token_data.claims)
}
