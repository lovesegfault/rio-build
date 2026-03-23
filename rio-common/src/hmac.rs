//! HMAC-SHA256 assignment tokens.
//!
//! The scheduler signs each `WorkAssignment.assignment_token` with a
//! shared secret. The store verifies the token on `PutPath` — a worker
//! can only upload outputs that match a valid assignment. This prevents
//! a compromised worker from uploading arbitrary paths (e.g., injecting
//! a backdoored libc).
//!
//! # Token format
//!
//! `base64url(serde_json(Claims)).base64url(hmac_sha256(key, claims_json))`
//!
//! JSON for the claims: self-delimiting, human-debuggable (base64-decode
//! a failing token → readable JSON), no delimiter-safety reasoning for
//! store paths. The `.` separator is URL-safe (base64url alphabet doesn't
//! use it).
//!
//! # mTLS bypass for the gateway
//!
//! The gateway also calls `PutPath` (for `nix copy --to`). It doesn't
//! have an assignment token. Two options: (a) long-lived gateway token
//! with `expected_outputs: *`, (b) mTLS-based bypass where the store
//! checks `request.peer_certs()` and skips HMAC for the gateway cert.
//! We go with (b) — simpler, relies on the mTLS identity. See
//! `rio-store/src/grpc/put_path.rs` for the bypass logic.

use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Claims embedded in an assignment token. The scheduler builds these
/// at dispatch time; the store verifies them on PutPath.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Claims {
    /// Worker the assignment was for. Not checked on verify (the store
    /// doesn't know which worker is calling — mTLS identifies the cert
    /// but not the pod name), but useful for audit logs.
    pub worker_id: String,
    /// Derivation hash. Ties the token to a specific build; a worker
    /// can't reuse one derivation's token for another.
    pub drv_hash: String,
    /// Store paths this token authorizes uploading. The store checks
    /// `ValidatedPathInfo.store_path ∈ expected_outputs` — uploading a
    /// path NOT in this list → PERMISSION_DENIED.
    ///
    /// For floating-CA derivations this is `[""]` at dispatch time (the
    /// output path is computed post-build from the NAR hash) — the store
    /// skips the membership check when [`is_ca`](Self::is_ca) is set.
    pub expected_outputs: Vec<String>,
    /// Floating-CA derivation: output paths are not known at dispatch
    /// time (computed post-build from the NAR hash). When set, the store
    /// skips the `store_path ∈ expected_outputs` check on PutPath.
    ///
    /// Threat model holds: the token is still bound to [`drv_hash`] (a
    /// worker can't upload for a derivation it wasn't assigned), and
    /// `r[store.integrity.verify-on-put]` independently hashes the NAR
    /// stream. A compromised worker can upload a garbage path for ITS
    /// assigned CA build — same blast radius as IA (it could upload
    /// garbage to its known IA output path too).
    ///
    /// `#[serde(default)]` for backward compat: tokens signed before
    /// this field existed deserialize with `is_ca = false` (IA
    /// semantics, the only kind that existed).
    ///
    /// [`drv_hash`]: Self::drv_hash
    #[serde(default)]
    pub is_ca: bool,
    /// Unix timestamp (seconds). Token invalid after this. Scheduler
    /// sets it to ~2× build_timeout; a worker legitimately uploading
    /// after build completion is well within that window. Prevents
    /// replay from a leaked token months later.
    pub expiry_unix: u64,
}

/// Signer: constructed on the scheduler from a key file.
pub struct HmacSigner {
    key: Vec<u8>,
}

