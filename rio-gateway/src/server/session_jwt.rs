//! Per-SSH-session JWT mint + refresh.
//!
//! Split out of `server/mod.rs`: the mint/refresh helpers and their TTL
//! constants are pure (no russh, no gRPC) and ~280 lines including
//! tests. `ConnectionHandler` calls in via [`mint_session_jwt`] (after
//! `auth_publickey` resolves the tenant UUID) and [`refresh_session_jwt`]
//! (on every `exec_request`).

use std::time::SystemTime;

use ed25519_dalek::SigningKey;
use rio_auth::jwt;
use tracing::{debug, warn};

/// JWT `exp` = mint time + this. `build_ssh_config` sets
/// `keepalive_interval=30s`, so a long build's keepalive replies and
/// stderr stream reset russh's `inactivity_timeout` indefinitely —
/// JWT expiry is the PRIMARY fence on token lifetime, not the second
/// one. A single channel can outlive this (chromium/llvm/ghc easily
/// exceed 65min); [`SessionJwt::token`](crate::handler::SessionJwt::token)
/// re-mints lazily on every access via [`refresh_session_jwt`] so a
/// post-build `wopQueryPathInfo` never sends an expired token.
///
/// Spec (`r[gw.jwt.claims]`) says "SSH session duration + grace" —
/// but we don't know the session duration at mint time. This is the
/// static upper bound. SIGHUP key rotation (T3) swaps the VERIFY
/// key on scheduler/store; tokens minted under the old signing key
/// become unverifiable post-swap. A long session that spans a
/// rotation will see `UNAUTHENTICATED` on its next gRPC call and
/// surface honestly as `NotLocallyHealable` (the gateway's signing
/// key is read once at boot, `main.rs:203-218`, with no reload path
/// — a re-mint signs with the same key the verifier just refused).
/// The client (nix) retries → new SSH connect to a rolled gateway
/// pod → new token under the new key. There is NO in-session heal
/// for rotation; the only heal-able rejection cause is local expiry
/// (`r[gw.jwt.remint-local-expiry-only]`).
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
/// connection churn. See the `Claims.jti` doc in `rio_auth::jwt`.
///
/// Returns `(token, claims)` so callers that want to log `jti`
/// without re-parsing the token can read it directly. The token is
/// opaque to the gateway after this — it's just a string to inject.
pub(crate) fn mint_session_jwt(
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

// r[impl gw.jwt.refresh-on-expiry+2]
/// Re-mint the cached session JWT if it is within
/// `JWT_REFRESH_SLACK_SECS` of expiry. Returns a borrow of the
/// (possibly-refreshed) token string, or `None` if no token is cached
/// (dual-mode fallback / single-tenant).
///
/// Called from [`SessionJwt::token`](crate::handler::SessionJwt::token)
/// on every token access (per outbound gRPC call) and from
/// `exec_request` for the per-channel snapshot. SSH `ControlMaster`
/// keeps one TCP connection alive indefinitely (I-129) AND a single
/// channel can outlive `JWT_SESSION_TTL_SECS` (keepalive resets the
/// inactivity timer, so a >65min build never trips it). Re-mint is
/// purely local: `tenant_id` is `claims.sub` from the cached token,
/// `signing_key` is already on hand — no `ResolveTenant` round-trip.
/// Cheap when fresh: one `SystemTime::now()` + i64 compare.
///
/// On re-mint failure (only possible if the signing key is corrupt —
/// the same key minted the original), the stale token is returned
/// unchanged and a warning logged. The store will reject it with a
/// clear `ExpiredSignature`; that surfaces the problem instead of
/// silently degrading to the `tenant_name` fallback mid-connection.
pub(crate) fn refresh_session_jwt<'a>(
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

/// What the gateway can locally conclude about an `UNAUTHENTICATED`
/// rejection of a token it just injected — the typed cause space the
/// pre-fix `note_rejected` collapsed into one bit (merged_bug_005,
/// R34-w(iii)). Closed enum (R14): the gateway is the ISSUER, so the
/// only fact it can verify without a wire round-trip is its own clock
/// against the token's own `exp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemintCause {
    /// The injected token is past `exp` (or inside the refresh slack)
    /// by the gateway's own clock — re-minting WILL produce a token
    /// the verifier accepts (same key, fresh `exp`, fresh `jti`). The
    /// re-mint is a witnessed recovery: the productive outcome is the
    /// follow-up call succeeding, not the mint itself.
    LocalExpiry,
    /// The injected token is well within its TTL by the gateway's own
    /// clock, so the rejection is something a re-mint CANNOT heal:
    /// per-jti revocation (the operator denied this session — a fresh
    /// jti would silently override the denial), an unknown verify key
    /// (the boot-time signing key in `main.rs:203-218` has no reload
    /// path, so a re-mint signs with the same key the verifier just
    /// rejected), or any other verifier-side refusal. Surfaced
    /// honestly; never re-minted.
    NotLocallyHealable,
}

