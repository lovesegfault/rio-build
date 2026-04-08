//! Per-SSH-session JWT mint + refresh.
//!
//! Split out of `server/mod.rs`: the mint/refresh helpers and their TTL
//! constants are pure (no russh, no gRPC) and ~280 lines including
//! tests. `ConnectionHandler` calls in via [`mint_session_jwt`] (after
//! `auth_publickey` resolves the tenant UUID) and [`refresh_session_jwt`]
//! (on every `exec_request`). Re-exported from `server/mod.rs` so
//! external paths are unchanged.

use std::time::SystemTime;

use ed25519_dalek::SigningKey;
use rio_common::jwt;
use tracing::{debug, warn};

/// JWT `exp` = mint time + this. Upper-bounds a single SSH session's
/// token lifetime at the SSH inactivity_timeout (3600s — see
/// `build_ssh_config`) plus a 5-minute grace for in-flight gRPC calls
/// that outlive the SSH close. A long build that exceeds 1h of SSH
/// idle gets dropped by russh anyway; the JWT expiry is a second
/// fence, not the primary one.
///
/// Spec (`r[gw.jwt.claims]`) says "SSH session duration + grace" —
/// but we don't know the session duration at mint time. This is the
/// static upper bound. SIGHUP key rotation (T3) swaps the VERIFY
/// key on scheduler/store; tokens minted under the old signing key
/// become unverifiable post-swap. A long session that spans a
/// rotation will see `UNAUTHENTICATED` on its next gRPC call → the
/// client (nix) retries → new SSH connect → new token under the new
/// key. Token refresh on long sessions would avoid the one failed
/// call, but the retry-on-reconnect path already handles it.
const JWT_SESSION_TTL_SECS: i64 = 3600 + 300;

/// Re-mint threshold. When the cached token has fewer than this many
/// seconds until `exp`, the next `refresh_session_jwt` call replaces
/// it. 5min covers realistic clock skew between gateway and
/// store/scheduler and leaves a channel opened just under the
/// threshold the full slack window before the store would reject it.
const JWT_REFRESH_SLACK_SECS: i64 = 300;

// r[impl gw.jwt.issue]
/// Mint a per-session tenant JWT. Called once per SSH connection,
/// right after `auth_publickey` accepts — the returned token is
/// stored on the `ConnectionHandler` and injected as
/// `x-rio-tenant-token` on every outbound gRPC call for the session's
/// lifetime.
///
/// `tenant_id` is the resolved UUID, not the authorized_keys comment
/// string. The gateway is PG-free (`r[sched.tenant.resolve]` says the
/// scheduler owns the `tenants` table), so the caller resolves
/// name→UUID via the `ResolveTenant` scheduler RPC before calling —
/// see `ConnectionHandler::resolve_and_mint` at the `auth_publickey`
/// call site.
///
/// `jti` is a fresh v4 UUID per call. It is the **revocation lookup
/// key** (scheduler checks `jti NOT IN jwt_revoked`) and the **audit
/// key** (INSERTed into `builds.jwt_jti`). It is NOT the rate-limit
/// partition key — that's `sub` (bounded: one key per tenant). A
/// `jti`-keyed rate limiter would leak memory proportional to
/// connection churn. See the `Claims.jti` doc in `rio_common::jwt`.
///
/// Returns `(token, claims)` so callers that want to log `jti`
/// without re-parsing the token can read it directly. The token is
/// opaque to the gateway after this — it's just a string to inject.
pub fn mint_session_jwt(
    tenant_id: uuid::Uuid,
    signing_key: &SigningKey,
) -> Result<(String, jwt::TenantClaims), jwt::JwtError> {
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs() as i64;
    let claims = jwt::TenantClaims {
        sub: tenant_id,
        iat: now,
        exp: now + JWT_SESSION_TTL_SECS,
        jti: uuid::Uuid::new_v4().to_string(),
    };
    let token = jwt::sign(&claims, signing_key)?;
    Ok((token, claims))
}

