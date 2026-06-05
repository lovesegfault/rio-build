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
//! request-time `PERMISSION_DENIED`, not a silent admit.
//!
//! The class VOCABULARY and the verdict live in [`rio_authz_kernel`]
//! (re-exported here): each keyed class declares exactly one verifier
//! family, the kernel's `decide()` hands each verdict arm a
//! single-knob projection, and a class arm reading a foreign knob
//! does not compile. This module owns CLASSIFICATION — mapping the
//! http request to a [`Presented`] kind *relative to the method's
//! class* — and the wire mapping of rejections. Classification is
//! per-family on purpose: real requests carry credential vectors
//! (the dashboard's nginx injects a service token on TailLog,
//! gateway PutPath attaches a JWT and a service token), and a
//! foreign credential must neither widen nor poison a verdict.
//!
//! Enforcement is **enforce-when-configured**, mirroring the HMAC
//! dev-mode posture (`gate.rs`) and the scheduler's JWT layer: a class
//! is enforced only when its DECLARED verifier is configured, so
//! single-node dev stores and VM scenarios without keys keep working.
//! The dangerous half-keyed states (JWT on, an HMAC key off) are
//! refused at BOOT by [`validate_key_coherence`] — the layer never
//! has to reason about them.
//!
//! The layer sits AFTER (inner to) the JWT `InterceptorLayer` in the
//! server stack: the interceptor verifies `x-rio-tenant-token` and
//! attaches [`TenantClaims`] to request extensions; this layer only
//! *requires presence* of the verified claims for `TenantJwt` methods.
//! It never verifies tenant tokens itself — verification stays in
//! exactly one place per token family. (Service tokens ARE verified
//! here, sync and I/O-free, because no outer layer owns them.)

use futures_util::future::{Either, Ready, ready};
use http::{HeaderValue, Request, Response};
use rio_auth::jwt::TenantClaims;
use rio_common::grpc::SERVICE_TOKEN_HEADER;
use std::sync::Arc;
use std::task::{Context, Poll};
use tonic::body::Body;
use tower::{Layer, Service};

pub use rio_authz_kernel::{
    CredentialClass, HandlerCheck, KeyCoherence, LayerVerdict, Presented, RejectReason,
    VerifierConfig, VerifierFamily, consumes, decide, key_coherence,
};

/// Builder-presented assignment token header — the shared wire const
/// (the stream gate verifies the HMAC binding; this layer pins
/// presence).
pub use rio_common::grpc::ASSIGNMENT_TOKEN_HEADER;

