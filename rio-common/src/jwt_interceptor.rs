//! tonic interceptor for `x-rio-tenant-token` JWT verification.
//!
//! Sits on the scheduler and store gRPC servers (the two binaries the
//! gateway dials). Extracts the `x-rio-tenant-token` metadata header,
//! verifies signature + expiry against the configured ed25519 pubkey,
//! and attaches the decoded [`Claims`] to the request's extensions for
//! handlers to read. The scheduler's `SubmitBuild` handler additionally
//! checks `Claims.jti` against the `jwt_revoked` table — that's a PG
//! lookup, so it lives in the handler, not here (this crate is PG-free).
//!
//! # Permissive on absent header — load-bearing for coexistence
//!
//! A missing `x-rio-tenant-token` header is **pass-through**, not
//! rejection. Three distinct callers hit the same `Server::builder()`
//! this interceptor layers onto, and only ONE of them carries a JWT:
//!
//! | Caller | Header? | Auth mechanism |
//! |---|---|---|
//! | Gateway (JWT mode) | yes | this interceptor |
//! | Workers → `WorkerService`/`StoreService` | no | HMAC assignment tokens |
//! | K8s kubelet → `grpc.health.v1.Health` | no | none (plaintext probe) |
//!
//! If absent-header were a rejection, wiring this via `.layer()` would
//! break worker heartbeats and health probes the moment the pubkey is
//! configured. The alternative — per-service `with_interceptor` wrapping
//! — loses `.max_decoding_message_size()` and diverges types across the
//! Option<key> branches.
//!
//! This is also the spec-mandated behavior: `r[gw.jwt.verify]` says
//! "reject invalid tokens" (present + bad signature/expiry), and
//! `r[gw.jwt.dual-mode]` makes the absent-header SSH-comment fallback
//! path permanent. A present-but-invalid token IS rejected with
//! `UNAUTHENTICATED` — a tampered or expired token never passes.
//!
//! # Hot-swap via `Arc<RwLock>`
//!
//! The pubkey is `Arc<RwLock<VerifyingKey>>` so P0260's SIGHUP handler
//! can swap the key without restarting the server. Read-lock held only
//! for the `jwt::verify` call (microseconds); annual key rotation means
//! the write lock is essentially uncontended. `std::sync::RwLock`, not
//! tokio's — the `Interceptor` trait is sync (`FnMut`, not `async Fn`),
//! and the verify call does no I/O.
//!
//! # `Option<key>` — dev mode gate
//!
//! `None` → interceptor is a no-op pass-through. Lets `main.rs` wire it
//! unconditionally (no type divergence between with/without branches)
//! while P0260 handles the actual ConfigMap mount + key load. Matches
//! the gateway-side `Option<SigningKey>` pattern — JWT is opt-in at
//! both ends, gated on deployment config.

use std::sync::{Arc, RwLock};

use ed25519_dalek::VerifyingKey;
use tonic::{Request, Status};

use crate::jwt::{self, Claims};

/// gRPC metadata key the gateway sets on every outbound call in JWT mode.
///
/// Lowercase: tonic normalizes metadata keys (HTTP/2 header rules).
/// Matches `rio-gateway/src/handler/build.rs` — if the gateway ever
/// renames this, `header_name_matches_gateway_literal` below fails.
pub const TENANT_TOKEN_HEADER: &str = "x-rio-tenant-token";

/// Shared pubkey handle the interceptor reads on every intercepted call.
///
/// `Option` at the type level: `None` means JWT verification is
/// disabled for this process (dev mode / key not yet configured).
/// `Arc<RwLock>` inside: the key itself is hot-swappable for rotation.
pub type JwtPubkey = Option<Arc<RwLock<VerifyingKey>>>;

