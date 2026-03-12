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
// TODO(phase4b): per-tenant narinfo scoping reads this. Currently write-only.
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
        .and_then(|h| h.strip_prefix("Bearer "));

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
                Err(_) => unauthorized("missing Authorization header"),
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

    static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

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
}