/// The credential class for every gRPC method bound on 9002.
///
/// `tests::table_covers_all_bound_methods` walks the proto descriptor
/// set and fails if this table and the bound method set ever diverge
/// in either direction — a new RPC cannot ship without a row here.
/// There is no catch-all class: a row is keyed (`AssignmentToken`,
/// `TenantJwt`, `Service`), `Public` with a recorded rationale, or
/// `HandlerEnforced` naming the handler check whose typed witness the
/// data path requires.
pub const METHOD_CREDENTIALS: &[(&str, CredentialClass)] = &[
    // ── grpc.health.v1.Health — kubelet probes, genuinely public ──
    (
        "/grpc.health.v1.Health/Check",
        CredentialClass::Public {
            rationale: "kubelet liveness/readiness probe; no caller identity exists",
        },
    ),
    (
        "/grpc.health.v1.Health/Watch",
        CredentialClass::Public {
            rationale: "kubelet liveness/readiness probe; no caller identity exists",
        },
    ),
    (
        "/grpc.health.v1.Health/List",
        CredentialClass::Public {
            rationale: "kubelet liveness/readiness probe; no caller identity exists",
        },
    ),
    // ── rio.store.StoreService ──
    // Builder/fetcher data plane. PutPath* and AppendHwPerfSample
    // carry the HMAC assignment token in the `x-rio-assignment-token`
    // TRANSPORT header, but the handler gate
    // (`verify_assignment_token`) owns a divergent service-caller
    // policy the transport layer cannot express: PutPath* admit an
    // allowlisted `x-rio-service-token` INSTEAD of an assignment
    // token (the gateway/scheduler upload path), while
    // AppendHwPerfSample rejects service callers outright. A layer
    // presence-pin on the assignment header would break the bypass
    // callers, so these rows are handler-enforced and the data path
    // demands the gate's typed witness.
    (
        "/rio.store.StoreService/PutPath",
        CredentialClass::HandlerEnforced {
            check: HandlerCheck::IngestToken,
        },
    ),
    (
        "/rio.store.StoreService/PutPathBatch",
        CredentialClass::HandlerEnforced {
            check: HandlerCheck::IngestToken,
        },
    ),
    // Read-side path metadata: the sig-visibility gate scopes what a
    // tenant session can see; builders read through the same methods
    // with no claims (dual-mode). Gate witnesses ride the data path.
    (
        "/rio.store.StoreService/GetPath",
        CredentialClass::HandlerEnforced {
            check: HandlerCheck::SigVisibility,
        },
    ),
    (
        "/rio.store.StoreService/QueryPathInfo",
        CredentialClass::HandlerEnforced {
            check: HandlerCheck::SigVisibility,
        },
    ),
    // Builder-internal batch surfaces: NO sig-visibility gate by
    // design — end-user tenants are rejected outright instead (the
    // deny-tenants polarity), so the gate-skip cannot be a bypass.
    (
        "/rio.store.StoreService/BatchQueryPathInfo",
        CredentialClass::HandlerEnforced {
            check: HandlerCheck::EndUserRejected,
        },
    ),
    (
        "/rio.store.StoreService/BatchGetManifest",
        CredentialClass::HandlerEnforced {
            check: HandlerCheck::EndUserRejected,
        },
    ),
    // The substitution probe: batch sig-visibility gate
    // (`sig_visibility_gate_batch`'s sole production caller).
    (
        "/rio.store.StoreService/FindMissingPaths",
        CredentialClass::HandlerEnforced {
            check: HandlerCheck::SigVisibility,
        },
    ),
    (
        "/rio.store.StoreService/QueryPathFromHashPart",
        CredentialClass::HandlerEnforced {
            check: HandlerCheck::SigVisibility,
        },
    ),
    (
        "/rio.store.StoreService/AddSignatures",
        CredentialClass::HandlerEnforced {
            check: HandlerCheck::SigVisibility,
        },
    ),
    (
        "/rio.store.StoreService/RegisterRealisation",
        CredentialClass::HandlerEnforced {
            check: HandlerCheck::ServiceCaller,
        },
    ),
    (
        "/rio.store.StoreService/QueryRealisation",
        CredentialClass::Public {
            rationale: "content-addressed CA realisation cache lookup served to \
                        gateway/tenant callers; the returned signatures are themselves \
                        the trust mechanism (clients verify against trusted keys)",
        },
    ),
    // The gateway forwards the SUBMITTING TENANT's JWT for quota
    // reads — this is a tenant surface, not a service one; handler
    // ownership (claims.sub vs the named tenant) closes the
    // cross-tenant read.
    (
        "/rio.store.StoreService/TenantQuota",
        CredentialClass::TenantJwt,
    ),
    (
        "/rio.store.StoreService/AppendHwPerfSample",
        CredentialClass::HandlerEnforced {
            check: HandlerCheck::IngestToken,
        },
    ),
    // ── rio.store.ChunkService — chunk reads for in-cluster callers
    // (S3-presigned is the bulk path); chunk content-hashes act as
    // capability tokens (you can only ask for what a manifest told
    // you about).
    (
        "/rio.store.ChunkService/GetChunk",
        CredentialClass::Public {
            rationale: "chunk reads are keyed by content hash — possession of the hash \
                        is the capability (manifest access is what the gated methods \
                        protect); S3-presigned URLs serve the same bytes",
        },
    ),
    // ── rio.store.StoreAdminService — cluster-internal operators and
    // the controller; a VERIFIED service token, nothing else. (The
    // pre-kernel ServiceOrTenant tenant leg is deleted: every admin
    // handler demanded a service caller anyway, so the leg admitted
    // nothing in the green path and was cluster-admin-for-any-tenant
    // in the half-config state.)
    (
        "/rio.store.StoreAdminService/TriggerGC",
        CredentialClass::Service,
    ),
    (
        "/rio.store.StoreAdminService/VerifyChunks",
        CredentialClass::Service,
    ),
    (
        "/rio.store.StoreAdminService/ListUpstreams",
        CredentialClass::Service,
    ),
    (
        "/rio.store.StoreAdminService/AddUpstream",
        CredentialClass::Service,
    ),
    (
        "/rio.store.StoreAdminService/RemoveUpstream",
        CredentialClass::Service,
    ),
    (
        "/rio.store.StoreAdminService/GetLoad",
        CredentialClass::Service,
    ),
    // ── rio.store.LogService ──
    // AppendLog: untrusted builder pods; token presence pinned here,
    // HMAC binding in the gate (which also re-checks presence).
    (
        "/rio.store.LogService/AppendLog",
        CredentialClass::AssignmentToken,
    ),
    // TailLog: tenant reads only (owner decision, bug_290). The
    // handler additionally requires build-membership OWNERSHIP of the
    // requested execution (store.log.tail-ownership; authorize_tail)
    // — this row pins that an unauthenticated caller never reaches
    // that handler when JWT is configured.
    ("/rio.store.LogService/TailLog", CredentialClass::TenantJwt),
];

