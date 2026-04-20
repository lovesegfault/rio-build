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
//! # Gateway bypass: service-identity tokens
//!
//! The gateway also calls `PutPath` (for `nix copy --to`). It doesn't
//! have an assignment token. The store grants bypass when the request
//! carries an `x-rio-service-token` header signed with a SEPARATE HMAC
//! key (`RIO_SERVICE_HMAC_KEY_PATH`) and `caller` is in the store's
//! allowlist. The gateway mints a fresh [`ServiceClaims`] per call
//! (60s expiry — sub-µs to sign, no caching).

use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Claims types signed by [`HmacSigner`] / verified by [`HmacVerifier`].
///
/// The trait exists so `sign`/`verify` are generic over the claims
/// shape — [`AssignmentClaims`] (scheduler→builder) and
/// [`ServiceClaims`] (gateway→store) share the envelope and expiry
/// machinery without duplicating sign/verify. The expiry accessor is
/// the only behaviour [`HmacVerifier::verify`] needs beyond serde.
// r[impl sec.authz.service-token]
pub trait HmacClaims: Serialize + serde::de::DeserializeOwned {
    fn expiry_unix(&self) -> u64;
}

/// Claims embedded in an assignment token. The scheduler builds these
/// at dispatch time; the store verifies them on PutPath.
///
/// Named `AssignmentClaims` (not bare `Claims`) to disambiguate from
/// [`crate::jwt::TenantClaims`] — both appear together in PutPath
/// handlers, and `hmac::Claims` vs `jwt::Claims` was a recurring
/// source of confusion.
// r[impl common.hmac.claims]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AssignmentClaims {
    /// Worker the assignment was for. Not checked on verify (the store
    /// doesn't know which worker is calling — mTLS identifies the cert
    /// but not the pod name), but useful for audit logs.
    pub executor_id: String,
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
    /// time (computed post-build from the NAR hash). When set, the
    /// store skips the `store_path ∈ expected_outputs` check on
    /// PutPath and instead RECOMPUTES the CA store path server-side
    /// from the verified NAR hash (`r[sec.authz.ca-path-derived]`),
    /// rejecting on mismatch. The `'uploading'` placeholder is NOT
    /// claimed until that recompute passes, so a worker holding an
    /// `is_ca=true` token cannot upload to (or squat the placeholder
    /// for) a path that doesn't match the content it actually sent —
    /// same blast radius as IA (one content-determined path per NAR).
    pub is_ca: bool,
    /// Unix timestamp (seconds). Token invalid after this. Scheduler
    /// sets it to ~2× build_timeout; a worker legitimately uploading
    /// after build completion is well within that window. Prevents
    /// replay from a leaked token months later.
    pub expiry_unix: u64,
}

impl HmacClaims for AssignmentClaims {
    fn expiry_unix(&self) -> u64 {
        self.expiry_unix
    }
}

/// Claims for a service-identity token. Minted by trusted control-plane
/// callers (gateway) on each `PutPath`; verified by the store as the
/// HMAC-bypass condition. Transport-agnostic replacement for the mTLS
/// CN-allowlist check.
///
/// Signed with a SEPARATE key from [`AssignmentClaims`] so a leaked
/// assignment key (or a stolen assignment token) cannot satisfy
/// `verify::<ServiceClaims>` — wrong key → `InvalidSignature`, and the
/// serde shape diverges (`ServiceClaims` lacks `drv_hash`/
/// `expected_outputs`) as a second independent reject.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServiceClaims {
    /// Caller identity. Checked against the store's
    /// `service_bypass_callers` allowlist (default `["rio-gateway"]`).
    pub caller: String,
    /// Unix seconds. Gateway sets `now + 60`; store rejects past
    /// expiry. No nonce — replay-within-60s is a no-op given
    /// idempotent PutPath (`r[store.put.idempotent]`).
    pub expiry_unix: u64,
}

impl HmacClaims for ServiceClaims {
    fn expiry_unix(&self) -> u64 {
        self.expiry_unix
    }
}

