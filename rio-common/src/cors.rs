//! Single home for the dashboard gRPC-Web CORS contract (bug_355).
//!
//! rio-scheduler and rio-store expose different RPCs to the SAME
//! browser SPA and previously carried duplicate `CorsLayer` builders
//! behind a "must agree on the CORS contract" comment — and they had
//! already drifted: the store copy silently `filter_map`ped invalid
//! origins away while the scheduler warned. One constructor + the
//! warn-on-invalid parser; both mains are one-line callers.
//!
//! The three `expose_headers` are what connect-web reads to surface
//! `Status.code`/`.message` to the SPA — without them the browser
//! blocks the trailer headers and every RPC error renders as
//! `Code.Unknown`. An empty origin list (the default) allows no
//! browser origin; native gRPC callers are unaffected by CORS.

use http::{HeaderName, Method};
use tower_http::cors::{AllowOrigin, CorsLayer};

/// Credential headers a browser-resident caller may need to attach to
/// gRPC-Web requests (merged_bug_108). The CORS `allow_headers` list is
/// DERIVED from this set — a credential header added here is allowed
/// in the same edit, so "the SPA holds a token the preflight refuses
/// to let it send" is no longer writable. Today: the tenant session
/// token (`TailLog`/`TenantQuota` are tenant-authenticated; the
/// dashboard itself is registry-declared KeylessOnly, so this entry is
/// the enabling work for any later-funded dashboard credential, not a
/// live send).
// r[impl store.log.consumer-registry]
pub const BROWSER_CREDENTIAL_HEADERS: &[&str] = &[crate::grpc::TENANT_TOKEN_HEADER];

/// Build the dashboard CORS layer from a comma-separated origin list
/// (helm renders `dashboard.cors.allowOrigins | join ","` into both
/// services' env).
pub fn dashboard_cors_layer(cors_allow_origins: &str) -> CorsLayer {
    // Transport headers connect-web always sends, plus the derived
    // browser-credential set.
    let allow = [
        HeaderName::from_static("content-type"),
        HeaderName::from_static("x-grpc-web"),
        HeaderName::from_static("x-user-agent"),
    ]
    .into_iter()
    .chain(
        BROWSER_CREDENTIAL_HEADERS
            .iter()
            .map(|h| HeaderName::from_static(h)),
    );
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(parse_cors_origins(cors_allow_origins)))
        .allow_methods([Method::POST, Method::OPTIONS])
        .allow_headers(allow.collect::<Vec<_>>())
        .expose_headers([
            HeaderName::from_static("grpc-status"),
            HeaderName::from_static("grpc-message"),
            HeaderName::from_static("grpc-status-details-bin"),
        ])
}

/// Parse a comma-separated CORS origin list (helm renders
/// `| join ","`): split, trim, drop empties, drop unparseable WITH A
/// WARNING (the store's silent-drop variant is retired — an operator
/// typo in an origin should be visible in logs, not a mystery CORS
/// block in the browser). Extracted so the split/trim/filter chain is
/// directly assertable — `CorsLayer`'s internal origin list isn't
/// inspectable, so a constructibility check alone is vacuous.
pub fn parse_cors_origins(raw: &str) -> Vec<http::HeaderValue> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|o| {
            http::HeaderValue::from_str(o)
                .inspect_err(|e| tracing::warn!(origin = o, error = %e, "invalid CORS origin"))
                .ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    /// The real browser handshake (merged_bug_108): an OPTIONS
    /// preflight asking to send the tenant session token must be
    /// granted — `allow_headers` is DERIVED from
    /// [`BROWSER_CREDENTIAL_HEADERS`], so adding a browser credential
    /// header cannot silently miss the CORS contract again.
    ///
    /// RED (pre-fix): `access-control-allow-headers` listed only
    /// content-type/x-grpc-web/x-user-agent — the browser refused to
    /// send `x-rio-tenant-token`, so a credentialed dashboard could
    /// never have worked even with a token in hand.
    // r[verify store.log.consumer-registry]
    #[tokio::test]
    async fn preflight_grants_tenant_token_header() {
        use tower::{Layer, ServiceExt};
        let layer = dashboard_cors_layer("http://dash.example");
        let svc = layer.layer(tower::service_fn(
            |_req: http::Request<axum::body::Body>| async {
                Ok::<_, std::convert::Infallible>(http::Response::new(axum::body::Body::empty()))
            },
        ));
        let req = http::Request::builder()
            .method(http::Method::OPTIONS)
            .uri("/rio.store.LogService/TailLog")
            .header("origin", "http://dash.example")
            .header("access-control-request-method", "POST")
            .header(
                "access-control-request-headers",
                crate::grpc::TENANT_TOKEN_HEADER,
            )
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        let allowed = resp
            .headers()
            .get("access-control-allow-headers")
            .map(|v| v.to_str().unwrap_or("").to_ascii_lowercase())
            .unwrap_or_default();
        assert!(
            allowed.contains(crate::grpc::TENANT_TOKEN_HEADER),
            "preflight must allow {}; got allow-headers={allowed:?}",
            crate::grpc::TENANT_TOKEN_HEADER
        );
    }

    /// Moved verbatim from rio-scheduler (the contract's previous
    /// home) when the layer was single-sourced.
    #[test]
    fn parse_cors_origins_contract() {
        assert_eq!(
            parse_cors_origins(" http://a.example , http://b.example ,"),
            vec![
                HeaderValue::from_static("http://a.example"),
                HeaderValue::from_static("http://b.example"),
            ],
            "whitespace trimmed, trailing-comma empty dropped"
        );
        // Unparseable origins (control bytes are rejected by
        // HeaderValue::from_str) are dropped, not propagated.
        assert_eq!(
            parse_cors_origins("http://ok,\x01bad,http://ok2"),
            vec![
                HeaderValue::from_static("http://ok"),
                HeaderValue::from_static("http://ok2"),
            ],
            "invalid origin filtered out, valid siblings kept"
        );
    }
}
