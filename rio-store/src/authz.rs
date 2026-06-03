//! Per-method credential-class table for every gRPC service bound on
//! the store's single port (9002), enforced by a path-aware tower
//! layer.
//!
//! The store multiplexes four very different caller populations over
//! one port: untrusted builder pods (HMAC assignment tokens), the
//! gateway acting for a tenant (JWT tenant tokens), cluster-internal
//! services (service HMAC tokens), and the kubelet (health checks).
//! Before this layer, which credential a method demanded was implicit
//! in its handler — and a method that demanded none (`TailLog`) was
//! indistinguishable from one whose check was simply missing. The
//! table below makes the credential class an explicit, reviewable
//! property of every bound method, and the layer fails CLOSED on any
//! method that is not declared: adding an RPC without deciding its
//! credential class is a startup-visible test failure and a
//! request-time `PERMISSION_DENIED`, not a silent `Open`.
//!
//! Enforcement is **enforce-when-configured**, mirroring the HMAC
//! dev-mode posture (`gate.rs`) and the scheduler's JWT layer: a class
//! is enforced only when the corresponding verifier is configured, so
//! single-node dev stores and VM scenarios without keys keep working,
//! and configuring a key flips the gate everywhere at once.
//!
//! The layer sits AFTER (inner to) the JWT `InterceptorLayer` in the
//! server stack: the interceptor verifies `x-rio-tenant-token` and
//! attaches [`TenantClaims`] to request extensions; this layer only
//! *requires presence* of the verified claims for `TenantJwt` methods.
//! It never verifies tokens itself — verification stays in exactly one
//! place per token family.

use futures_util::future::{Either, Ready, ready};
use http::{HeaderValue, Request, Response};
use rio_auth::jwt::TenantClaims;
use rio_common::grpc::SERVICE_TOKEN_HEADER;
use std::sync::Arc;
use std::task::{Context, Poll};
use tonic::body::Body;
use tower::{Layer, Service};

/// What a caller must present for a method to be dispatched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialClass {
    /// An `x-rio-assignment-token` header must be present (builder
    /// ingest). The HMAC *binding* — token matches this exec/builder —
    /// stays in the stream gate ([`crate::logs::gate`]), which sees the
    /// first frame; this layer pins presence so a tokenless probe is
    /// rejected before a stream is ever opened. Enforced when the
    /// assignment-HMAC verifier is configured.
    AssignmentToken,
    /// Verified tenant claims must be attached by the JWT interceptor
    /// (the header was present AND verified). Enforced when the JWT
    /// pubkey is configured. Per the owner decision on bug_290 there
    /// is NO service-token bypass for this class: operator tooling
    /// reads logs through the gateway path like every other caller.
    TenantJwt,
    /// Either verified tenant claims or a VERIFIED
    /// `x-rio-service-token` (cluster-internal callers). The layer
    /// verifies the service token itself with the configured store
    /// verifier — presence is not a credential. When a JWT pubkey is
    /// configured but no service key is, the service leg admits
    /// nothing (configure both or use a tenant session); handlers may
    /// keep their own verification as defense in depth. Enforced when
    /// the JWT pubkey is configured.
    ServiceOrTenant,
    /// No transport-level credential. Either the method is genuinely
    /// public (health) or its handler enforces a per-message credential
    /// the transport cannot see (HMAC tokens inside streamed request
    /// bodies); the per-row comments say which.
    Open,
}

/// Builder-presented assignment token header — the shared wire const
/// (the stream gate verifies the HMAC binding; this layer pins
/// presence).
pub use rio_common::grpc::ASSIGNMENT_TOKEN_HEADER;