// r[impl gw.jwt.verify]
/// Build the JWT-verify interceptor closure.
///
/// The returned closure is `Clone` (`InterceptorLayer` requires it):
/// `Option<Arc<_>>` clones the Arc pointer, not the underlying key.
/// Every tonic connection gets its own closure clone, but they all
/// share the single `RwLock<VerifyingKey>` instance.
///
/// Error reporting: on verify failure, the `jsonwebtoken` error's
/// `Display` is threaded into the Status message. That's safe to
/// surface — it says "ExpiredSignature" or "InvalidSignature", not
/// anything about the key material. An operator debugging a 401 can
/// tell expired-vs-tampered without guessing.
pub fn jwt_interceptor(pubkey: JwtPubkey) -> impl tonic::service::Interceptor + Clone {
    move |mut req: Request<()>| -> Result<Request<()>, Status> {
        // ---- Dev-mode bypass ----
        // No pubkey configured → no verification possible → pass through.
        // P0260 wires the key from a K8s ConfigMap mount; until then,
        // this interceptor is inert in every binary that installs it.
        let Some(pubkey) = &pubkey else {
            return Ok(req);
        };

        // ---- Dual-mode bypass ----
        // Header absent → gateway is in SSH-comment fallback mode (or
        // this is a worker/health/admin caller that never sends it).
        // Pass through WITHOUT attaching Claims. Handlers that care
        // about tenant identity fall back to `tenant_name` proto field.
        let Some(raw) = req.metadata().get(TENANT_TOKEN_HEADER) else {
            return Ok(req);
        };

        // ---- Strict verify on present header ----
        // From here down, ANY failure is `UNAUTHENTICATED`. A caller
        // that went to the trouble of setting the header is asserting
        // "I am authenticated via JWT" — a malformed or expired token
        // is an auth failure, not a fallback trigger.
        let token = raw.to_str().map_err(|_| {
            // Metadata values are ASCII-encodable bytes. A non-ASCII
            // token header is either corrupted or adversarial; either
            // way, not a valid JWT (base64url is pure ASCII).
            Status::unauthenticated(format!("{TENANT_TOKEN_HEADER} header is not valid ASCII"))
        })?;

        let claims: Claims = {
            // Read-lock scope: held ONLY for verify. `jwt::verify` is
            // pure compute (ed25519 sig check + JSON parse + exp
            // compare); no await, no I/O. The guard drops at scope
            // exit — before we touch `req.extensions_mut()`.
            //
            // `.expect` on the lock: RwLock poisoning means a prior
            // thread panicked WHILE HOLDING THE WRITE LOCK. The only
            // writer is P0260's SIGHUP handler. If that panicked, the
            // key state is unknown — refusing service is correct.
            // (Realistically: the SIGHUP handler does `*lock = new_key`
            // which can't panic, so this is unreachable.)
            let guard = pubkey
                .read()
                .expect("jwt pubkey RwLock poisoned — SIGHUP key-swap handler panicked");
            jwt::verify(token, &guard)
                .map_err(|e| Status::unauthenticated(format!("JWT verify failed: {e}")))?
        };

        // Attach Claims for handlers. `insert` replaces any prior
        // value of the same type — there won't BE one (this
        // interceptor is the only Claims producer), but the semantics
        // mean a double-layer would be idempotent rather than broken.
        req.extensions_mut().insert(claims);
        Ok(req)
    }
}