// r[impl gw.jwt.refresh-on-expiry+2]
/// live_062 + live_064 (the tail blackout and the WatchBuild re-attach
/// death): a `Clone + Send + Sync` token source for every long-lived
/// consumer of the session JWT — the refresh seam the snapshot designs
/// never had. The token is a RENEWABLE obligation of the SESSION,
/// never a frozen capability of any one stream (R32).
///
/// THE CONSUMING FACES (the R32 doctrine census — every face reads
/// [`Self::fresh`] at injection time, never a stored string):
/// - the live-tail relays (live_062): `LogTailSet` used to hold a
///   STRING snapshot ("snapshot semantics", documented in-code at its
///   field — the confession live_062 cashed in: every `TailLog`
///   re-open after mint+65min carried the expired token,
///   `UNAUTHENTICATED` forever, total tail blackout while the build
///   succeeded); each relay clones this source and reads per open.
/// - the build watch stream (live_064): `submit_and_process_build`
///   used to take a submit-time `Option<&str>` snapshot and replay it
///   on every WatchBuild re-attach — the owner's 72-min build died
///   with eleven identical UNAUTHENTICATED re-attach cycles while the
///   gateway re-minted the session token microseconds AFTER failing
///   the build; the initial submit and every re-attach now read this
///   source at injection time.
///
/// Derivation note (recorded): the build-scoped long-TTL read-token
/// alternative was REJECTED — TTL >= build deadline is unbounded for
/// unbounded builds, it weakens the revocation posture (a new
/// long-lived token class), and the refresh path already exists with
/// its own spec rule (the scheduler-stream precedent this extends).
pub(crate) struct SessionTokenSource {
    inner: Option<std::sync::Arc<SessionTokenInner>>,
}

impl Clone for SessionTokenSource {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

struct SessionTokenInner {
    /// The cached `(token, claims)` + the force-re-mint flag set by
    /// [`SessionTokenSource::note_rejected`]. One uncontended lock per
    /// tail OPEN (reconnects are backoff-paced; never per-chunk).
    cached: std::sync::Mutex<(Option<(String, jwt::TenantClaims)>, bool)>,
    signing_key: Option<std::sync::Arc<SigningKey>>,
}

impl SessionTokenSource {
    /// Dual-mode fallback / single-tenant: no JWT was minted for the
    /// session; [`Self::fresh`] always answers `None` and the store's
    /// tenant check rides the proto body's `tenant_name` instead.
    pub(crate) fn none() -> Self {
        Self { inner: None }
    }

    /// Construct from the session's cached mint + signing key (both
    /// cloned off `SessionJwt` — see `SessionJwt::tail_source`).
    pub(crate) fn new(
        cached: Option<(String, jwt::TenantClaims)>,
        signing_key: Option<std::sync::Arc<SigningKey>>,
    ) -> Self {
        if cached.is_none() && signing_key.is_none() {
            return Self::none();
        }
        Self {
            inner: Some(std::sync::Arc::new(SessionTokenInner {
                cached: std::sync::Mutex::new((cached, false)),
                signing_key,
            })),
        }
    }

