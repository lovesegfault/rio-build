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

/// Build the dashboard CORS layer from a comma-separated origin list
/// (helm renders `dashboard.cors.allowOrigins | join ","` into both
/// services' env).
pub fn dashboard_cors_layer(cors_allow_origins: &str) -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(parse_cors_origins(cors_allow_origins)))
        .allow_methods([Method::POST, Method::OPTIONS])
        .allow_headers([
            HeaderName::from_static("content-type"),
            HeaderName::from_static("x-grpc-web"),
            HeaderName::from_static("x-user-agent"),
        ])
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