// r[verify gw.jwt.verify]
#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use tonic::service::Interceptor;

    /// Deterministic keypair from a fixed seed — same pattern as
    /// `jwt.rs` tests (see the rand_core version-skew note there).
    fn keypair(seed: u8) -> (SigningKey, VerifyingKey) {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        let vk = sk.verifying_key();
        (sk, vk)
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    fn claims(exp_offset: i64) -> Claims {
        let n = now();
        Claims {
            sub: uuid::Uuid::from_u128(0x1234),
            iat: n,
            exp: n + exp_offset,
            jti: "test-jti".into(),
        }
    }

    fn pubkey(vk: VerifyingKey) -> JwtPubkey {
        Some(Arc::new(RwLock::new(vk)))
    }

    /// Build a `Request<()>` with the token header set. Metadata keys
    /// are lowercase-normalized by tonic; using the constant keeps
    /// tests in sync with production.
    fn req_with_token(token: &str) -> Request<()> {
        let mut req = Request::new(());
        req.metadata_mut()
            .insert(TENANT_TOKEN_HEADER, token.parse().unwrap());
        req
    }

    // ------------------------------------------------------------------------
    // Pass-through paths — the two bypass branches
    // ------------------------------------------------------------------------

    /// `pubkey = None` → inert. Dev mode / pre-P0260.
    #[test]
    fn no_pubkey_passes_through() {
        let mut intercept = jwt_interceptor(None);
        // Even WITH a header present, no pubkey means no verify.
        // (We can't verify without a key; passing through is the
        // only non-broken option. The alternative — reject all —
        // would brick dev clusters.)
        let out = intercept.call(req_with_token("garbage")).unwrap();
        assert!(
            out.extensions().get::<Claims>().is_none(),
            "no-pubkey path must not attach Claims — it didn't verify anything"
        );
    }

    /// Header absent → dual-mode fallback. Workers, health probes,
    /// admin tools all take this path. They authenticate through
    /// OTHER mechanisms (HMAC, mTLS client cert, none).
    #[test]
    fn absent_header_passes_through() {
        let (_, vk) = keypair(0x42);
        let mut intercept = jwt_interceptor(pubkey(vk));
        let out = intercept.call(Request::new(())).unwrap();
        assert!(
            out.extensions().get::<Claims>().is_none(),
            "absent header → no Claims attached; handler falls back to tenant_name"
        );
    }

    // ------------------------------------------------------------------------
    // Positive path — valid token → Claims attached
    // ------------------------------------------------------------------------

    #[test]
    fn valid_token_attaches_claims() {
        let (sk, vk) = keypair(0x42);
        let original = claims(3600);
        let token = jwt::sign(&original, &sk).unwrap();

        let mut intercept = jwt_interceptor(pubkey(vk));
        let out = intercept.call(req_with_token(&token)).unwrap();

        let attached = out
            .extensions()
            .get::<Claims>()
            .expect("valid token → Claims attached to extensions");
        // Full struct equality, not just jti — sub is the tenant
        // identity handlers use for authz; it must roundtrip exactly.
        assert_eq!(attached, &original);
    }

    // ------------------------------------------------------------------------
    // Negative paths — every UNAUTHENTICATED mode
    // ------------------------------------------------------------------------

    #[test]
    fn expired_jwt_returns_unauthenticated() {
        let (sk, vk) = keypair(0x42);
        // 1h past. jwt.rs's expired_jwt_rejected test explains why
        // -3600 (not -30): jsonwebtoken's 60s leeway would make a
        // small offset flaky.
        let token = jwt::sign(&claims(-3600), &sk).unwrap();

        let mut intercept = jwt_interceptor(pubkey(vk));
        let status = intercept.call(req_with_token(&token)).unwrap_err();

        assert_eq!(status.code(), tonic::Code::Unauthenticated);
        // The jsonwebtoken error surfaces in the message — operators
        // debugging a 401 can tell expired-vs-bad-sig without
        // cranking up log levels.
        assert!(
            status.message().contains("JWT verify failed"),
            "got: {}",
            status.message()
        );
    }

    /// Wrong key → signature mismatch → UNAUTHENTICATED. A token
    /// minted by a compromised (or just misconfigured) gateway with
    /// the wrong signing key is rejected here.
    #[test]
    fn invalid_jwt_returns_unauthenticated() {
        let (sk_a, _) = keypair(0xAA);
        let (_, vk_b) = keypair(0xBB);
        let token = jwt::sign(&claims(3600), &sk_a).unwrap();

        let mut intercept = jwt_interceptor(pubkey(vk_b));
        let status = intercept.call(req_with_token(&token)).unwrap_err();

        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    /// Garbage in the header → UNAUTHENTICATED, not a panic or a
    /// different code. jsonwebtoken's decode handles this gracefully
    /// (returns an error); we map it. An attacker spraying random
    /// header values gets a uniform 401.
    #[test]
    fn malformed_token_returns_unauthenticated() {
        let (_, vk) = keypair(0x42);
        let mut intercept = jwt_interceptor(pubkey(vk));

        for garbage in ["", "not.a.jwt", "a.b.c.d.e", "🎭"] {
            // The emoji case never reaches jwt::verify — it fails the
            // `to_str()` ASCII check first (metadata values are
            // percent-encoded bytes; parsing a non-ASCII string into
            // MetadataValue would fail anyway, but belt-and-braces).
            // Skip it if `parse()` rejects.
            let Ok(val) = garbage.parse() else { continue };
            let mut req = Request::new(());
            req.metadata_mut().insert(TENANT_TOKEN_HEADER, val);

            let status = intercept.call(req).unwrap_err();
            assert_eq!(
                status.code(),
                tonic::Code::Unauthenticated,
                "garbage {:?} should map to UNAUTHENTICATED",
                garbage
            );
        }
    }

    // ------------------------------------------------------------------------
    // Hot-swap — Arc<RwLock> proves its purpose
    // ------------------------------------------------------------------------

    /// After a write-lock swap, the SAME interceptor instance (no
    /// re-construction) verifies against the new key. P0260's SIGHUP
    /// handler does exactly this: `*pubkey.write().unwrap() = new_vk`.
    ///
    /// Without the RwLock (e.g., plain `Arc<VerifyingKey>`), rotation
    /// would need a server restart — the Arc can't be mutated and
    /// every tonic connection already cloned it.
    #[test]
    fn hot_swap_key_takes_effect_without_rebuild() {
        let (sk_old, vk_old) = keypair(0x01);
        let (sk_new, vk_new) = keypair(0x02);

        let shared = Arc::new(RwLock::new(vk_old));
        let mut intercept = jwt_interceptor(Some(Arc::clone(&shared)));

        // Phase 1: old key active. Old-key token passes; new-key token fails.
        let tok_old = jwt::sign(&claims(3600), &sk_old).unwrap();
        let tok_new = jwt::sign(&claims(3600), &sk_new).unwrap();
        intercept.call(req_with_token(&tok_old)).unwrap();
        intercept.call(req_with_token(&tok_new)).unwrap_err();

        // Hot-swap. Simulates P0260's SIGHUP handler writing the
        // ConfigMap-reloaded key.
        *shared.write().unwrap() = vk_new;

        // Phase 2: new key active. Flipped.
        intercept.call(req_with_token(&tok_old)).unwrap_err();
        let out = intercept.call(req_with_token(&tok_new)).unwrap();
        assert!(
            out.extensions().get::<Claims>().is_some(),
            "post-swap: new-key token verifies AND attaches Claims"
        );
    }

    // ------------------------------------------------------------------------
    // Cross-crate consistency pins
    // ------------------------------------------------------------------------

    /// The gateway hardcodes `"x-rio-tenant-token"` as a string literal
    /// at the injection site (`rio-gateway/src/handler/build.rs`).
    /// This test fails if the constant here drifts. It can't catch the
    /// gateway drifting (that's a cross-crate check tracey can't do),
    /// but a VM test doing a full gateway→scheduler SubmitBuild covers
    /// the end-to-end path.
    #[test]
    fn header_name_matches_gateway_literal() {
        assert_eq!(TENANT_TOKEN_HEADER, "x-rio-tenant-token");
        // And it's already lowercase — tonic would reject a mixed-case
        // metadata key at insert time, but asserting here makes the
        // constraint visible.
        assert_eq!(TENANT_TOKEN_HEADER, TENANT_TOKEN_HEADER.to_lowercase());
    }
}