    /// A token fit for ONE outbound injection (a `TailLog` open, the
    /// initial `SubmitBuild`, a `WatchBuild` re-attach): re-minted through
    /// [`refresh_session_jwt`] when within the slack of expiry, or
    /// unconditionally when the previous open was rejected
    /// `UNAUTHENTICATED` AND [`Self::note_rejected`] judged the cause
    /// locally heal-able ([`RemintCause::LocalExpiry`]). Returns an
    /// owned snapshot: the consumer attaches it to exactly one
    /// injection and re-asks next time.
    pub(crate) fn fresh(&self) -> Option<String> {
        let inner = self.inner.as_ref()?;
        let mut guard = inner.cached.lock().expect("tail token lock poisoned");
        let (cached, force) = &mut *guard;
        if *force {
            // One forced re-mint per LocalExpiry observation: clear
            // the flag BEFORE attempting so a mint failure (corrupt
            // key) degrades to the stale token + the store's clear
            // error, never a mint-spam loop (the reconnect backoff
            // paces the next rejection).
            *force = false;
            if let (Some((_, claims)), Some(key)) = (cached.as_ref(), inner.signing_key.as_ref()) {
                match mint_session_jwt(claims.sub, key) {
                    Ok((token, new_claims)) => {
                        debug!(
                            jti = %new_claims.jti,
                            tenant = %new_claims.sub,
                            "re-minted session JWT after a local-expiry rejection"
                        );
                        metrics::counter!("rio_gateway_jwt_refreshed_total").increment(1);
                        *cached = Some((token, new_claims));
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            "tail JWT forced re-mint failed; keeping stale token"
                        );
                        metrics::counter!("rio_gateway_jwt_refresh_failed_total").increment(1);
                    }
                }
            }
        }
        refresh_session_jwt(cached, inner.signing_key.as_deref()).map(str::to_owned)
    }

    // r[impl gw.jwt.remint-local-expiry-only]
    /// The peer rejected the last injection `UNAUTHENTICATED`: classify
    /// the rejection against the gateway's OWN clock and the cached
    /// `exp`, and arm a forced re-mint only for
    /// [`RemintCause::LocalExpiry`]. Returns the typed cause so the
    /// caller can branch (re-mint+retry vs surface honestly) without
    /// re-deriving it — R34-w(i)/(iii): a re-mint is a recovery claim
    /// and is scoped to causes the issuer can locally verify as
    /// heal-able.
    ///
    /// `NotLocallyHealable` in dual mode (no cached claims): nothing to
    /// re-mint; the rejection is a real authz failure the caller
    /// surfaces.
    pub(crate) fn note_rejected(&self) -> RemintCause {
        let Some(inner) = self.inner.as_ref() else {
            return RemintCause::NotLocallyHealable;
        };
        let mut guard = inner.cached.lock().expect("tail token lock poisoned");
        let (cached, force) = &mut *guard;
        let Some((_, claims)) = cached.as_ref() else {
            return RemintCause::NotLocallyHealable;
        };
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before 1970")
            .as_secs() as i64;
        // The same predicate `refresh_session_jwt` uses (within the
        // slack of expiry), so a token the gateway WOULD have refreshed
        // on its own — had the consumer asked one beat later — is
        // judged LocalExpiry, not a verifier-side surprise. A token
        // well clear of the slack was rejected for a reason a re-mint
        // cannot change (revoked jti, unknown key, malformed).
        if claims.exp - now < JWT_REFRESH_SLACK_SECS {
            *force = true;
            RemintCause::LocalExpiry
        } else {
            RemintCause::NotLocallyHealable
        }
    }
}

// r[verify gw.jwt.issue]
#[cfg(test)]
mod jwt_issuance_tests {
    use super::*;