/// Claims for an executor-identity token. Minted by the scheduler per
/// `SpawnIntent`, threaded through the controller as the
/// `RIO_EXECUTOR_TOKEN` pod env var, presented by builders on
/// `BuildExecution` open and every `Heartbeat` as
/// `x-rio-executor-token`. The scheduler verifies it to bind a
/// stream/heartbeat to the intent the pod was spawned for — a
/// compromised pod cannot hijack another pod's stream or spoof its
/// heartbeat fields, because it cannot mint a token for a different
/// `intent_id`.
///
/// Signed with the SAME assignment-HMAC key as [`AssignmentClaims`]:
/// both are scheduler-minted, scheduler-verified; the serde shape
/// (`intent_id` vs `drv_hash`/`expected_outputs`) provides the
/// cross-type isolation (`deny_unknown_fields`).
// r[impl sec.executor.identity-token+2]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutorClaims {
    /// `SpawnIntent.intent_id` (= drv_hash) the token authorizes.
    /// Checked against `HeartbeatRequest.intent_id` and the actor's
    /// `ExecutorState.intent_id` on reconnect.
    pub intent_id: String,
    /// `SpawnIntent.kind` (proto `ExecutorKind` wire i32: 0=Builder,
    /// 1=Fetcher) the token authorizes. Checked against
    /// `HeartbeatRequest.kind` — `kind` decides the FOD/non-FOD
    /// airgap routing, and the worker is NOT trusted (a compromised
    /// open-egress Fetcher heartbeating `kind=Builder` would
    /// otherwise receive non-FOD builds with secret inputs).
    ///
    /// Stored as the wire i32 (not the proto enum) because rio-auth
    /// is dependency-minimal and the proto enum doesn't derive serde;
    /// callers convert via `ExecutorKind::try_from(claims.kind)`.
    pub kind: i32,
    /// Unix seconds. Scheduler sets `now + deadline_secs + grace`;
    /// pod outliving its `activeDeadlineSeconds` is a bug, so the
    /// token outliving the pod is fine.
    pub expiry_unix: u64,
}

impl HmacClaims for ExecutorClaims {
    fn expiry_unix(&self) -> u64 {
        self.expiry_unix
    }
}

/// tonic client interceptor that mints `x-rio-service-token` (HMAC-signed
/// [`ServiceClaims`]) on every outgoing request.
///
/// Shared by every control-plane caller of a service-token-gated RPC:
/// rio-controller (`caller="rio-controller"`) and rio-cli
/// (`caller="rio-cli"`). The verifier side checks `claims.caller` against
/// a per-RPC allowlist. `signer = None` → no-op (dev-mode pass-through;
/// the verifier is also `None` in that mode). See
/// `r[sec.authz.service-token]`.
#[derive(Clone)]
pub struct ServiceTokenInterceptor {
    signer: Option<std::sync::Arc<HmacSigner>>,
    caller: &'static str,
}

impl ServiceTokenInterceptor {
    pub fn new(signer: Option<std::sync::Arc<HmacSigner>>, caller: &'static str) -> Self {
        Self { signer, caller }
    }
}

impl tonic::service::Interceptor for ServiceTokenInterceptor {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(signer) = &self.signer {
            let claims = ServiceClaims {
                caller: self.caller.to_string(),
                // 60s: sub-µs to sign, no caching. Same window as the
                // gateway's PutPath token.
                expiry_unix: crate::now_unix()
                    .map_err(|e| tonic::Status::internal(e.to_string()))?
                    + 60,
            };
            // sign() output is base64url + '.' — always ASCII.
            if let Ok(v) = signer.sign(&claims).parse() {
                req.metadata_mut()
                    .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, v);
            }
        }
        Ok(req)
    }
}

/// Gate for control-plane-only RPCs. Verifies `x-rio-service-token`
/// (HMAC-signed [`ServiceClaims`]) and checks `claims.caller ∈ allowed`.
/// `verifier == None` → dev-mode pass-through (parity with the
/// assignment-token verifier; returns synthetic `caller="dev-mode"`).
///
/// Shared by scheduler `AdminService` and store `StoreAdminService` —
/// both share a port with builder-reachable services, so without this
/// gate a compromised builder could call `AddUpstream` cross-tenant
/// (cache poisoning) or `AppendInterruptSample` (poison λ\[h\]). See
/// `r[sec.authz.service-token]`.
// r[impl sec.authz.service-token]
pub fn ensure_service_caller(
    md: &tonic::metadata::MetadataMap,
    verifier: Option<&HmacVerifier>,
    allowed: &[&str],
) -> Result<ServiceClaims, tonic::Status> {
    let Some(verifier) = verifier else {
        return Ok(ServiceClaims {
            caller: "dev-mode".to_string(),
            expiry_unix: u64::MAX,
        });
    };
    let tok = md
        .get(rio_common::grpc::SERVICE_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            tonic::Status::permission_denied(format!(
                "{} header required for this RPC",
                rio_common::grpc::SERVICE_TOKEN_HEADER
            ))
        })?;
    let claims = verifier
        .verify::<ServiceClaims>(tok)
        .map_err(|e| tonic::Status::permission_denied(format!("service token: {e}")))?;
    if !allowed.contains(&claims.caller.as_str()) {
        return Err(tonic::Status::permission_denied(format!(
            "service-token caller {:?} not in allowlist {allowed:?}",
            claims.caller
        )));
    }
    Ok(claims)
}