/// Verifier: constructed on the store from the SAME key file.
pub struct HmacVerifier {
    key: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum HmacError {
    #[error("key file I/O ({path}): {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("key file is empty")]
    EmptyKey,
    #[error("token format invalid: expected 'claims.hmac', got {0} '.'-separated parts")]
    Format(usize),
    #[error("base64 decode failed: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("JSON decode failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("HMAC verification failed (tampered or wrong key)")]
    InvalidSignature,
    #[error("token expired (expiry={expiry_unix}, now={now_unix})")]
    Expired { expiry_unix: u64, now_unix: u64 },
}

/// Load a key from a file. Returns `Ok(None)` if `path` is `None` —
/// HMAC disabled, not an error. Same pattern as `Signer::load` in
/// rio-store.
///
/// The key file contains raw bytes (no encoding). Operators generate
/// it with e.g. `openssl rand -out /etc/rio/hmac-key 32`. We don't
/// impose a length minimum (hmac handles short keys by padding) but
/// 32 bytes (256 bits) matches SHA-256's output and is the standard
/// recommendation.
fn load_key(path: Option<&std::path::Path>) -> Result<Option<Vec<u8>>, HmacError> {
    let Some(path) = path else {
        return Ok(None);
    };
    let key = std::fs::read(path).map_err(|source| HmacError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    // Trim trailing newline: `echo -n` is the correct way to write
    // a key file, but `echo` (no -n) is an easy mistake. A trailing
    // \n would be part of the key — not WRONG per se (hmac doesn't
    // care), but it means the scheduler's key file and the store's
    // key file must BOTH have or not-have the newline. Trimming
    // makes it forgiving.
    //
    // CRLF first, then LF: a Windows-edited key file has \r\n;
    // stripping only \n would leave \r in the key → scheduler and
    // store key mismatch → all uploads rejected with opaque
    // InvalidSignature.
    let key = key
        .strip_suffix(b"\r\n")
        .or_else(|| key.strip_suffix(b"\n"))
        .map(|s| s.to_vec())
        .unwrap_or(key);
    if key.is_empty() {
        return Err(HmacError::EmptyKey);
    }
    Ok(Some(key))
}

impl HmacSigner {
    /// Load from a key file. `None` path → `None` signer (disabled).
    pub fn load(path: Option<&std::path::Path>) -> Result<Option<Self>, HmacError> {
        Ok(load_key(path)?.map(|key| Self { key }))
    }

    /// Construct from raw key bytes (for tests).
    pub fn from_key(key: Vec<u8>) -> Self {
        Self { key }
    }

    /// Sign claims into a token string.
    ///
    /// Format: `base64url(json(claims)).base64url(hmac(key, json(claims)))`.
    /// The claims JSON is what's signed — the base64 is just transport
    /// encoding.
    pub fn sign(&self, claims: &Claims) -> String {
        let claims_json = serde_json::to_vec(claims)
            .expect("Claims serialization can't fail (no maps with non-string keys)");
        let mut mac = HmacSha256::new_from_slice(&self.key)
            .expect("HMAC::new_from_slice accepts any key length");
        mac.update(&claims_json);
        let tag = mac.finalize().into_bytes();

        // base64url (URL_SAFE_NO_PAD): no '/' (path separator) or
        // '+' (would need URL encoding), no '=' padding (not needed,
        // length is known from the '.' split). The '.' separator is
        // not in base64url's alphabet — unambiguous split.
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!("{}.{}", b64.encode(&claims_json), b64.encode(tag))
    }
}

impl HmacVerifier {
    /// Load from a key file. `None` path → `None` verifier (disabled
    /// = PutPath accepts any caller, dev mode).
    pub fn load(path: Option<&std::path::Path>) -> Result<Option<Self>, HmacError> {
        Ok(load_key(path)?.map(|key| Self { key }))
    }

    /// Construct from raw key bytes (for tests).
    pub fn from_key(key: Vec<u8>) -> Self {
        Self { key }
    }

    /// Verify a token and return its claims.
    ///
    /// Checks signature THEN expiry. Signature first so we don't
    /// leak timing information about expiry parsing (not a real
    /// concern here — the JSON decode is after sig check anyway —
    /// but good discipline). Constant-time compare via
    /// `Mac::verify_slice` (never `==` on raw tag bytes).
    pub fn verify(&self, token: &str) -> Result<Claims, HmacError> {
        // Split on the single '.'. More parts → someone injected a
        // '.' into claims (impossible with our encoding) or
        // tampered.
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 2 {
            return Err(HmacError::Format(parts.len()));
        }

        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let claims_json = b64.decode(parts[0])?;
        let tag = b64.decode(parts[1])?;

        // Signature check FIRST. Constant-time via verify_slice —
        // the hmac crate's MacError from this is a generic "verify
        // failed" (no detail), which is what we want (don't tell
        // attackers WHICH byte differed).
        let mut mac = HmacSha256::new_from_slice(&self.key)
            .expect("HMAC::new_from_slice accepts any key length");
        mac.update(&claims_json);
        mac.verify_slice(&tag)
            .map_err(|_| HmacError::InvalidSignature)?;

        // Now safe to decode — the JSON came from us (signature
        // verified), so malicious-input concerns don't apply.
        // (serde_json is safe on untrusted input anyway, but the
        // principle holds.)
        let claims: Claims = serde_json::from_slice(&claims_json)?;

        // Expiry check. SystemTime::now → Unix secs. Clock skew
        // concern: scheduler + store + worker clocks may drift.
        // The expiry is ~2× build_timeout (hours); NTP keeps skew
        // under seconds. Non-issue in practice. If clocks are
        // wildly wrong, tokens fail with a clear "Expired" error
        // — debuggable.
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if now_unix > claims.expiry_unix {
            return Err(HmacError::Expired {
                expiry_unix: claims.expiry_unix,
                now_unix,
            });
        }

        Ok(claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KEY: &[u8] = b"test-key-at-least-32-bytes-long!";

    fn test_claims(expiry_offset_secs: i64) -> Claims {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Claims {
            worker_id: "test-worker".into(),
            drv_hash: "abc123".into(),
            expected_outputs: vec![
                "/nix/store/aaa-hello".into(),
                "/nix/store/bbb-hello-dev".into(),
            ],
            is_ca: false,
            expiry_unix: (now as i64 + expiry_offset_secs).max(0) as u64,
        }
    }

    #[test]
    fn sign_verify_roundtrip() {
        let signer = HmacSigner::from_key(TEST_KEY.to_vec());
        let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());

        let claims = test_claims(3600); // 1h future
        let token = signer.sign(&claims);

        let verified = verifier.verify(&token).expect("valid token should verify");
        assert_eq!(verified, claims);
    }

    #[test]
    fn tampered_signature_rejected() {
        let signer = HmacSigner::from_key(TEST_KEY.to_vec());
        let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());

        let token = signer.sign(&test_claims(3600));
        // Decode the signature, flip one byte, re-encode. Doing
        // this at the raw-bytes level (not base64-char-flip)
        // guarantees the result is valid base64 that DECODES to
        // a different byte sequence — so the error is definitely
        // InvalidSignature, not Base64. A char-flip in base64
        // might produce an invalid byte that fails at decode
        // (last-char carries only 2 bits; flipping it wrong
        // gives out-of-range).
        let parts: Vec<&str> = token.split('.').collect();
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let mut sig = b64.decode(parts[1]).unwrap();
        sig[0] ^= 0xFF; // flip all bits in first byte
        let tampered = format!("{}.{}", parts[0], b64.encode(&sig));

        assert!(matches!(
            verifier.verify(&tampered),
            Err(HmacError::InvalidSignature)
        ));
    }

    #[test]
    fn tampered_claims_rejected() {
        let signer = HmacSigner::from_key(TEST_KEY.to_vec());
        let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());

        let token = signer.sign(&test_claims(3600));
        // Decode claims, modify, re-encode with ORIGINAL signature.
        let parts: Vec<&str> = token.split('.').collect();
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let mut claims: Claims = serde_json::from_slice(&b64.decode(parts[0]).unwrap()).unwrap();
        claims
            .expected_outputs
            .push("/nix/store/evil-backdoor".into());
        let tampered_claims = b64.encode(serde_json::to_vec(&claims).unwrap());
        let tampered = format!("{tampered_claims}.{}", parts[1]);

        // Signature was over ORIGINAL claims; tampered claims →
        // signature mismatch.
        assert!(matches!(
            verifier.verify(&tampered),
            Err(HmacError::InvalidSignature)
        ));
    }

    #[test]
    fn expired_token_rejected() {
        let signer = HmacSigner::from_key(TEST_KEY.to_vec());
        let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());

        let claims = test_claims(-60); // expired 1 minute ago
        let token = signer.sign(&claims);

        let result = verifier.verify(&token);
        assert!(
            matches!(result, Err(HmacError::Expired { .. })),
            "expected Expired, got {result:?}"
        );
    }