/// Which production surface consumes a tenant-authenticated store
/// method (merged_bug_108).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConsumerSurface {
    Gateway,
    Cli,
    Dashboard,
}

/// How that surface obtains the credential the method's class demands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredSource {
    /// Relays the calling tenant's own JWT (the gateway pattern: the
    /// watching/submitting caller's token travels with the request).
    CallerJwtRelay,
    /// Operator-supplied tenant token (CLI flag / env var).
    TenantTokenFlag,
    /// Sends no credential. MUST carry the owner-decision rationale —
    /// a keyless surface against a keyed method is a broken surface in
    /// every jwt-enabled deployment, and that breakage must be a
    /// signed, declared posture (not an accident discovered in prod).
    KeylessOnly { rationale: &'static str },
}

/// One declared consumer of a tenant-authenticated store method.
pub struct MethodConsumer {
    /// Method name (the `METHOD_CREDENTIALS` path suffix).
    pub method: &'static str,
    pub surface: ConsumerSurface,
    pub cred_source: CredSource,
    /// Workspace-relative file the consumer lives in. The
    /// `consumer-registry-anchors` misc-check greps `anchor_symbol`
    /// in this file — a renamed/removed consumer breaks the check, so
    /// the registry cannot silently rot.
    pub anchor_file: &'static str,
    pub anchor_symbol: &'static str,
}

