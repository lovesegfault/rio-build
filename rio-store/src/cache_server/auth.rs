//! Bearer token authentication for the binary cache HTTP server.
//!
//! Token→tenant mapping via the `tenants` table: each tenant's
//! `cache_token` column (nullable) is matched against the incoming
//! `Authorization: Bearer <token>` header. A valid token authenticates
//! the request as that tenant (attached as [`AuthenticatedTenant`]
//! extension for per-tenant scoping).
//!
//! Unauthenticated access is NOT the default — `cache_allow_unauthenticated`
//! must be explicitly set to `true`. When `false` and no tenants have
//! tokens configured, requests return 503 with a descriptive message
//! so operators notice the misconfiguration immediately (vs. a silent
//! 401 that looks like "wrong token").

use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use rio_common::tenant::NormalizedName;
use sqlx::PgPool;
use uuid::Uuid;

/// Configuration for the auth middleware.
#[derive(Clone)]
pub struct CacheAuth {
    pub pool: PgPool,
    /// When `true`, requests with no `Authorization` header pass through
    /// (tenant = None). Default `false` — fail loud on unconfigured
    /// deployments.
    pub allow_unauthenticated: bool,
}

/// Attached as an axum `Extension` on authenticated requests. Handlers
/// can `Extension<AuthenticatedTenant>` to see which tenant made the
/// request (both fields `None` for anonymous when
/// `allow_unauthenticated=true`).
///
/// Both fields move together: either both `Some` (token matched a
/// tenant row) or both `None` (anonymous). Not an `Option<(Uuid,
/// NormalizedName)>` because the two fields are read independently —
/// `tenant_id` for the narinfo handler's `path_tenants` JOIN filter
/// (`query_by_hash_part_for_tenant`), `tenant_name` for logging.
///
/// `tenant_name` is [`NormalizedName`] — PG stores already-normalized
/// values (the write path is `CreateTenant` which validates via
/// `NormalizedName::new`), so wrapping on read is an invariant check,
/// not a transformation.
#[derive(Clone, Debug)]
pub struct AuthenticatedTenant {
    pub tenant_id: Option<Uuid>,
    pub tenant_name: Option<NormalizedName>,
}

// r[impl store.cache.auth-bearer]
pub async fn auth_middleware(
    State(auth): State<CacheAuth>,
    mut request: Request,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        // RFC 7235 §2.1: auth-scheme is case-insensitive.
        .and_then(|h| {
            h.get(..7)
                .filter(|p| p.eq_ignore_ascii_case("Bearer "))
                .map(|_| &h[7..])
        })
        .map(str::trim)
        .filter(|t| !t.is_empty());

    match token {
        Some(t) => {
            // Token comparison happens in PG's `WHERE cache_token = $1`.
            // PG's text comparison timing is not an exploitable side-
            // channel at network RTT resolution. If stricter timing-
            // safety is needed, Phase 5 hashes tokens before storage.
            match sqlx::query_as::<_, (Uuid, String)>(
                "SELECT tenant_id, tenant_name FROM tenants WHERE cache_token = $1",
            )
            .bind(t)
            .fetch_optional(&auth.pool)
            .await
            {
                Ok(Some((id, name))) => {
                    // PG stores already-normalized names (CreateTenant
                    // validates via NormalizedName::new before INSERT,
                    // and migration 020 adds a CHECK constraint
                    // enforcing the same invariant). If normalization
                    // rejects a PG-stored name, something bypassed
                    // BOTH — fail the request with 500 so the operator
                    // notices. Don't silently authenticate with
                    // tenant_name=None: that breaks the
                    // `tenant_id.is_some() → tenant_name.is_some()`
                    // invariant downstream code relies on
                    // (authenticated-but-anonymous).
                    //
                    // T3's CHECK constraint makes this branch provably
                    // unreachable for post-migration rows; it remains
                    // a defense for pre-migration rows and a
                    // belt-and-suspenders guard against future write-
                    // path regressions.
                    let tenant_name = match NormalizedName::new(&name) {
                        Ok(n) => n,
                        Err(e) => {
                            tracing::error!(
                                tenant_id = %id,
                                stored_name = %name,
                                error = %e,
                                "PG-stored tenant_name failed normalization — \
                                 write-path bypass (manual INSERT? pre-CHECK row?)"
                            );
                            return (StatusCode::INTERNAL_SERVER_ERROR, "tenant record malformed")
                                .into_response();
                        }
                    };
                    request.extensions_mut().insert(AuthenticatedTenant {
                        tenant_id: Some(id),
                        tenant_name: Some(tenant_name),
                    });
                    next.run(request).await
                }
                Ok(None) => unauthorized("invalid token"),
                Err(e) => {
                    tracing::error!(error = %e, "cache auth: tenant lookup failed");
                    (StatusCode::INTERNAL_SERVER_ERROR, "auth lookup failed").into_response()
                }
            }
        }
        None if auth.allow_unauthenticated => {
            request.extensions_mut().insert(AuthenticatedTenant {
                tenant_id: None,
                tenant_name: None,
            });
            next.run(request).await
        }
        None => {
            // Fail-loud misconfiguration check: if no tenant has a
            // cache_token, the operator forgot to configure auth entirely.
            // 503 + descriptive message surfaces this in the Nix client's
            // error output immediately — vs. a silent 401 that looks like
            // "wrong token".
            match sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM tenants WHERE cache_token IS NOT NULL)",
            )
            .fetch_one(&auth.pool)
            .await
            {
                Ok(true) => unauthorized("missing Authorization header"),
                Ok(false) => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "binary cache auth not configured: set cache_allow_unauthenticated=true \
                     or configure tenant cache_token values",
                )
                    .into_response(),
                Err(e) => {
                    tracing::warn!(error = %e, "cache auth: token-existence check failed; falling back to 401");
                    unauthorized("missing Authorization header")
                }
            }
        }
    }
}