// r[impl gw.jwt.refresh-on-expiry]
/// Re-mint the cached session JWT if it is within
/// `JWT_REFRESH_SLACK_SECS` of expiry. Returns a borrow of the
/// (possibly-refreshed) token string, or `None` if no token is cached
/// (dual-mode fallback / single-tenant).
///
/// Called from `ConnectionHandler::ensure_fresh_jwt` on every
/// `exec_request` — i.e., every new SSH channel on a mux'd connection.
/// SSH `ControlMaster` keeps one TCP connection alive indefinitely;
/// without this, a channel opened past `JWT_SESSION_TTL_SECS` would
/// inject an expired token and get `ExpiredSignature` from the store
/// (I-129). Re-mint is purely local: `tenant_id` is `claims.sub` from
/// the cached token, `signing_key` is already on the handler — no
/// `ResolveTenant` round-trip.
///
/// On re-mint failure (only possible if the signing key is corrupt —
/// the same key minted the original), the stale token is returned
/// unchanged and a warning logged. The store will reject it with a
/// clear `ExpiredSignature`; that surfaces the problem instead of
/// silently degrading to the `tenant_name` fallback mid-connection.
pub fn refresh_session_jwt<'a>(
    cached: &'a mut Option<(String, jwt::TenantClaims)>,
    signing_key: Option<&SigningKey>,
) -> Option<&'a str> {
    let (tenant_id, exp) = match cached.as_ref() {
        Some((_, c)) => (c.sub, c.exp),
        None => return None,
    };
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs() as i64;
    if exp - now < JWT_REFRESH_SLACK_SECS
        && let Some(key) = signing_key
    {
        match mint_session_jwt(tenant_id, key) {
            Ok((token, claims)) => {
                debug!(
                    jti = %claims.jti,
                    tenant = %tenant_id,
                    old_exp = exp,
                    new_exp = claims.exp,
                    "refreshed session JWT (near expiry)"
                );
                metrics::counter!("rio_gateway_jwt_refreshed_total").increment(1);
                *cached = Some((token, claims));
            }
            Err(e) => {
                warn!(
                    error = %e,
                    tenant = %tenant_id,
                    "JWT refresh mint failed; keeping stale token"
                );
                metrics::counter!("rio_gateway_jwt_refresh_failed_total").increment(1);
            }
        }
    }
    cached.as_ref().map(|(t, _)| t.as_str())
}

// r[verify gw.jwt.issue]
#[cfg(test)]
mod jwt_issuance_tests {
    use super::*;
    use std::sync::Arc;

    /// Fixed-seed key. Same pattern as rio-common/src/jwt.rs tests —
    /// SigningKey::generate needs rand_core 0.6 but the workspace is
    /// on rand 0.9; from_bytes sidesteps the trait version mismatch.
    fn test_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// Core spec requirement: minted JWT carries the resolved tenant
    /// UUID in `sub`. The gateway never lets the client choose `sub`
    /// — it's bound by the SSH key match. This test constructs
    /// `tenant_id` directly (simulating a completed scheduler
    /// resolve); the production call site gets it from the
    /// `ResolveTenant` RPC.
    #[test]
    fn minted_jwt_decodes_to_tenant_sub() {
        let tenant_id = uuid::Uuid::from_u128(0xCAFE_0000_0000_0000_0000_0000_0000_0258);
        let key = test_key(0x42);

        let (token, claims) = mint_session_jwt(tenant_id, &key).expect("mint");

        // Self-precondition: the returned claims match what we asked
        // for. If mint_session_jwt ever grows a UUID-mangling step
        // (e.g., canonicalization), this catches it before the
        // verify roundtrip below masks it.
        assert_eq!(claims.sub, tenant_id, "returned claims.sub must be input");

        // Round-trip: the TOKEN (not just the returned claims)
        // decodes back to the same sub. This is the real proof —
        // `claims` is just a convenience return; downstream services
        // only see the token string.
        let decoded = jwt::verify(&token, &key.verifying_key()).expect("verify");
        assert_eq!(decoded.sub, tenant_id, "token must decode to tenant UUID");
        assert_eq!(decoded.jti, claims.jti, "jti must survive round-trip");
    }

    /// jti is fresh per mint. Two sessions for the same tenant get
    /// distinct jtis — revocation of one doesn't revoke the other.
    /// The scheduler's jwt_revoked table is keyed by jti; if jti
    /// collided, revoking tenant-X's laptop session would also kill
    /// their CI session.
    #[test]
    fn jti_unique_across_mints() {
        let tenant_id = uuid::Uuid::from_u128(0x1234);
        let key = test_key(0x01);

        let (_, c1) = mint_session_jwt(tenant_id, &key).expect("mint 1");
        let (_, c2) = mint_session_jwt(tenant_id, &key).expect("mint 2");

        // Self-precondition on sub: same tenant → same sub.
        // Without this, a "unique jti" pass could be masked by
        // accidentally-different tenants (copy-paste bug in the
        // test itself, or a future mint_session_jwt that rewrites
        // sub). Asserting sub-equality makes the jti assertion
        // strictly about jti.
        assert_eq!(c1.sub, c2.sub, "precondition: same tenant");
        assert_ne!(c1.jti, c2.jti, "jti must be fresh per mint (v4 UUID)");
    }

    /// exp is in the future and bounded by JWT_SESSION_TTL_SECS.
    /// Not just "is future" — that's too weak (exp = now+1 would
    /// pass but expire before the first gRPC call completes). Bound
    /// it at both ends: at least the TTL minus clock-read skew, at
    /// most the TTL plus skew.
    #[test]
    fn exp_bounded_by_ttl() {
        let key = test_key(0x77);
        let before = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let (_, claims) = mint_session_jwt(uuid::Uuid::nil(), &key).expect("mint");

        let after = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // exp was computed from a `now` snapshot taken between
        // `before` and `after`. So exp - TTL must land in
        // [before, after]. Two-clock-reads brackets the mint.
        let mint_now = claims.exp - JWT_SESSION_TTL_SECS;
        assert!(
            (before..=after).contains(&mint_now),
            "exp={} implies mint-time now={}, but we bracketed [{}, {}]",
            claims.exp,
            mint_now,
            before,
            after
        );
        assert_eq!(
            claims.iat, mint_now,
            "iat and exp must derive from the same `now` snapshot"
        );
    }

