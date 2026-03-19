//! Bearer token authentication for the binary cache HTTP server.
//!
//! Token→tenant mapping via the `tenants` table: each tenant's
//! `cache_token` column (nullable) is matched against the incoming
//! `Authorization: Bearer <token>` header. A valid token authenticates
//! the request as that tenant (attached as [`AuthenticatedTenant`]
//! extension for future per-tenant scoping in 4b).
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
use sqlx::PgPool;

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
/// request (or `None` for anonymous when `allow_unauthenticated=true`).
// TODO(P0272): per-tenant narinfo scoping reads this. Currently write-only.
#[derive(Clone, Debug)]
pub struct AuthenticatedTenant(pub Option<String>);

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
            match sqlx::query_scalar::<_, String>(
                "SELECT tenant_name FROM tenants WHERE cache_token = $1",
            )
            .bind(t)
            .fetch_optional(&auth.pool)
            .await
            {
                Ok(Some(name)) => {
                    request
                        .extensions_mut()
                        .insert(AuthenticatedTenant(Some(name)));
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
            request.extensions_mut().insert(AuthenticatedTenant(None));
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
    use rio_test_support::TestDb;
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
        let body = String::from_utf8_lossy(&body);
        assert!(
            body.contains("not configured"),
            "503 should explain the misconfiguration: {body}"
        );
    }

    #[tokio::test]
    async fn auth_required_no_header_tokens_exist_401() {
        let db = TestDb::new(&MIGRATOR).await;
        sqlx::query("INSERT INTO tenants (tenant_name, cache_token) VALUES ('team-a', 'secret')")
            .execute(&db.pool)
            .await
            .unwrap();

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
        sqlx::query("INSERT INTO tenants (tenant_name, cache_token) VALUES ('team-a', 'secret')")
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
        sqlx::query("INSERT INTO tenants (tenant_name, cache_token) VALUES ('team-a', 'secret')")
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
        sqlx::query("INSERT INTO tenants (tenant_name, cache_token) VALUES ('misconfigured', '')")
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
        sqlx::query("INSERT INTO tenants (tenant_name, cache_token) VALUES ('ws', '   ')")
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
        sqlx::query("INSERT INTO tenants (tenant_name, cache_token) VALUES ('team-a', 'secret')")
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
        sqlx::query("INSERT INTO tenants (tenant_name, cache_token) VALUES ('team-a', 'secret')")
            .execute(&db.pool)
            .await
            .unwrap();

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
}