/// Shared HMAC key. The scheduler signs, the store verifies — same key
/// file, same field, and a process is one role or the other (never
/// both), so a single struct with both methods is sufficient. The
/// [`HmacSigner`]/[`HmacVerifier`] aliases keep call sites readable.
pub struct HmacKey {
    key: Vec<u8>,
}

/// Scheduler-side alias. See [`HmacKey`].
pub type HmacSigner = HmacKey;
/// Store-side alias. See [`HmacKey`].
pub type HmacVerifier = HmacKey;

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
    #[error(transparent)]
    Clock(#[from] crate::ClockBeforeEpoch),
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

impl HmacKey {
    /// Load from a key file. `None` path → `None` key (HMAC disabled).
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
    pub fn sign<C: HmacClaims>(&self, claims: &C) -> String {
        let claims_json = serde_json::to_vec(claims)
            .expect("HmacClaims serialization can't fail (no maps with non-string keys)");
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

    /// Verify a token and return its claims.
    ///
    /// Checks signature THEN expiry. Signature first so we don't
    /// leak timing information about expiry parsing (not a real
    /// concern here — the JSON decode is after sig check anyway —
    /// but good discipline). Constant-time compare via
    /// `Mac::verify_slice` (never `==` on raw tag bytes).
    pub fn verify<C: HmacClaims>(&self, token: &str) -> Result<C, HmacError> {
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
        let claims: C = serde_json::from_slice(&claims_json)?;

        // Expiry check. SystemTime::now → Unix secs. Clock skew
        // concern: scheduler + store + worker clocks may drift.
        // The expiry is ~2× build_timeout (hours); NTP keeps skew
        // under seconds. Non-issue in practice. If clocks are
        // wildly wrong, tokens fail with a clear "Expired" error
        // — debuggable. Pre-epoch clock → `Clock` error (NOT the
        // old `unwrap_or(0)`, which silently accepted every token).
        let now_unix = crate::now_unix()?;
        if now_unix > claims.expiry_unix() {
            return Err(HmacError::Expired {
                expiry_unix: claims.expiry_unix(),
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

    fn test_claims(expiry_offset_secs: i64) -> AssignmentClaims {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        AssignmentClaims {
            executor_id: "test-builder".into(),
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

        let verified = verifier
            .verify::<AssignmentClaims>(&token)
            .expect("valid token should verify");
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
            verifier.verify::<AssignmentClaims>(&tampered),
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
        let mut claims: AssignmentClaims =
            serde_json::from_slice(&b64.decode(parts[0]).unwrap()).unwrap();
        claims
            .expected_outputs
            .push("/nix/store/evil-backdoor".into());
        let tampered_claims = b64.encode(serde_json::to_vec(&claims).unwrap());
        let tampered = format!("{tampered_claims}.{}", parts[1]);

        // Signature was over ORIGINAL claims; tampered claims →
        // signature mismatch.
        assert!(matches!(
            verifier.verify::<AssignmentClaims>(&tampered),
            Err(HmacError::InvalidSignature)
        ));
    }

    #[test]
    fn expired_token_rejected() {
        let signer = HmacSigner::from_key(TEST_KEY.to_vec());
        let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());

        let claims = test_claims(-60); // expired 1 minute ago
        let token = signer.sign(&claims);

        let result = verifier.verify::<AssignmentClaims>(&token);
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
            verifier.verify::<AssignmentClaims>(&token),
            Err(HmacError::InvalidSignature)
        ));
    }

    #[test]
    fn malformed_token_rejected() {
        let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());

        // No '.'
        assert!(matches!(
            verifier.verify::<AssignmentClaims>("nodot"),
            Err(HmacError::Format(1))
        ));
        // Too many '.'
        assert!(matches!(
            verifier.verify::<AssignmentClaims>("a.b.c"),
            Err(HmacError::Format(3))
        ));
        // Bad base64
        assert!(matches!(
            verifier.verify::<AssignmentClaims>("!!!.!!!"),
            Err(HmacError::Base64(_))
        ));
    }

    #[test]
    fn service_token_interceptor_mints_header_and_noop_when_unsigned() {
        use tonic::service::Interceptor;
        // signer present → header attached, verifies, caller matches.
        let signer = std::sync::Arc::new(HmacSigner::from_key(TEST_KEY.to_vec()));
        let mut int = ServiceTokenInterceptor::new(Some(signer), "rio-cli");
        let req = int.call(tonic::Request::new(())).unwrap();
        let tok = req
            .metadata()
            .get(rio_common::grpc::SERVICE_TOKEN_HEADER)
            .expect("interceptor should attach header")
            .to_str()
            .unwrap();
        let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());
        let claims = verifier.verify::<ServiceClaims>(tok).expect("verifies");
        assert_eq!(claims.caller, "rio-cli");

        // signer=None → no header (dev-mode pass-through).
        let mut noop = ServiceTokenInterceptor::new(None, "rio-cli");
        let req = noop.call(tonic::Request::new(())).unwrap();
        assert!(
            req.metadata()
                .get(rio_common::grpc::SERVICE_TOKEN_HEADER)
                .is_none()
        );
    }

    // r[verify sec.authz.service-token]
    // r[verify store.admin.service-gate]
    #[test]
    fn ensure_service_caller_gates() {
        use tonic::service::Interceptor;
        let key = HmacVerifier::from_key(TEST_KEY.to_vec());
        let signer = std::sync::Arc::new(HmacSigner::from_key(TEST_KEY.to_vec()));

        // None verifier → dev-mode pass-through (synthetic caller).
        let md = tonic::metadata::MetadataMap::new();
        let c = ensure_service_caller(&md, None, &["rio-cli"]).expect("dev-mode passes");
        assert_eq!(c.caller, "dev-mode");

        // Verifier present, no header → PermissionDenied.
        let err = ensure_service_caller(&md, Some(&key), &["rio-cli"]).unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(err.message().contains("required"), "msg: {}", err.message());

        // Valid token, caller in allowlist → Ok.
        let mut int = ServiceTokenInterceptor::new(Some(signer.clone()), "rio-cli");
        let req = int.call(tonic::Request::new(())).unwrap();
        let c = ensure_service_caller(req.metadata(), Some(&key), &["rio-cli", "rio-controller"])
            .expect("allowlisted caller passes");
        assert_eq!(c.caller, "rio-cli");

        // Valid token, caller NOT in allowlist → PermissionDenied.
        let err =
            ensure_service_caller(req.metadata(), Some(&key), &["rio-controller"]).unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(
            err.message().contains("not in allowlist"),
            "msg: {}",
            err.message()
        );

        // Wrong-key token (e.g. assignment key leaked) → PermissionDenied.
        let bad_signer = std::sync::Arc::new(HmacSigner::from_key(b"wrong-key-32-bytes!!!".into()));
        let mut bad = ServiceTokenInterceptor::new(Some(bad_signer), "rio-cli");
        let req = bad.call(tonic::Request::new(())).unwrap();
        let err = ensure_service_caller(req.metadata(), Some(&key), &["rio-cli"]).unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    #[test]
    fn service_claims_roundtrip_and_shape_isolation() {
        let signer = HmacSigner::from_key(TEST_KEY.to_vec());
        let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let svc = ServiceClaims {
            caller: "rio-gateway".into(),
            expiry_unix: now + 60,
        };
        let svc_token = signer.sign(&svc);
        assert_eq!(
            verifier
                .verify::<ServiceClaims>(&svc_token)
                .expect("service token verifies"),
            svc
        );
        // Shape isolation, both directions: a token signed as one
        // claims type cannot verify as the other, even with the
        // correct key — serde rejects the mismatched JSON shape after
        // the signature passes. This is the second independent
        // defence (the primary one is separate keys in production).
        assert!(matches!(
            verifier.verify::<AssignmentClaims>(&svc_token),
            Err(HmacError::Json(_))
        ));
        let asn_token = signer.sign(&test_claims(3600));
        assert!(matches!(
            verifier.verify::<ServiceClaims>(&asn_token),
            Err(HmacError::Json(_))
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
            .verify::<AssignmentClaims>(&token)
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
            .verify::<AssignmentClaims>(&token)
            .expect("CRLF trimmed → same key → verify succeeds");
        assert_eq!(verified, claims);
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
        let parsed: AssignmentClaims = serde_json::from_slice(&claims_json).unwrap();
        assert_eq!(parsed, claims);
        // The JSON string is human-readable (operator can debug a
        // failing token by base64-decoding the first part).
        let json_str = String::from_utf8(claims_json).unwrap();
        assert!(json_str.contains("\"executor_id\":\"test-builder\""));
    }
}