/// The credential class for every gRPC method bound on 9002.
///
/// `tests::table_covers_all_bound_methods` walks the proto descriptor
/// set and fails if this table and the bound method set ever diverge
/// in either direction — a new RPC cannot ship without a row here.
pub const METHOD_CREDENTIALS: &[(&str, CredentialClass)] = &[
    // ── grpc.health.v1.Health — kubelet probes, genuinely public ──
    ("/grpc.health.v1.Health/Check", CredentialClass::Open),
    ("/grpc.health.v1.Health/Watch", CredentialClass::Open),
    ("/grpc.health.v1.Health/List", CredentialClass::Open),
    // ── rio.store.StoreService ──
    // Builder/fetcher data plane. PutPath* carry the HMAC assignment
    // token inside the first streamed message (the transport header is
    // not where the token rides today); GetPath/QueryPathInfo serve the
    // gateway (JWT) AND builders (token-in-body) — per-handler checks
    // own these. Declared Open at the transport layer, enforced in the
    // handlers; rows exist so the fail-closed default cannot regress
    // them silently.
    ("/rio.store.StoreService/PutPath", CredentialClass::Open),
    (
        "/rio.store.StoreService/PutPathBatch",
        CredentialClass::Open,
    ),
    ("/rio.store.StoreService/GetPath", CredentialClass::Open),
    (
        "/rio.store.StoreService/QueryPathInfo",
        CredentialClass::Open,
    ),
    (
        "/rio.store.StoreService/BatchQueryPathInfo",
        CredentialClass::Open,
    ),
    (
        "/rio.store.StoreService/BatchGetManifest",
        CredentialClass::Open,
    ),
    (
        "/rio.store.StoreService/FindMissingPaths",
        CredentialClass::Open,
    ),
    (
        "/rio.store.StoreService/QueryPathFromHashPart",
        CredentialClass::Open,
    ),
    (
        "/rio.store.StoreService/AddSignatures",
        CredentialClass::Open,
    ),
    (
        "/rio.store.StoreService/RegisterRealisation",
        CredentialClass::Open,
    ),
    (
        "/rio.store.StoreService/QueryRealisation",
        CredentialClass::Open,
    ),
    ("/rio.store.StoreService/TenantQuota", CredentialClass::Open),
    (
        "/rio.store.StoreService/AppendHwPerfSample",
        CredentialClass::Open,
    ),
    // ── rio.store.ChunkService — chunk reads for in-cluster callers
    // (S3-presigned is the bulk path); no per-chunk credential today.
    ("/rio.store.ChunkService/GetChunk", CredentialClass::Open),
    // ── rio.store.StoreAdminService — cluster-internal operators and
    // the controller; service token or a tenant session.
    (
        "/rio.store.StoreAdminService/TriggerGC",
        CredentialClass::ServiceOrTenant,
    ),
    (
        "/rio.store.StoreAdminService/VerifyChunks",
        CredentialClass::ServiceOrTenant,
    ),
    (
        "/rio.store.StoreAdminService/ListUpstreams",
        CredentialClass::ServiceOrTenant,
    ),
    (
        "/rio.store.StoreAdminService/AddUpstream",
        CredentialClass::ServiceOrTenant,
    ),
    (
        "/rio.store.StoreAdminService/RemoveUpstream",
        CredentialClass::ServiceOrTenant,
    ),
    (
        "/rio.store.StoreAdminService/GetLoad",
        CredentialClass::ServiceOrTenant,
    ),
    // ── rio.store.LogService ──
    // AppendLog: untrusted builder pods; token presence pinned here,
    // HMAC binding in the gate (which also re-checks presence).
    (
        "/rio.store.LogService/AppendLog",
        CredentialClass::AssignmentToken,
    ),
    // TailLog: tenant reads only (owner decision, bug_290). The
    // handler additionally checks derivation OWNERSHIP against the
    // verified claims — this row pins that an unauthenticated caller
    // never reaches that handler when JWT is configured.
    ("/rio.store.LogService/TailLog", CredentialClass::TenantJwt),
];

/// Class lookup by full gRPC path (`/package.Service/Method`).
pub fn class_for(path: &str) -> Option<CredentialClass> {
    METHOD_CREDENTIALS
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, c)| *c)
}

/// Tower layer enforcing [`METHOD_CREDENTIALS`].
#[derive(Clone)]
pub struct AuthzLayer {
    /// JWT pubkey configured → `TenantJwt`/`ServiceOrTenant` enforced.
    pub jwt_configured: bool,
    /// Assignment-HMAC verifier configured → `AssignmentToken` enforced.
    pub hmac_configured: bool,
    /// Verifier for the `ServiceOrTenant` service leg. The layer
    /// verifies inline (sync, no I/O) — an unverifying layer is not
    /// constructible without explicitly passing `None`, and `None`
    /// closes the leg rather than degrading to presence.
    pub service_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>,
}