    #[test]
    fn wrong_key_rejected() {
        let signer = HmacSigner::from_key(TEST_KEY.to_vec());
        let verifier = HmacVerifier::from_key(b"completely-different-key-32bytes".to_vec());

        let token = signer.sign(&test_claims(3600));
        assert!(matches!(
            verifier.verify(&token),
            Err(HmacError::InvalidSignature)
        ));
    }

    #[test]
    fn malformed_token_rejected() {
        let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());

        // No '.'
        assert!(matches!(
            verifier.verify("nodot"),
            Err(HmacError::Format(1))
        ));
        // Too many '.'
        assert!(matches!(
            verifier.verify("a.b.c"),
            Err(HmacError::Format(3))
        ));
        // Bad base64
        assert!(matches!(
            verifier.verify("!!!.!!!"),
            Err(HmacError::Base64(_))
        ));
    }

    #[test]
    fn load_none_returns_none() {
        assert!(HmacSigner::load(None).unwrap().is_none());
        assert!(HmacVerifier::load(None).unwrap().is_none());
    }

    #[test]
    fn load_empty_file_errors() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // Write nothing — empty file.
        assert!(matches!(
            HmacSigner::load(Some(tmp.path())),
            Err(HmacError::EmptyKey)
        ));
    }

    #[test]
    fn load_trims_trailing_newline() {
        // `echo "secret" > keyfile` produces trailing \n. Both
        // signer and verifier should get the SAME key regardless.
        let tmp1 = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp1.path(), b"secret-key-32-bytes-long-here!!\n").unwrap();
        let tmp2 = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp2.path(), b"secret-key-32-bytes-long-here!!").unwrap();

        let signer = HmacSigner::load(Some(tmp1.path())).unwrap().unwrap();
        let verifier = HmacVerifier::load(Some(tmp2.path())).unwrap().unwrap();

        // Sign with newline-file, verify with no-newline-file.
        // Should match (both trimmed to same key).
        let claims = test_claims(3600);
        let token = signer.sign(&claims);
        let verified = verifier
            .verify(&token)
            .expect("trailing newline trimmed → same key → verify succeeds");
        assert_eq!(verified, claims);
    }

    #[test]
    fn load_trims_trailing_crlf() {
        // Windows-edited key file: CRLF line ending. Stripping only
        // \n would leave \r in the key → mismatch with a Unix-edited
        // key file (or a K8s Secret created from a literal).
        let tmp_crlf = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp_crlf.path(), b"secret-key-32-bytes-long-here!!\r\n").unwrap();
        let tmp_bare = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp_bare.path(), b"secret-key-32-bytes-long-here!!").unwrap();

        let signer = HmacSigner::load(Some(tmp_crlf.path())).unwrap().unwrap();
        let verifier = HmacVerifier::load(Some(tmp_bare.path())).unwrap().unwrap();

        let claims = test_claims(3600);
        let token = signer.sign(&claims);
        let verified = verifier
            .verify(&token)
            .expect("CRLF trimmed → same key → verify succeeds");
        assert_eq!(verified, claims);
    }

    /// Tokens signed before the `is_ca` field existed must still
    /// verify (deserialize with `is_ca = false`). Pins the
    /// `#[serde(default)]` annotation — without it, old tokens fail
    /// with `missing field is_ca` and every in-flight build's PutPath
    /// gets PERMISSION_DENIED during a rolling deploy.
    #[test]
    fn missing_is_ca_defaults_false() {
        let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());
        // Hand-craft a token WITHOUT is_ca (what an old scheduler
        // would sign). Can't use Claims — it always serializes the
        // field now.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let old_json = serde_json::json!({
            "worker_id": "w",
            "drv_hash": "d",
            "expected_outputs": ["/nix/store/p"],
            "expiry_unix": now + 3600,
        });
        let claims_json = serde_json::to_vec(&old_json).unwrap();
        let mut mac = HmacSha256::new_from_slice(TEST_KEY).unwrap();
        mac.update(&claims_json);
        let tag = mac.finalize().into_bytes();
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let token = format!("{}.{}", b64.encode(&claims_json), b64.encode(tag));

        let claims = verifier
            .verify(&token)
            .expect("token without is_ca field should verify (serde default)");
        assert!(!claims.is_ca, "missing is_ca should default to false");
    }

    /// Token is human-debuggable: base64-decode the first part →
    /// readable JSON. Not a functional test, more a documentation-
    /// by-example that the format is what we claimed.
    #[test]
    fn token_is_human_debuggable() {
        let signer = HmacSigner::from_key(TEST_KEY.to_vec());
        let claims = test_claims(3600);
        let token = signer.sign(&claims);

        let claims_b64 = token.split('.').next().unwrap();
        let claims_json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(claims_b64)
            .unwrap();
        let parsed: Claims = serde_json::from_slice(&claims_json).unwrap();
        assert_eq!(parsed, claims);
        // The JSON string is human-readable (operator can debug a
        // failing token by base64-decoding the first part).
        let json_str = String::from_utf8(claims_json).unwrap();
        assert!(json_str.contains("\"worker_id\":\"test-worker\""));
    }
}