/// The consumer registry: every production surface that calls a
/// tenant-authenticated LogService/StoreService method, with its
/// credential source declared. Tests pin the registry laws (KeylessOnly
/// requires a rationale; rows unique; the dashboard's Logs tab stays
/// KeylessOnly per owner decision Q1, 2026-06-04); the
/// `consumer-registry-anchors` misc-check pins the anchors against the
/// real files.
// r[impl store.log.consumer-registry]
pub const METHOD_CONSUMERS: &[MethodConsumer] = &[
    MethodConsumer {
        method: "TailLog",
        surface: ConsumerSurface::Gateway,
        cred_source: CredSource::CallerJwtRelay,
        // Forwards the watching caller's tenant token on the relayed
        // TailLog open.
        anchor_file: "rio-gateway/src/handler/log_tail.rs",
        anchor_symbol: "TENANT_TOKEN_HEADER",
    },
    MethodConsumer {
        method: "TailLog",
        surface: ConsumerSurface::Cli,
        cred_source: CredSource::TenantTokenFlag,
        anchor_file: "rio-cli/src/logs.rs",
        anchor_symbol: "RIO_TENANT_TOKEN",
    },
    MethodConsumer {
        method: "TailLog",
        surface: ConsumerSurface::Dashboard,
        cred_source: CredSource::KeylessOnly {
            rationale: "owner decision Q1 (2026-06-04), extending bug_290 \
                 (tenant JWT + ownership, no service bypass): no dashboard \
                 credential is funded this wave, so the Logs tab breaks in \
                 every jwt-enabled deployment — declared here, surfaced as \
                 the terminal authRequired stream state (no retry). A \
                 session-token exchange would restore only a per-tenant \
                 view; the cross-tenant operator surface needs \
                 operator-scope claims under its own owner decision.",
        },
        anchor_file: "rio-dashboard/src/lib/logStream.svelte.ts",
        anchor_symbol: "authRequired",
    },
    MethodConsumer {
        method: "TenantQuota",
        surface: ConsumerSurface::Gateway,
        cred_source: CredSource::CallerJwtRelay,
        // Forwards the submitting tenant's JWT on the quota probe.
        anchor_file: "rio-gateway/src/quota.rs",
        anchor_symbol: "with_jwt",
    },
];

/// Class lookup by full gRPC path (`/package.Service/Method`).
pub fn class_for(path: &str) -> Option<CredentialClass> {
    METHOD_CREDENTIALS
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, c)| *c)
}

/// Boot key-coherence check: `jwt ⇒ (service ∧ hmac)`, refusal naming
/// the missing knob(s). Call before serving; see the kernel's
/// [`key_coherence`] for the predicate and the spec rationale.
// r[impl store.authz.key-coherence]
pub fn validate_key_coherence(cfg: VerifierConfig) -> Result<(), String> {
    match key_coherence(cfg) {
        KeyCoherence::Coherent => Ok(()),
        KeyCoherence::MissingServiceKey => Err(
            "refusing to serve: JWT pubkey is configured but the service HMAC key \
             (RIO_STORE_SERVICE_HMAC_KEY_FILE / store.serviceHmacKey) is not — \
             Service-class methods would be silently unenforced while callers \
             believe the store authenticated (jwt => service && hmac)"
                .into(),
        ),
        KeyCoherence::MissingAssignmentKey => Err(
            "refusing to serve: JWT pubkey is configured but the assignment HMAC key \
             (RIO_STORE_LOG_HMAC_KEY_FILE / store.logHmacKey) is not — \
             AssignmentToken-class methods would be silently unenforced while \
             callers believe the store authenticated (jwt => service && hmac)"
                .into(),
        ),
        KeyCoherence::MissingBothKeys => Err(
            "refusing to serve: JWT pubkey is configured but BOTH HMAC keys are \
             missing (service: RIO_STORE_SERVICE_HMAC_KEY_FILE / store.serviceHmacKey; \
             assignment: RIO_STORE_LOG_HMAC_KEY_FILE / store.logHmacKey) — every \
             keyed class except TenantJwt would be silently unenforced \
             (jwt => service && hmac)"
                .into(),
        ),
    }
}