fn unauthorized(msg: &'static str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        msg,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, http::Request as HttpRequest, routing::get};
    use rio_test_support::{TenantSeed, TestDb};
    use tower::ServiceExt;

    use crate::MIGRATOR;

    async fn test_router(auth: CacheAuth) -> Router {
        Router::new()
            .route("/test", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(auth, auth_middleware))
    }

    // r[verify store.cache.auth-bearer]
    #[tokio::test]
    async fn allow_unauthenticated_no_header_passes() {
        let db = TestDb::new(&MIGRATOR).await;
        let app = test_router(CacheAuth {
            pool: db.pool.clone(),
            allow_unauthenticated: true,
        })
        .await;

        let resp = app
            .oneshot(HttpRequest::get("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_required_no_header_no_tokens_503() {
        let db = TestDb::new(&MIGRATOR).await;
        let app = test_router(CacheAuth {
            pool: db.pool.clone(),
            allow_unauthenticated: false,
        })
        .await;

        let resp = app
            .oneshot(HttpRequest::get("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        // Test assertion display, not parse-path.
        #[allow(clippy::disallowed_methods)]
        let body = String::from_utf8_lossy(&body);
        assert!(
            body.contains("not configured"),
            "503 should explain the misconfiguration: {body}"
        );
    }

    #[tokio::test]
    async fn auth_required_no_header_tokens_exist_401() {
        let db = TestDb::new(&MIGRATOR).await;
        TenantSeed::new("team-a")
            .with_cache_token("secret")
            .seed(&db.pool)
            .await;

        let app = test_router(CacheAuth {
            pool: db.pool.clone(),
            allow_unauthenticated: false,
        })
        .await;

        let resp = app
            .oneshot(HttpRequest::get("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            resp.headers().get(header::WWW_AUTHENTICATE).unwrap(),
            "Bearer"
        );
    }

    #[tokio::test]
    async fn valid_token_authenticates() {
        let db = TestDb::new(&MIGRATOR).await;
        TenantSeed::new("team-a")
            .with_cache_token("secret")
            .seed(&db.pool)
            .await;

        let app = test_router(CacheAuth {
            pool: db.pool.clone(),
            allow_unauthenticated: false,
        })
        .await;

        let resp = app
            .oneshot(
                HttpRequest::get("/test")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn invalid_token_401() {
        let db = TestDb::new(&MIGRATOR).await;
        TenantSeed::new("team-a")
            .with_cache_token("secret")
            .seed(&db.pool)
            .await;

        let app = test_router(CacheAuth {
            pool: db.pool.clone(),
            allow_unauthenticated: false,
        })
        .await;

        let resp = app
            .oneshot(
                HttpRequest::get("/test")
                    .header(header::AUTHORIZATION, "Bearer wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// "Bearer " (trailing space, empty token) must NOT authenticate
    /// even if a tenant has cache_token=''. Regression: strip_prefix
    /// yields Some("") which would match WHERE cache_token = '' without
    /// the .filter(!is_empty) guard.
    #[tokio::test]
    async fn empty_bearer_token_rejected_even_with_empty_string_cache_token() {
        let db = TestDb::new(&MIGRATOR).await;
        // Seed a tenant with cache_token='' (operator mistake — e.g.,
        // grpcurl JSON sends "" thinking it means "no cache access").
        // CreateTenant now rejects this, but defend at the auth layer
        // too in case the row was written via direct SQL.
        TenantSeed::new("misconfigured")
            .with_cache_token("")
            .seed(&db.pool)
            .await;

        let app = test_router(CacheAuth {
            pool: db.pool.clone(),
            allow_unauthenticated: false,
        })
        .await;

        let resp = app
            .oneshot(
                HttpRequest::get("/test")
                    .header(header::AUTHORIZATION, "Bearer ")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Empty token → treated as no-header → 401 (tokens exist).
        assert_ne!(
            resp.status(),
            StatusCode::OK,
            "empty bearer must not authenticate"
        );
    }

    /// Same class as the empty-token bypass: "Bearer    " (trailing
    /// whitespace). strip_prefix("Bearer ") → Some("   ") which passed
    /// !is_empty() before the .map(str::trim) guard. Round-3 fix was
    /// incomplete — it caught "" but not "   ".
    #[tokio::test]
    async fn whitespace_only_bearer_token_rejected_even_with_whitespace_cache_token() {
        let db = TestDb::new(&MIGRATOR).await;
        TenantSeed::new("ws")
            .with_cache_token("   ")
            .seed(&db.pool)
            .await;

        let app = test_router(CacheAuth {
            pool: db.pool.clone(),
            allow_unauthenticated: false,
        })
        .await;

        let resp = app
            .oneshot(
                HttpRequest::get("/test")
                    .header(header::AUTHORIZATION, "Bearer    ")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::OK,
            "whitespace-only bearer must not authenticate"
        );
    }

    /// "Bearer  secret" (double space) → strip_prefix → " secret"
    /// (leading space). Without trim this would fail the PG lookup
    /// for cache_token='secret'. With trim it matches. Not a security
    /// issue (fails closed without trim), just a usability fix.
    #[tokio::test]
    async fn bearer_with_leading_whitespace_trimmed_and_matches() {
        let db = TestDb::new(&MIGRATOR).await;
        TenantSeed::new("team-a")
            .with_cache_token("secret")
            .seed(&db.pool)
            .await;

        let app = test_router(CacheAuth {
            pool: db.pool.clone(),
            allow_unauthenticated: false,
        })
        .await;

        let resp = app
            .oneshot(
                HttpRequest::get("/test")
                    .header(header::AUTHORIZATION, "Bearer  secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// RFC 7235 §2.1: auth-scheme is case-insensitive. "bearer secret"
    /// and "BEARER secret" must match. Fails closed without this (not a
    /// security issue) — same usability class as the trim fix above.
    #[tokio::test]
    async fn bearer_scheme_case_insensitive() {
        let db = TestDb::new(&MIGRATOR).await;
        TenantSeed::new("team-a")
            .with_cache_token("secret")
            .seed(&db.pool)
            .await;

        let app = test_router(CacheAuth {
            pool: db.pool.clone(),
            allow_unauthenticated: false,
        })
        .await;

        for scheme in ["bearer secret", "BEARER secret", "BeArEr secret"] {
            let resp = app
                .clone()
                .oneshot(
                    HttpRequest::get("/test")
                        .header(header::AUTHORIZATION, scheme)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "scheme {scheme:?} should auth"
            );
        }
    }

    /// T5 regression for P0367-T2: PG row with a non-normalized
    /// `tenant_name` (write-path bypass — manual INSERT, pre-CHECK
    /// migration, or CreateTenant bug) fails the auth request with
    /// 500, NOT silently authenticating with `tenant_name=None`.
    ///
    /// Before T2, `.ok()` + `debug_assert!` meant release builds
    /// produced `AuthenticatedTenant { tenant_id: Some(id),
    /// tenant_name: None }` — authenticated-but-anonymous, breaking
    /// the `tenant_id.is_some() → tenant_name.is_some()` invariant.
    ///
    /// Mutation target: restoring `.ok()` + `debug_assert!` fails
    /// this test — the request would 200 (auth succeeded, just with a
    /// broken-invariant extension).
    ///
    /// Test setup must bypass the migration-020 CHECK constraint to
    /// seed a bad row. Drop it in test setup — simplest, and the test
    /// is about the auth.rs fail-loud path, not the constraint. (The
    /// constraint is exercised separately in T6.)
    // r[verify store.cache.auth-bearer]
    #[tokio::test]
    async fn non_normalized_pg_name_fails_request() {
        let db = TestDb::new(&MIGRATOR).await;

        // Bypass the CHECK constraint so we can seed a bad row.
        // Migration 020 adds `tenant_name_normalized`; dropping it
        // here simulates a pre-migration database (or a manual-INSERT
        // bypass the constraint would normally block). IF EXISTS so
        // the test works regardless of migration ordering during
        // development.
        sqlx::query("ALTER TABLE tenants DROP CONSTRAINT IF EXISTS tenant_name_normalized")
            .execute(&db.pool)
            .await
            .unwrap();

        // Seed a tenant with an untrimmed name that NormalizedName::new
        // rejects (Err(Empty) after trim → whitespace-only). Direct
        // INSERT — TenantSeed would succeed (the constraint is gone)
        // but using raw SQL makes the bypass explicit.
        sqlx::query("INSERT INTO tenants (tenant_name, cache_token) VALUES ($1, $2)")
            .bind("  bad name  ") // interior + leading/trailing whitespace
            .bind("bad-token")
            .execute(&db.pool)
            .await
            .unwrap();

        let app = test_router(CacheAuth {
            pool: db.pool.clone(),
            allow_unauthenticated: false,
        })
        .await;

        let resp = app
            .oneshot(
                HttpRequest::get("/test")
                    .header(header::AUTHORIZATION, "Bearer bad-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Fail-loud: 500, not 200. The token matched a row (auth
        // technically succeeded at the PG layer) but the name is
        // unrepresentable as a NormalizedName — refuse the request
        // rather than silently breaking the struct invariant.
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "non-normalized PG name must fail-loud (500), not silently \
             degrade to tenant_name=None"
        );
    }
}