impl<S> Layer<S> for AuthzLayer {
    type Service = AuthzService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        AuthzService {
            inner,
            jwt_configured: self.jwt_configured,
            hmac_configured: self.hmac_configured,
            service_verifier: self.service_verifier.clone(),
        }
    }
}

/// The service produced by [`AuthzLayer`].
#[derive(Clone)]
pub struct AuthzService<S> {
    inner: S,
    jwt_configured: bool,
    hmac_configured: bool,
    service_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>,
}

/// Synthesize a gRPC error response at the http layer (Trailers-Only:
/// status in headers, empty body — same shape tonic itself produces
/// for pre-dispatch failures).
fn grpc_reject(code: tonic::Code, msg: &str) -> Response<Body> {
    let mut resp = Response::new(Body::empty());
    resp.headers_mut()
        .insert("content-type", HeaderValue::from_static("application/grpc"));
    resp.headers_mut().insert(
        "grpc-status",
        HeaderValue::from_str(&(code as i32).to_string())
            .expect("grpc code is always a valid header value"),
    );
    resp.headers_mut().insert(
        "grpc-message",
        HeaderValue::from_str(msg).unwrap_or(HeaderValue::from_static("rejected")),
    );
    *resp.status_mut() = http::StatusCode::OK;
    resp
}

impl<S> AuthzService<S> {
    /// `Some(reject)` if the request must not be dispatched.
    fn check(&self, req: &Request<Body>) -> Option<Response<Body>> {
        let path = req.uri().path();
        let Some(class) = class_for(path) else {
            // Fail CLOSED: a bound-but-undeclared method is a
            // deployment bug, not an open door. (Non-gRPC paths do not
            // reach this layer — GrpcWebLayer and tonic route only
            // /pkg.Service/Method shapes.)
            return Some(grpc_reject(
                tonic::Code::PermissionDenied,
                "method has no declared credential class",
            ));
        };
        match class {
            CredentialClass::Open => None,
            CredentialClass::AssignmentToken => {
                if !self.hmac_configured || req.headers().contains_key(ASSIGNMENT_TOKEN_HEADER) {
                    None
                } else {
                    Some(grpc_reject(
                        tonic::Code::Unauthenticated,
                        "assignment token required",
                    ))
                }
            }
            CredentialClass::TenantJwt => {
                if !self.jwt_configured || req.extensions().get::<TenantClaims>().is_some() {
                    None
                } else {
                    Some(grpc_reject(
                        tonic::Code::Unauthenticated,
                        "tenant token required",
                    ))
                }
            }
            CredentialClass::ServiceOrTenant => {
                if !self.jwt_configured || req.extensions().get::<TenantClaims>().is_some() {
                    return None;
                }
                // The service leg: VERIFY, never trust presence — a
                // spoofable header is not a credential. Sync HMAC
                // verify, no I/O.
                match req.headers().get(SERVICE_TOKEN_HEADER) {
                    Some(raw) => {
                        let ok = raw
                            .to_str()
                            .ok()
                            .zip(self.service_verifier.as_ref())
                            .is_some_and(|(tok, sv)| {
                                sv.verify::<rio_auth::hmac::ServiceClaims>(tok).is_ok()
                            });
                        if ok {
                            None
                        } else {
                            Some(grpc_reject(
                                tonic::Code::Unauthenticated,
                                "service token verification failed",
                            ))
                        }
                    }
                    None => Some(grpc_reject(
                        tonic::Code::Unauthenticated,
                        "service or tenant credential required",
                    )),
                }
            }
        }
    }
}