    /// Fixed-seed key. Same pattern as the `rio-auth/src/jwt.rs` tests —
    /// we never call `SigningKey::generate`; building from seed bytes
    /// gives a deterministic key and mirrors production, where the key
    /// always arrives as seed bytes from a K8s Secret rather than being
    /// generated in-process.
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

    // r[verify gw.jwt.refresh-on-expiry+2]
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

    // r[verify gw.jwt.refresh-on-expiry+2]
    /// `SessionJwt::token()` is the ONLY public read path and ALWAYS
    /// goes through `refresh_session_jwt`. A `SessionJwt` constructed
    /// with an already-expired token returns a fresh one on first
    /// access; second access returns the SAME (now-fresh) string with
    /// no churn. Regression: at b62291b8 the token was a bare
    /// `Option<String>` snapshotted once at `exec_request` — a single
    /// channel running a >65min build would send the stale token on
    /// the post-build `wopQueryPathInfo`.
    #[test]
    fn session_jwt_token_refreshes_per_access() {
        use crate::handler::SessionJwt;
        use std::sync::Arc;

        let tenant_id = uuid::Uuid::from_u128(0xCAFE);
        let key = test_key(0xAA);
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Already-expired cached token — simulates a channel that has
        // been running a long build past JWT_SESSION_TTL_SECS.
        let stale_claims = jwt::TenantClaims {
            sub: tenant_id,
            iat: now - JWT_SESSION_TTL_SECS - 10,
            exp: now - 10,
            jti: "stale-jti".to_string(),
        };
        let mut sj = SessionJwt::new(
            Some(("stale-token".to_string(), stale_claims)),
            Some(Arc::new(key)),
        );

        let first = sj.token().expect("token present").to_owned();
        assert_ne!(first, "stale-token", "first access must re-mint");

        let second = sj.token().expect("token present");
        assert_eq!(
            second, first,
            "second access on a now-fresh token must NOT re-mint (no jti churn)"
        );

        // Dual-mode: none() always yields None.
        assert!(SessionJwt::none().token().is_none());
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

    // r[verify gw.jwt.refresh-on-expiry+2]
    /// live_062 — the tail token source heals expiry: a source built
    /// over an EXPIRED cached token answers `fresh()` with a re-mint,
    /// not the stale string (the pre-fix `LogTailSet` snapshot served
    /// the stale token on every re-open forever — the 65-min
    /// blackout).
    #[test]
    fn tail_source_fresh_heals_expiry() {
        let tenant_id = uuid::Uuid::from_u128(0xBEEF);
        let key = test_key(0x21);
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let stale_claims = jwt::TenantClaims {
            sub: tenant_id,
            iat: now - JWT_SESSION_TTL_SECS - 10,
            exp: now - 10,
            jti: "stale-jti".to_string(),
        };
        let source = SessionTokenSource::new(
            Some(("stale-token".to_string(), stale_claims)),
            Some(std::sync::Arc::new(key)),
        );

        let t1 = source.fresh().expect("token present");
        assert_ne!(t1, "stale-token", "expired snapshot must be re-minted");
        // A clone shares the cache: the refreshed token is visible
        // through every relay's handle (one session, one cache).
        let t2 = source.clone().fresh().expect("token present");
        assert_eq!(t1, t2, "fresh-on-fresh must not churn jti");
    }

    // r[verify gw.jwt.remint-local-expiry-only]
    /// W14-A1 (revoked-jti) + W14-A1b (unknown-key) — merged_bug_005
    /// red-first: a token WELL WITHIN its TTL rejected by the verifier
    /// is `NotLocallyHealable` and `note_rejected` does NOT arm a
    /// re-mint. Pre-fix RED (the wave-13 unconditional force flag): the
    /// healthy token below was re-minted with a FRESH jti — the
    /// scheduler's per-jti revocation healed around in one cycle, and
    /// the rotation case burned the WatchBuild one-shot on a token
    /// signed with the same boot-time key the verifier just refused.
    /// Both verifier-side faces are the same gateway-side observable
    /// (bare Unauthenticated on a healthy token); the typed cause is
    /// what the issuer can locally PROVE, not what the verifier said.
    #[test]
    fn note_rejected_on_healthy_token_is_not_locally_healable() {
        let tenant_id = uuid::Uuid::from_u128(0xF0F0);
        let key = test_key(0x31);
        let (token, claims) = mint_session_jwt(tenant_id, &key).expect("mint");
        let source = SessionTokenSource::new(Some((token, claims)), Some(std::sync::Arc::new(key)));

        let t0 = source.fresh().expect("token");
        let cause = source.note_rejected();
        assert_eq!(
            cause,
            RemintCause::NotLocallyHealable,
            "a token well within TTL rejected by the verifier is not locally heal-able \
             (revoked jti or unknown verify key — neither fixed by re-signing with the same key)"
        );
        let t1 = source.fresh().expect("token");
        assert_eq!(
            t0, t1,
            "NotLocallyHealable must NOT re-mint: pre-fix this minted a fresh jti and \
             silently healed around the operator's per-jti revocation (merged_bug_005)"
        );
        // W14-A3 (the budget face): the force flag was never armed, so
        // the one-shot forced re-mint stays available for a LATER
        // genuine LocalExpiry — the WatchBuild caller's
        // `unauth_remint_spent` is keyed on the LocalExpiry arm only.
    }

    // r[verify gw.jwt.remint-local-expiry-only]
    /// W14-A2 — merged_bug_005's witnessed-recovery positive: a token
    /// PAST `exp` (or inside the slack) rejected by the verifier IS
    /// `LocalExpiry`, `note_rejected` arms exactly ONE forced re-mint,
    /// and the next `fresh()` produces a token with a fresh `exp` the
    /// verifier accepts — the productive outcome that witnesses the
    /// recovery (R34-w(i): the recovery evidence is the re-minted
    /// token verifying, not the mint itself).
    #[test]
    fn note_rejected_on_expired_token_is_local_expiry_and_remints_once() {
        let tenant_id = uuid::Uuid::from_u128(0xF0F1);
        let key = test_key(0x32);
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let stale = jwt::TenantClaims {
            sub: tenant_id,
            iat: now - JWT_SESSION_TTL_SECS - 10,
            exp: now - 10,
            jti: "stale-jti".into(),
        };
        let source = SessionTokenSource::new(
            Some(("stale-token".into(), stale)),
            Some(std::sync::Arc::new(key.clone())),
        );

        let cause = source.note_rejected();
        assert_eq!(
            cause,
            RemintCause::LocalExpiry,
            "a token past exp by the gateway's own clock is the one cause a re-mint can heal"
        );
        let t1 = source.fresh().expect("token");
        assert_ne!(t1, "stale-token", "LocalExpiry must force a re-mint");
        // The witnessed productive outcome: the re-minted token
        // verifies (same signing key, fresh exp).
        let decoded = jwt::verify(&t1, &key.verifying_key()).expect("re-mint must verify");
        assert_eq!(decoded.sub, tenant_id);
        assert!(decoded.exp > now, "re-minted exp must be in the future");
        // The force flag is one-shot: a second fresh() does not churn.
        let t2 = source.fresh().expect("token");
        assert_eq!(t1, t2, "the force flag is one-shot");
    }

    /// Dual-mode: a `none()` source answers `None` forever and
    /// `note_rejected` is `NotLocallyHealable` (nothing to re-mint —
    /// the rejection is a real authz failure the caller surfaces).
    #[test]
    fn tail_source_none_stays_none() {
        let source = SessionTokenSource::none();
        assert!(source.fresh().is_none());
        assert_eq!(source.note_rejected(), RemintCause::NotLocallyHealable);
        assert!(source.fresh().is_none());
        assert!(SessionTokenSource::new(None, None).fresh().is_none());
    }
}