/// Tower layer enforcing [`METHOD_CREDENTIALS`].
#[derive(Clone)]
pub struct AuthzLayer {
    /// JWT pubkey configured → `TenantJwt` enforced.
    pub jwt_configured: bool,
    /// Assignment-HMAC verifier configured → `AssignmentToken` enforced.
    pub hmac_configured: bool,
    /// Verifier for the `Service` class. The layer verifies inline
    /// (sync, no I/O); `Some` is what "service knob configured" means
    /// — an unverifying-but-enforcing state is not constructible.
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

/// Wire mapping for kernel rejections.
fn reject_response(reason: RejectReason) -> Response<Body> {
    let (code, msg) = match reason {
        RejectReason::MissingTenantToken => (tonic::Code::Unauthenticated, "tenant token required"),
        RejectReason::MissingAssignmentToken => {
            (tonic::Code::Unauthenticated, "assignment token required")
        }
        RejectReason::MissingServiceToken => {
            (tonic::Code::Unauthenticated, "service token required")
        }
        RejectReason::ServiceVerificationFailed => (
            tonic::Code::Unauthenticated,
            "service token verification failed",
        ),
    };
    grpc_reject(code, msg)
}

impl<S> AuthzService<S> {
    /// The three knobs as a kernel view. The service knob IS verifier
    /// presence — an enforcing-but-unverifying state cannot be built.
    fn verifier_config(&self) -> VerifierConfig {
        VerifierConfig {
            jwt: self.jwt_configured,
            service: self.service_verifier.is_some(),
            hmac: self.hmac_configured,
        }
    }

    /// Classify the request's credential presentation RELATIVE TO the
    /// method's declared family (the credential-vector rule): only the
    /// declared family's material is inspected, so a foreign
    /// credential — a service token on a TenantJwt method, tenant
    /// claims on a Service method — is invisible and can neither
    /// widen nor poison the verdict.
    fn classify(&self, class: CredentialClass, req: &Request<Body>) -> Presented {
        match consumes(class) {
            Some(VerifierFamily::TenantJwt) => {
                if req.extensions().get::<TenantClaims>().is_some() {
                    Presented::TenantClaims
                } else {
                    Presented::None
                }
            }
            Some(VerifierFamily::Service) => match req.headers().get(SERVICE_TOKEN_HEADER) {
                // VERIFY, never trust presence — a spoofable header is
                // not a credential. Sync HMAC verify, no I/O.
                Some(raw) => {
                    let ok = raw
                        .to_str()
                        .ok()
                        .zip(self.service_verifier.as_ref())
                        .is_some_and(|(tok, sv)| {
                            sv.verify::<rio_auth::hmac::ServiceClaims>(tok).is_ok()
                        });
                    if ok {
                        Presented::ServiceVerified
                    } else {
                        Presented::ServiceGarbage
                    }
                }
                None => Presented::None,
            },
            Some(VerifierFamily::Assignment) => {
                if req.headers().contains_key(ASSIGNMENT_TOKEN_HEADER) {
                    Presented::AssignmentHeader
                } else {
                    Presented::None
                }
            }
            // Public / HandlerEnforced consume no transport verifier.
            None => Presented::None,
        }
    }