impl<S> Service<Request<Body>> for AuthzService<S>
where
    S: Service<Request<Body>, Response = Response<Body>>,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Either<Ready<Result<Self::Response, Self::Error>>, S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        match self.check(&req) {
            Some(reject) => Either::Left(ready(Ok(reject))),
            None => Either::Right(self.inner.call(req)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    fn req(path: &str) -> Request<Body> {
        Request::builder()
            .uri(format!("http://store{path}"))
            .body(Body::empty())
            .unwrap()
    }

    const SVC_KEY: &[u8] = b"authz-service-token-test-key-32b";

    fn svc(jwt: bool, hmac: bool) -> AuthzService<EchoOk> {
        svc_with_verifier(
            jwt,
            hmac,
            Some(Arc::new(rio_auth::hmac::HmacVerifier::from_key(
                SVC_KEY.to_vec(),
            ))),
        )
    }

    fn svc_with_verifier(
        jwt: bool,
        hmac: bool,
        service_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>,
    ) -> AuthzService<EchoOk> {
        AuthzLayer {
            jwt_configured: jwt,
            hmac_configured: hmac,
            service_verifier,
        }
        .layer(EchoOk)
    }

    /// Inner service standing in for the tonic router: always OK.
    #[derive(Clone)]
    struct EchoOk;
    impl Service<Request<Body>> for EchoOk {
        type Response = Response<Body>;
        type Error = std::convert::Infallible;
        type Future = Ready<Result<Self::Response, Self::Error>>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, _: Request<Body>) -> Self::Future {
            ready(Ok(Response::new(Body::empty())))
        }
    }

    fn grpc_status(resp: &Response<Body>) -> Option<i32> {
        resp.headers()
            .get("grpc-status")
            .map(|v| v.to_str().unwrap().parse().unwrap())
    }

    async fn call(svc: &mut AuthzService<EchoOk>, r: Request<Body>) -> Response<Body> {
        svc.call(r).await.unwrap()
    }

    // r[verify store.log.method-credential]
    #[tokio::test]
    async fn taillog_tokenless_rejected_when_jwt_configured() {
        let mut s = svc(true, true);
        let resp = call(&mut s, req("/rio.store.LogService/TailLog")).await;
        assert_eq!(
            grpc_status(&resp),
            Some(tonic::Code::Unauthenticated as i32),
            "tokenless TailLog must be rejected when a JWT pubkey is configured"
        );
    }

    #[tokio::test]
    async fn taillog_with_verified_claims_admitted() {
        let mut s = svc(true, true);
        let mut r = req("/rio.store.LogService/TailLog");
        r.extensions_mut().insert(test_claims());
        let resp = call(&mut s, r).await;
        assert_eq!(grpc_status(&resp), None, "verified claims pass the layer");
    }

    #[tokio::test]
    async fn taillog_open_when_jwt_unconfigured() {
        // enforce-when-configured: dev stores without a pubkey keep
        // serving (the interceptor would not have verified anything
        // anyway — there is no key to verify against).
        let mut s = svc(false, true);
        let resp = call(&mut s, req("/rio.store.LogService/TailLog")).await;
        assert_eq!(grpc_status(&resp), None);
    }

    #[tokio::test]
    async fn appendlog_tokenless_rejected_when_hmac_configured() {
        let mut s = svc(true, true);
        let resp = call(&mut s, req("/rio.store.LogService/AppendLog")).await;
        assert_eq!(
            grpc_status(&resp),
            Some(tonic::Code::Unauthenticated as i32)
        );
    }

    #[tokio::test]
    async fn appendlog_with_token_header_admitted() {
        // The layer pins presence; the gate keeps the HMAC binding.
        let mut s = svc(true, true);
        let mut r = req("/rio.store.LogService/AppendLog");
        r.headers_mut().insert(
            ASSIGNMENT_TOKEN_HEADER,
            HeaderValue::from_static("opaque-token"),
        );
        let resp = call(&mut s, r).await;
        assert_eq!(grpc_status(&resp), None);
    }

    /// Presence is not a credential: a garbage service token must be
    /// REJECTED at the layer (pre-fix red: the arm passed on
    /// contains_key alone — a spoofed header bypassed the layer).
    // r[verify store.log.method-credential]
    #[tokio::test]
    async fn service_or_tenant_garbage_token_rejected() {
        let mut s = svc(true, true);
        let mut r = req("/rio.store.StoreAdminService/GetLoad");
        r.headers_mut().insert(
            SERVICE_TOKEN_HEADER,
            HeaderValue::from_static("garbage-not-a-token"),
        );
        let resp = call(&mut s, r).await;
        assert_eq!(
            grpc_status(&resp),
            Some(tonic::Code::Unauthenticated as i32),
            "an unverified service token must not pass the layer"
        );
    }

    #[tokio::test]
    async fn service_or_tenant_valid_token_admitted() {
        let mut s = svc(true, true);
        let tok = rio_auth::hmac::HmacSigner::from_key(SVC_KEY.to_vec()).sign(
            &rio_auth::hmac::ServiceClaims {
                caller: "rio-controller".into(),
                expiry_unix: u64::MAX,
                instance: None,
            },
        );
        let mut r = req("/rio.store.StoreAdminService/GetLoad");
        r.headers_mut()
            .insert(SERVICE_TOKEN_HEADER, tok.parse().unwrap());
        let resp = call(&mut s, r).await;
        assert_eq!(grpc_status(&resp), None, "a verified service token passes");
    }

    #[tokio::test]
    async fn service_leg_closed_when_verifier_unconfigured() {
        // jwt configured, NO service verifier: the service leg admits
        // nothing — presence cannot degrade the gate open.
        let mut s = svc_with_verifier(true, true, None);
        let mut r = req("/rio.store.StoreAdminService/GetLoad");
        r.headers_mut().insert(
            SERVICE_TOKEN_HEADER,
            HeaderValue::from_static("anything-at-all"),
        );
        let resp = call(&mut s, r).await;
        assert_eq!(
            grpc_status(&resp),
            Some(tonic::Code::Unauthenticated as i32)
        );
    }

    #[tokio::test]
    async fn undeclared_method_fails_closed() {
        let mut s = svc(false, false);
        let resp = call(&mut s, req("/rio.store.StoreService/SomeFutureRpc")).await;
        assert_eq!(
            grpc_status(&resp),
            Some(tonic::Code::PermissionDenied as i32),
            "an undeclared method must be rejected even with no verifiers configured"
        );
    }

    #[tokio::test]
    async fn health_always_open() {
        let mut s = svc(true, true);
        let resp = call(&mut s, req("/grpc.health.v1.Health/Check")).await;
        assert_eq!(grpc_status(&resp), None);
    }

    fn test_claims() -> TenantClaims {
        TenantClaims {
            sub: uuid::Uuid::nil(),
            iat: 0,
            exp: i64::MAX,
            jti: String::from("test-jti"),
        }
    }

    /// Walk the compiled proto descriptor set and assert the table
    /// covers EXACTLY the rio.store methods bound on 9002 (plus the
    /// health rows, whose descriptor lives in tonic-health, asserted
    /// by name).
    #[test]
    fn table_covers_all_bound_methods() {
        let fds =
            prost_types::FileDescriptorSet::decode(rio_proto::FILE_DESCRIPTOR_SET).expect("fds");
        // The services main.rs binds on 9002, by proto full name.
        let bound = [
            "rio.store.StoreService",
            "rio.store.ChunkService",
            "rio.store.StoreAdminService",
            "rio.store.LogService",
        ];
        let mut expected: Vec<String> = Vec::new();
        for file in &fds.file {
            let pkg = file.package.clone().unwrap_or_default();
            for svc in &file.service {
                let full = format!("{pkg}.{}", svc.name());
                if bound.contains(&full.as_str()) {
                    for m in &svc.method {
                        expected.push(format!("/{full}/{}", m.name()));
                    }
                }
            }
        }
        assert!(
            !expected.is_empty(),
            "descriptor walk found no bound services — descriptor set or names drifted"
        );
        let declared: Vec<&str> = METHOD_CREDENTIALS.iter().map(|(p, _)| *p).collect();
        for path in &expected {
            assert!(
                declared.contains(&path.as_str()),
                "bound method {path} has no credential-class row — declare it (fail-closed \
                 means it is rejected at runtime until you do)"
            );
        }
        for path in &declared {
            if path.starts_with("/grpc.health.v1.Health/") {
                continue; // tonic-health descriptor not in our set.
            }
            assert!(
                expected.iter().any(|e| e == path),
                "table row {path} matches no bound method — stale row?"
            );
        }
    }
}