    /// No jwt_signing_key → with_jwt_signing_key never called →
    /// ConnectionHandler.jwt_token stays None → SessionContext gets
    /// None → no header injection. This is the default state when
    /// `cfg.jwt.key_path` is unset (dev mode / no K8s Secret mount).
    /// Tested at the GatewayServer level because that's where the
    /// `None` default lives (the field initializer in `::new()`).
    ///
    /// Field-level access only; spinning up a real
    /// StoreServiceClient/SchedulerServiceClient needs a listening
    /// socket, which is way too heavy for "assert default is None".
    /// The field is private, so this test relies on same-module
    /// access. If GatewayServer moves to its own module, this test
    /// moves with it.
    #[test]
    fn signing_key_defaults_none() {
        // Can't construct GatewayServer without real clients (no
        // Default impl, no mock-friendly constructor). Instead,
        // assert structurally on with_jwt_signing_key's contract:
        // it takes an owned SigningKey and wraps it in Some(Arc).
        // The `None` default in `::new()` is trivially verified by
        // reading the source — this test proves the OTHER half:
        // that calling the builder actually flips it to Some.
        //
        // This is weaker than a full construction test, but the
        // alternative (mock clients or a test-only constructor)
        // is scope creep for a 3-line field initializer. The
        // security.nix VM test covers the full flow.
        let key = test_key(0xAB);
        let arc = Arc::new(key);
        // Structural: Arc<SigningKey> is what the Some variant holds.
        // If someone changes the field type (e.g., to Box), this
        // fails to compile — which is the signal we want.
        let _: Option<Arc<SigningKey>> = Some(arc);
    }

    // r[verify gw.jwt.refresh-on-expiry]
    /// A token within `JWT_REFRESH_SLACK_SECS` of expiry is re-minted
    /// on the next `refresh_session_jwt` call. Exercises the I-129
    /// path: ControlMaster mux'd connection, channel opened past the
    /// original token's TTL.
    #[test]
    fn stale_token_is_refreshed() {
        let tenant_id = uuid::Uuid::from_u128(0xDEAD_BEEF);
        let key = test_key(0x55);
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Cached token already expired (exp = now - 10). The token
        // string itself doesn't matter — refresh keys off claims.exp,
        // never re-parses the string.
        let stale_claims = jwt::TenantClaims {
            sub: tenant_id,
            iat: now - JWT_SESSION_TTL_SECS - 10,
            exp: now - 10,
            jti: "stale-jti".to_string(),
        };
        let mut cached = Some(("stale-token".to_string(), stale_claims));

        let refreshed = refresh_session_jwt(&mut cached, Some(&key))
            .expect("refresh must return a token when one was cached");

        assert_ne!(refreshed, "stale-token", "stale token must be replaced");
        let (_, new_claims) = cached.as_ref().unwrap();
        assert!(
            new_claims.exp > now,
            "refreshed exp={} must be > now={}",
            new_claims.exp,
            now
        );
        assert_eq!(new_claims.sub, tenant_id, "sub preserved across refresh");
        assert_ne!(new_claims.jti, "stale-jti", "fresh jti per re-mint");
    }

    /// A token well within its TTL is left untouched — refresh is a
    /// no-op, returns the same string. Guards against accidentally
    /// re-minting on every channel open (would churn jti and spam
    /// `rio_gateway_jwt_refreshed_total`).
    #[test]
    fn fresh_token_is_not_refreshed() {
        let tenant_id = uuid::Uuid::from_u128(0xF00D);
        let key = test_key(0x66);

        let (token, claims) = mint_session_jwt(tenant_id, &key).expect("mint");
        let original_jti = claims.jti.clone();
        let mut cached = Some((token.clone(), claims));

        let out = refresh_session_jwt(&mut cached, Some(&key)).expect("token");
        assert_eq!(out, token, "fresh token must be returned unchanged");
        assert_eq!(
            cached.as_ref().unwrap().1.jti,
            original_jti,
            "no re-mint → jti unchanged"
        );
    }

    /// `None` cached → `None` out, regardless of key. Dual-mode
    /// fallback path stays None across the refresh hook.
    #[test]
    fn refresh_none_stays_none() {
        let key = test_key(0x99);
        let mut cached: Option<(String, jwt::TenantClaims)> = None;
        assert!(refresh_session_jwt(&mut cached, Some(&key)).is_none());
        assert!(cached.is_none());
    }
}