    /// `Some(reject)` if the request must not be dispatched.
    // r[impl store.authz.declared-verifier]
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
        let presented = self.classify(class, req);
        match decide(class, self.verifier_config(), presented) {
            LayerVerdict::Admit => None,
            LayerVerdict::Reject(reason) => Some(reject_response(reason)),
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

    /// Registry laws (merged_bug_108): keyless surfaces carry a dated
    /// owner-decision rationale; (method, surface) rows are unique;
    /// every registered method is a declared METHOD_CREDENTIALS row.
    // r[verify store.log.consumer-registry]
    #[test]
    fn consumer_registry_laws() {
        let mut seen = std::collections::HashSet::new();
        for c in METHOD_CONSUMERS {
            assert!(
                seen.insert((c.method, c.surface)),
                "duplicate consumer row: {} / {:?}",
                c.method,
                c.surface
            );
            if let CredSource::KeylessOnly { rationale } = c.cred_source {
                assert!(
                    rationale.contains("owner decision") && rationale.contains("20"),
                    "KeylessOnly row {}/{:?} must cite a dated owner decision",
                    c.method,
                    c.surface
                );
            }
            assert!(
                METHOD_CREDENTIALS
                    .iter()
                    .any(|(path, _)| path.ends_with(&format!("/{}", c.method))),
                "consumer row {} names a method with no METHOD_CREDENTIALS row",
                c.method
            );
            assert!(
                !c.anchor_file.is_empty() && !c.anchor_symbol.is_empty(),
                "consumer row {}/{:?} must carry a grep anchor",
                c.method,
                c.surface
            );
        }
    }

    /// Owner decision Q1 pinned: the dashboard TailLog surface stays
    /// KeylessOnly with the terminal authRequired anchor — flipping it
    /// to a credentialed source is a NEW owner decision and must edit
    /// this test knowingly.
    // r[verify store.log.consumer-registry]
    #[test]
    fn dashboard_taillog_stays_keyless_only() {
        let row = METHOD_CONSUMERS
            .iter()
            .find(|c| c.method == "TailLog" && c.surface == ConsumerSurface::Dashboard)
            .expect("the dashboard TailLog consumer must be registry-declared");
        assert!(
            matches!(row.cred_source, CredSource::KeylessOnly { .. }),
            "dashboard TailLog is KeylessOnly per owner decision Q1 (2026-06-04)"
        );
        assert_eq!(
            row.anchor_symbol, "authRequired",
            "the declared surface for the keyless break is the terminal authRequired state"
        );
    }

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

    // r[verify store.log.method-credential+2]
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

    /// The compensating control the L4-only builder/fetcher netpol
    /// posture leans on (bug_290; see networkpolicy.yaml's edge
    /// comment): a worker presenting its VALID credential — the
    /// assignment-token header, exactly what builder pods hold — must
    /// still be rejected by TailLog's tenant-JWT class. Assignment
    /// tokens never produce `TenantClaims` (the JWT interceptor
    /// attaches claims only from a verified tenant JWT), so the worker
    /// credential is STRUCTURALLY incapable of satisfying this gate —
    /// pinned here rather than assumed. Post-kernel this is the
    /// credential-vector rule: a foreign credential is invisible to
    /// the TenantJwt classifier.
    #[tokio::test]
    async fn taillog_assignment_token_rejected() {
        let mut s = svc(true, true);
        let mut r = req("/rio.store.LogService/TailLog");
        // The worker's real credential shape: a log/assignment token
        // header, NO verified tenant claims in extensions.
        r.headers_mut().insert(
            "x-rio-log-token",
            http::HeaderValue::from_static("hmac.exec.builder.sig"),
        );
        let resp = call(&mut s, r).await;
        assert_eq!(
            grpc_status(&resp),
            Some(tonic::Code::Unauthenticated as i32),
            "an assignment token must not satisfy the tenant-JWT class"
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

    /// THE dead-leg red (bug_237, recorded pre-fix): verified tenant
    /// claims on an admin method were ADMITTED by the old
    /// `ServiceOrTenant` tenant leg (observed: grpc-status None vs
    /// expected Unauthenticated). Every admin handler demands a
    /// service caller, so the leg admitted nothing in the green path
    /// — but in the half-config state (JWT on, service key off) the
    /// handler's dev-mode passthrough made ANY tenant a cluster
    /// admin. Post-kernel: tenant claims are a foreign credential on
    /// a `Service` method and never admit.
    // r[verify store.authz.declared-verifier]
    #[tokio::test]
    async fn dead_tenant_leg_rejected_on_admin_methods() {
        let mut s = svc(true, true);
        let mut r = req("/rio.store.StoreAdminService/TriggerGC");
        r.extensions_mut().insert(test_claims());
        let resp = call(&mut s, r).await;
        assert_eq!(
            grpc_status(&resp),
            Some(tonic::Code::Unauthenticated as i32),
            "tenant claims must not admit an admin (Service-class) method"
        );
    }

    /// 122's layer red (recorded pre-fix): tokenless TenantQuota was
    /// ADMITTED (the row was `Open`; observed grpc-status None).
    /// Post-reclassification the row is `TenantJwt`: no verified
    /// claims, no quota read. (Handler-side ownership — claims.sub vs
    /// the named tenant — lands with the witness series.)
    // r[verify store.log.method-credential+2]
    #[tokio::test]
    async fn tenantquota_tokenless_rejected_when_jwt_configured() {
        let mut s = svc(true, true);
        let resp = call(&mut s, req("/rio.store.StoreService/TenantQuota")).await;
        assert_eq!(
            grpc_status(&resp),
            Some(tonic::Code::Unauthenticated as i32),
            "tokenless TenantQuota must be rejected when JWT is configured"
        );
    }

    /// Presence is not a credential: a garbage service token must be
    /// REJECTED at the layer (pre-kernel red: the arm passed on
    /// contains_key alone — a spoofed header bypassed the layer).
    // r[verify store.log.method-credential+2]
    #[tokio::test]
    async fn service_garbage_token_rejected() {
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
    async fn service_valid_token_admitted() {
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

    /// The Service class is keyed on the SERVICE verifier alone: with
    /// no service key the class is unenforced at the layer (dual-mode
    /// doctrine) — and the dangerous variant of this state (JWT on,
    /// service key off) never serves at all: `validate_key_coherence`
    /// refuses it at boot (see `boot_coherence_*` below). The
    /// pre-kernel layer keyed this class on the JWT knob instead —
    /// the foreign-knob read the kernel makes uncompilable.
    #[tokio::test]
    async fn service_class_unenforced_without_service_key() {
        let mut s = svc_with_verifier(false, true, None);
        let mut r = req("/rio.store.StoreAdminService/GetLoad");
        r.headers_mut().insert(
            SERVICE_TOKEN_HEADER,
            HeaderValue::from_static("anything-at-all"),
        );
        let resp = call(&mut s, r).await;
        assert_eq!(
            grpc_status(&resp),
            None,
            "no service key (and no JWT — the coherent variant) = dev mode for the \
             Service class; the JWT-on variant is refused at boot, not here"
        );
    }

    /// Boot coherence (bug_237's C2 red, recorded pre-fix: the store
    /// BOOTED in state (1,0,1) — no coherence check existed):
    /// jwt ⇒ (service ∧ hmac); each refusal names the missing knob.
    // r[verify store.authz.key-coherence]
    #[test]
    fn boot_coherence_refuses_jwt_without_service_key() {
        let err = validate_key_coherence(VerifierConfig {
            jwt: true,
            service: false,
            hmac: true,
        })
        .unwrap_err();
        assert!(err.contains("service HMAC key"), "{err}");
        assert!(err.contains("serviceHmacKey"), "{err}");
    }

    // r[verify store.authz.key-coherence]
    #[test]
    fn boot_coherence_refuses_jwt_without_assignment_key() {
        let err = validate_key_coherence(VerifierConfig {
            jwt: true,
            service: true,
            hmac: false,
        })
        .unwrap_err();
        assert!(err.contains("assignment HMAC key"), "{err}");
        assert!(err.contains("logHmacKey"), "{err}");
    }

    // r[verify store.authz.key-coherence]
    #[test]
    fn boot_coherence_refuses_jwt_without_both_keys() {
        let err = validate_key_coherence(VerifierConfig {
            jwt: true,
            service: false,
            hmac: false,
        })
        .unwrap_err();
        assert!(err.contains("BOTH HMAC keys"), "{err}");
    }

    /// The five coherent states all boot: dev (0,0,0), helm default
    /// (0,1,1), full (1,1,1), and the two keys-without-jwt states.
    // r[verify store.authz.key-coherence]
    #[test]
    fn boot_coherence_admits_the_five_coherent_states() {
        for (jwt, service, hmac) in [
            (false, false, false),
            (false, true, true),
            (true, true, true),
            (false, true, false),
            (false, false, true),
        ] {
            assert!(
                validate_key_coherence(VerifierConfig { jwt, service, hmac }).is_ok(),
                "({jwt},{service},{hmac}) must boot"
            );
        }
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
