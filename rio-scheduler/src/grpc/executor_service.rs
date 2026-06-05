//! `ExecutorService` gRPC implementation for [`SchedulerGrpc`].
//!
//! Worker-facing RPCs: the pull-mode `PullAssignment`/`ReportOutcome`
//! unaries — the only work-delivery surface (the legacy `BuildExecution`
//! stream and `Heartbeat` unary were removed with the proto sweep; a
//! stray stream-mode executor now gets tonic's unimplemented-method
//! answer at the routing layer).

use tonic::{Request, Response, Status};
use tracing::{instrument, warn};

use rio_proto::ExecutorService;

use crate::actor::ActorCommand;

use super::SchedulerGrpc;

// ── Worker-supplied field bounds ─────────────────────────────────────
// r[impl sched.executor.input-bounds+2]
// EVERY string/bytes field a worker can set on the ExecutorService
// surface is listed here with its bound or an explicit reason it is left
// unbounded. Numeric fields are listed with their validation /
// total-arithmetic treatment when the scheduler folds them into persisted
// row metadata or per-execution ordering state; all other numerics stay
// `n/a`. When a field is added to the request messages or their nested
// messages, add a row — an unlisted field is a review rejection. Two
// enforcement styles:
//   reject   — fail the RPC.
//   truncate — bound the field in place, keep the message. For
//              CompletionReport payload fields (the report itself must reach
//              the actor — a lost completion strands the derivation in
//              Running).
// PullAssignment RPC (pull-mode dispatch):
//   intent_id                         → DAG lookup, assignments/executions rows; the build-pull identity
//                                       and the left half of the composite materialization ExecutorId →
//                                       reject RPC > MAX_IDENT_LEN; reject RPC if it contains '@' (the
//                                       composite separator — keeps build identities and materialization
//                                       composites disjoint)
//   executor_token                    → HMAC-verified then dropped        → reject RPC > MAX_EXECUTOR_TOKEN_LEN
//                                       (verification fails on any tamper; the bound only caps the hash work)
//   kind                              → enum/i32 (UNSPECIFIED|BUILD → Build; MATERIALIZATION →
//                                       Materialization); decides identity binding + attempt_kind column.
//                                       AUTHZ: MATERIALIZATION + any verified executor token → reject RPC
//                                       PermissionDenied (no executor credential authorizes the kind);
//                                       the kind-attested store-service credential
//                                       (ServiceClaims caller="rio-store", x-rio-service-token) is what
//                                       authorizes it — verified then dropped → reject RPC > MAX_EXECUTOR_TOKEN_LEN
//   executor_instance                 → BC-1 per-replica identity, interpolated into the composite
//                                       ExecutorId and assignments/executions rows → reject RPC unless a
//                                       DNS-1123 label (lowercase alnum + interior hyphens, ≤63; excludes
//                                       '@' so the composite stays unambiguous); reject RPC empty for
//                                       kind=MATERIALIZATION (mandatory); ignored/dropped for kind=BUILD.
//                                       AUTHZ (T-5.1): under the enforced posture the field must EQUAL the
//                                       instance bound inside the signed store-service claims → reject RPC
//                                       PermissionDenied on mismatch (the claim is the authority; this
//                                       request field is defense in depth)
// ReportOutcome RPC (pull-mode dispatch):
//   exec_id                           → UUID parse → attempt lookup       → reject RPC if not a valid UUID
//   report.result.error_msg           → event ring, terminal payload      → truncate to MAX_ERROR_MSG_LEN
//   report.node_name / report.hw_class → build_samples row                → None if > MAX_IDENT_LEN
//   report.drv_path                   → never read (exec_id names the attempt) → dropped before the actor
//   report numerics (peak_*, final_resources.*) → build_samples row       → validated actor-side (completion.rs
//                                       record_build_sample: finite/in-domain or NULL; .min(i64::MAX) clamps)
//   materialization_outcome.*         → routed to the materialization consumption transaction for
//                                       materialization-kind attempts; acknowledged-and-ignored for
//                                       build attempts (kindMatchesWorker). Mutually exclusive with
//                                       report (reject RPC when both set). AUTHZ: requires the
//                                       store-service credential; an executor-authenticated report
//                                       carrying it → reject RPC PermissionDenied. Path/cause strings land
//                                       in ledger error_msg / routing inputs → reject RPC if report also set;
//                                       per-field truncation rides MAX_ERROR_MSG_LEN at the ledger append
// ListMaterializationJobs RPC (leader-served store poll):
//   service_token                     → HMAC-verified then dropped (the body fallback carrier for the
//                                       store-service credential; an executor token here is rejected) →
//                                       reject RPC > MAX_EXECUTOR_TOKEN_LEN
//   limit                             → numeric; clamped to 256 actor-side → n/a
// ReportMaterializationProgress RPC (Phase B: the BC-4 relay into build events):
//   AUTHZ: the same store-service credential gate as the other materialization
//   operations (an executor token on the metadata carrier → reject RPC
//   PermissionDenied; no credential under a configured key family → reject RPC
//   Unauthenticated; only full dev mode is open). The proto carries no body
//   token field, so x-rio-service-token metadata is the only carrier. The gate
//   runs BEFORE any request-body parse (pinned by the T-1.9 sweep test).
//   exec_id                           → UUID parse → attempt lookup (relay key) → reject RPC if not a
//                                       valid UUID; unknown/superseded exec → ack + drop (display-only)
//   upstream_uri                      → display-only Event::SubstituteProgress field → truncate to
//                                       MAX_IDENT_LEN before the event ring
//   bytes_done / bytes_expected       → display-only event numerics (never persisted, never folded
//                                       into scheduler state) → n/a

/// Upper bound on worker-supplied identifier/label fields: `intent_id`,
/// `node_name`, and `hw_class`. All are either k8s object names (≤253
/// bytes by the DNS-subdomain rule), UUIDs, or Nix system strings (tens
/// of bytes). They are interpolated into log lines or land in
/// `build_samples` rows.
pub(super) const MAX_IDENT_LEN: usize = 256;

/// Upper bound on a worker-supplied `BuildResult.error_msg`. Truncated
/// (not rejected — a dropped `CompletionReport` strands the derivation in
/// `Running`). A legitimate daemon/executor error is well under 16 KiB;
/// the field is fanned out as `DerivationEvent::failed.error_message` to
/// `(1 + cascaded_ancestors) × interested_builds` state-ring slots and
/// `nix build -L` terminals.
///
/// Head-truncation cannot break the scheduler's semantic dispatch on this
/// field: `handle_infrastructure_failure` greps for `CGROUP_OOM_MSG` and
/// `CONCURRENT_PUTPATH_MSG`, both short builder-constructed prefixes that
/// sit in the first few hundred bytes of a legitimate message. A hostile
/// builder padding the marker past 16 KiB only denies itself the
/// resource-floor bump.
pub(super) const MAX_ERROR_MSG_LEN: usize = 16 * 1024;

/// Upper bound on the body-supplied `PullAssignmentRequest.executor_token`.
/// A legitimate HMAC executor token is a few hundred bytes; the bound only
/// caps how much input the verifier hashes before rejecting garbage. The
/// metadata carrier is already bounded by the HTTP/2 header limits.
pub(super) const MAX_EXECUTOR_TOKEN_LEN: usize = 8 * 1024;

/// RFC-1123 DNS label check for `executor_instance` (a k8s pod name
/// component). merged_bug_243: re-exported from `rio_common::dns` —
/// the SAME single-sourced alphabet the store's `Dns1123Label`
/// sanitizer/composer enforces, so the producer and this validator
/// cannot drift (the pre-fix private copy validated a bound the
/// store-side composition silently broke). The instance is
/// interpolated into the composite materialization ExecutorId
/// (`{intent}@{instance}`), so the alphabet exclusion of `@` (and
/// everything else) is what keeps that composite unambiguous.
// r[impl store.materialize.worker-identity]
pub(super) use rio_common::dns::is_dns1123_label;

/// `ServiceClaims.caller` values whose service token authorizes
/// materialization operations (the Wave-4 kind-attested store
/// credential). Exactly the store service: the credential attests the
/// caller IS a store replica — the work class materialization belongs
/// to — not merely "some control-plane service".
const MATERIALIZATION_SERVICE_CALLERS: &[&str] = &["rio-store"];

/// Outcome of the store-service credential gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum StoreServiceAuth {
    /// A valid `ServiceClaims{caller="rio-store"}` token was presented.
    /// Carries the verified instance claim from the signed body —
    /// `None` for a pre-T-5.1 or non-store-minted (gateway-style)
    /// instance-less token.
    Authorized { instance: Option<String> },
    /// Full dev mode (no executor HMAC key AND no service verifier):
    /// open, the flag gate decides — the same posture every other
    /// ExecutorService unary has in dev mode.
    DevMode,
}

impl SchedulerGrpc {
    /// The store-service credential gate for materialization operations
    /// (the Wave-4 kind-attested credential — security-review findings
    /// 2/3 from the Wave-3 hardening).
    ///
    /// Materialization pulls / job listing / outcome reports are
    /// fleet-level STORE operations, not per-intent executor
    /// operations: the authorizing credential is
    /// `ServiceClaims { caller: "rio-store" }` signed with the SEPARATE
    /// service-HMAC key (the same `x-rio-service-token` family the
    /// admin surfaces verify) — never an executor token.
    ///
    /// Carriers: the `x-rio-service-token` metadata header (what
    /// `rio_auth::hmac::ServiceTokenInterceptor` attaches), with
    /// `body_token` as the body fallback for requests whose proto
    /// carries one (`ListMaterializationJobsRequest.service_token`).
    ///
    /// Closed-by-default: the only open outcome is full dev mode
    /// (neither key family configured). A configured deployment with a
    /// missing/invalid/wrong-caller/wrong-key token is rejected.
    // r[impl sched.materialize.job+2]
    pub(super) fn require_store_service<T>(
        &self,
        req: &tonic::Request<T>,
        body_token: &str,
    ) -> Result<StoreServiceAuth, Status> {
        // Full dev mode → open (the flag gate answers; flag-off that is
        // the empty list / NotYetReady / ack-and-ignore).
        if self.hmac_key.is_none() && self.service_verifier.is_none() {
            return Ok(StoreServiceAuth::DevMode);
        }
        // Locate a presented token: metadata carrier first, body
        // fallback second.
        let token = req
            .metadata()
            .get(rio_common::grpc::SERVICE_TOKEN_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .or_else(|| (!body_token.is_empty()).then(|| body_token.to_owned()));
        let Some(token) = token else {
            // No service credential presented in an authenticated
            // deployment → the same closed answer the executor-token
            // unaries give credential-less calls.
            return Err(Status::unauthenticated(
                "materialization operations require x-rio-service-token \
                 (the store-service credential)",
            ));
        };
        // r[impl sched.executor.input-bounds+2]
        rio_common::grpc::check_bound("service token bytes", token.len(), MAX_EXECUTOR_TOKEN_LEN)?;
        let Some(verifier) = &self.service_verifier else {
            // Half-configured deployment (executor HMAC without a
            // service key): the presented credential cannot be
            // verified → closed.
            return Err(Status::permission_denied(
                "a store-service credential was presented but the scheduler has \
                 no service-HMAC verifier configured",
            ));
        };
        let claims: rio_auth::hmac::ServiceClaims = verifier.verify(&token).map_err(|e| {
            Status::permission_denied(format!("store-service token verification failed: {e}"))
        })?;
        if !MATERIALIZATION_SERVICE_CALLERS.contains(&claims.caller.as_str()) {
            return Err(Status::permission_denied(format!(
                "service-token caller {:?} does not authorize materialization \
                 operations (allowed: {MATERIALIZATION_SERVICE_CALLERS:?})",
                claims.caller
            )));
        }
        Ok(StoreServiceAuth::Authorized {
            instance: claims.instance,
        })
    }

    /// The instance token-claim binding (substitution-replacement Phase B
    /// security obligation 1 / T-5.1) layered on
    /// [`Self::require_store_service`]: the materialization WORK surfaces
    /// (claim, listing, outcome report — everything with a state effect)
    /// require the store-service credential to be **instance-bound**
    /// (`ServiceClaims.instance = Some(_)`, minted only by the store's
    /// flag-gated materialization client), and — when the request itself
    /// asserts an `executor_instance` (the claim path) — the claimed
    /// instance MUST equal the request's. The claim is the authority; the
    /// request field is now defense in depth (its DNS-1123 validation
    /// stays).
    ///
    /// Privilege narrowing: gateway-PutPath-style instance-less
    /// ServiceClaims tokens (and every pre-T-5.1 in-flight token) no
    /// longer authorize materialization work — they were never minted for
    /// it. The display-only progress relay is NOT routed through this
    /// gate (no state effect to bind; the fleet-level credential
    /// suffices there).
    ///
    /// Dev mode passes through unchanged (no claims exist to bind).
    // r[impl sched.materialize.job+2]
    pub(super) fn require_store_service_instance_bound<T>(
        &self,
        req: &tonic::Request<T>,
        body_token: &str,
        request_instance: Option<&str>,
    ) -> Result<StoreServiceAuth, Status> {
        let auth = self.require_store_service(req, body_token)?;
        match &auth {
            StoreServiceAuth::DevMode => Ok(auth),
            StoreServiceAuth::Authorized { instance } => {
                let Some(claimed) = instance else {
                    return Err(Status::permission_denied(
                        "the store-service token does not carry the instance claim \
                         (materialization operations require an instance-bound \
                         credential since Phase B)",
                    ));
                };
                if let Some(requested) = request_instance
                    && claimed != requested
                {
                    return Err(Status::permission_denied(format!(
                        "instance claim mismatch: the store-service token is bound to \
                         {claimed:?} but the request asserts executor_instance {requested:?}"
                    )));
                }
                Ok(auth)
            }
        }
    }

    /// Whether the request presents a token that VERIFIES as an
    /// executor credential, on either carrier (`x-rio-executor-token`
    /// metadata or the given body field). Used by the materialization
    /// authorization arms: a verified executor credential never
    /// authorizes the materialization kind regardless of carrier
    /// (the Wave-3 rejection, kept verbatim).
    ///
    /// `None` when no executor HMAC key is configured, when no token is
    /// present, or when what is present does not verify — the caller
    /// then consults the store-service gate, which gives the precise
    /// closed/open answer.
    pub(super) fn verified_executor_claims<T>(
        &self,
        req: &tonic::Request<T>,
        body_token: &str,
    ) -> Option<rio_auth::hmac::ExecutorClaims> {
        let key = self.hmac_key.as_ref()?;
        if let Some(tok) = req
            .metadata()
            .get(rio_proto::EXECUTOR_TOKEN_HEADER)
            .and_then(|v| v.to_str().ok())
            && let Ok(claims) = key.verify::<rio_auth::hmac::ExecutorClaims>(tok)
        {
            return Some(claims);
        }
        if !body_token.is_empty()
            && body_token.len() <= MAX_EXECUTOR_TOKEN_LEN
            && let Ok(claims) = key.verify::<rio_auth::hmac::ExecutorClaims>(body_token)
        {
            return Some(claims);
        }
        None
    }

    /// The shared handler prologue (bug_362): trace linkage, the
    /// standby gate, and the actor-liveness gate — LINE ONE of every
    /// `ExecutorService` handler. The descriptor-driven sweep test
    /// (`every_executor_service_method_is_standby_gated`) enumerates
    /// the service's methods from `FILE_DESCRIPTOR_SET`, so the next
    /// RPC cannot omit this call unnoticed.
    pub(super) fn executor_prologue<T>(&self, request: &Request<T>) -> Result<(), Status> {
        rio_proto::interceptor::link_parent(request);
        self.ensure_leader()?;
        self.check_actor_alive()
    }

    // r[impl sec.executor.identity-token+3]
    /// THE credential chokepoint (merged_bug_084): the credential
    /// FAMILY is selected by the payload kind BEFORE any verification —
    /// a gate nested under another family's error arm is unwritable.
    ///
    ///   Build                  → the executor-identity family
    ///                            (`require_executor`, with the body
    ///                            fallback where the proto carries one;
    ///                            no key configured = dev mode, the
    ///                            as-built posture).
    ///   Materialization        → the store-service family: a token
    ///   (state-effecting)        that VERIFIES as an executor
    ///                            credential is rejected first (it
    ///                            never authorizes the kind, on any
    ///                            carrier), then the instance-bound
    ///                            store credential is required.
    ///   Materialization        → the same family without the instance
    ///   (display-only)           binding (no state effect to bind).
    ///
    /// The half-configured deployment {hmac: None, service: Some} is
    /// the red cell this chokepoint closes: family selection no longer
    /// consults the executor key's presence, so a credential-less
    /// materialization payload is rejected even when `require_executor`
    /// would have answered dev-mode `Ok(None)`.
    pub(super) fn credential_for<T>(
        &self,
        kind: PayloadCredentialKind<'_>,
        req: &Request<T>,
    ) -> Result<ResolvedCredential, Status> {
        match kind {
            PayloadCredentialKind::Build { body_token } => {
                match self.require_executor(req) {
                    Ok(claims) => Ok(ResolvedCredential::Executor(claims)),
                    Err(metadata_err) => {
                        if body_token.is_empty() {
                            return Err(metadata_err);
                        }
                        // r[impl sched.executor.input-bounds+2]
                        rio_common::grpc::check_bound(
                            "executor_token bytes",
                            body_token.len(),
                            MAX_EXECUTOR_TOKEN_LEN,
                        )?;
                        let Some(key) = &self.hmac_key else {
                            // require_executor only fails when a key is
                            // configured; unreachable, but stay closed.
                            return Err(metadata_err);
                        };
                        let claims: rio_auth::hmac::ExecutorClaims =
                            key.verify(body_token).map_err(|e| {
                                Status::unauthenticated(format!(
                                    "executor_token verification failed: {e}"
                                ))
                            })?;
                        Ok(ResolvedCredential::Executor(Some(claims)))
                    }
                }
            }
            PayloadCredentialKind::MaterializationStateEffecting {
                executor_body_token,
                service_body_token,
                request_instance,
            } => {
                // The executor-rejection arm checks BOTH carriers: the
                // metadata header and whatever body field the unary
                // carries (the pull's executor_token, the listing's
                // service_token) — a builder credential never
                // authorizes the kind regardless of where it rides.
                if self
                    .verified_executor_claims(req, executor_body_token)
                    .is_some()
                {
                    return Err(Status::permission_denied(
                        "executor tokens do not authorize materialization operations \
                         (a store-service credential is required)",
                    ));
                }
                self.require_store_service_instance_bound(req, service_body_token, request_instance)
                    .map(ResolvedCredential::StoreService)
            }
            PayloadCredentialKind::MaterializationDisplayOnly => {
                if self.verified_executor_claims(req, "").is_some() {
                    return Err(Status::permission_denied(
                        "executor tokens do not authorize materialization progress reports \
                         (a store-service credential is required)",
                    ));
                }
                self.require_store_service(req, "")
                    .map(ResolvedCredential::StoreService)
            }
        }
    }
}

/// The payload class [`SchedulerGrpc::credential_for`] selects the
/// credential family from — decided by FIELD PRESENCE / the request's
/// claimed kind, never by which keys happen to be configured.
#[derive(Debug)]
pub(super) enum PayloadCredentialKind<'a> {
    /// A build payload: the executor-identity family.
    Build { body_token: &'a str },
    /// A state-effecting materialization payload (claim, listing,
    /// outcome report): the instance-bound store-service family.
    MaterializationStateEffecting {
        /// The unary's body field a stray EXECUTOR token could ride in
        /// (the pull's `executor_token`, the listing's `service_token`)
        /// — checked by the rejection arm alongside the metadata header.
        executor_body_token: &'a str,
        /// The unary's body fallback for the STORE credential
        /// (`service_token` where the proto carries one; "" otherwise).
        service_body_token: &'a str,
        request_instance: Option<&'a str>,
    },
    /// The display-only progress relay: the store-service family
    /// without the instance binding.
    MaterializationDisplayOnly,
}

/// What [`SchedulerGrpc::credential_for`] resolves.
#[derive(Debug)]
pub(super) enum ResolvedCredential {
    /// Build family: the HMAC-attested executor claims
    /// (`None` = dev mode, no key configured).
    Executor(Option<rio_auth::hmac::ExecutorClaims>),
    /// Materialization family: the store-service gate's outcome.
    /// The carried auth (DevMode vs the verified instance claim) is
    /// not consumed by today's handlers — it is the landing site for
    /// any future per-replica binding (the merged_bug_115 record names
    /// `credential_for` as that single site), so the chokepoint
    /// surfaces it now rather than re-plumbing later.
    StoreService(#[allow(dead_code)] StoreServiceAuth),
}

#[tonic::async_trait]
impl ExecutorService for SchedulerGrpc {
    // Pull-mode dispatch surface — the only work-delivery path.

    /// The pull-mode pod's single ask. Leader-served; the actor turn
    /// runs the admission kernel and (on Deliver) the one fenced
    /// transaction. A re-pull while the attempt is open returns the
    /// identical payload and exec_id.
    // r[impl sched.executor.pull-transaction+2]
    #[instrument(skip(self, request), fields(rpc = "PullAssignment"))]
    async fn pull_assignment(
        &self,
        request: Request<rio_proto::types::PullAssignmentRequest>,
    ) -> Result<Response<rio_proto::types::PullAssignmentResponse>, Status> {
        self.executor_prologue(&request)?;
        // Substitution-replacement: the claimed work class, read before
        // the identity gate because it selects WHICH credential family
        // applies. Proto3 zero value (UNSPECIFIED) and BUILD both map to
        // Build — deployed builder pods that predate the field send
        // nothing and behave bit-for-bit as before (the frozen
        // pull-contract addendum).
        let kind = match request.get_ref().kind() {
            rio_proto::types::AttemptKind::Materialization => {
                rio_evidence_kernel::pull::PullKind::Materialization
            }
            rio_proto::types::AttemptKind::Unspecified | rio_proto::types::AttemptKind::Build => {
                rio_evidence_kernel::pull::PullKind::Build
            }
        };
        // r[impl sec.executor.identity-token+3]
        // Identity gates, per work class:
        //
        // BUILD pulls (the as-built path, byte-identical): the executor
        // token↔intent binding the stream-era session enforced,
        // applied per-unary. The token may arrive in metadata
        // (x-rio-executor-token) or in the request body (the unary is
        // self-contained for clients that cannot set per-call metadata);
        // either carrier is verified by the same key.
        //
        // MATERIALIZATION pulls (the Wave-3/Wave-4 authorization): an
        // executor token is the per-intent BUILDER/FETCHER credential —
        // its `kind` claim attests the pod class for the FOD airgap, not
        // the materialization work class — so it never authorizes the
        // kind, on either carrier. The kind-attested store-service
        // credential (ServiceClaims caller="rio-store", the separate
        // x-rio-service-token key family) is what does. Full dev mode
        // (neither key family configured) falls through to the flag
        // gate, which parks while materialization dispatch is disabled.
        // The credential family, selected by the claimed kind through
        // the chokepoint (merged_bug_084). T-5.1 lives inside it: the
        // materialization arm requires the instance-bound store
        // credential and matches it against the request's
        // executor_instance — the claim is the authority, the request
        // field is defense in depth. An empty request instance skips
        // the match and is rejected InvalidArgument by the BC-1
        // validation below (the mandatory-identity rule precedes the
        // binding rule).
        // r[impl sched.materialize.job+2]
        let credential_kind = if kind == rio_evidence_kernel::pull::PullKind::Materialization {
            PayloadCredentialKind::MaterializationStateEffecting {
                executor_body_token: request.get_ref().executor_token.as_str(),
                service_body_token: "",
                request_instance: {
                    let asserted = request.get_ref().executor_instance.as_str();
                    (!asserted.is_empty()).then_some(asserted)
                },
            }
        } else {
            PayloadCredentialKind::Build {
                body_token: request.get_ref().executor_token.as_str(),
            }
        };
        let auth_claims = match self.credential_for(credential_kind, &request) {
            Ok(ResolvedCredential::Executor(claims)) => claims,
            // The store credential is fleet-level, not per-intent: the
            // pulling identity is the composite (intent, replica) pair
            // the kernel arbitrates on, so there is no auth_intent.
            Ok(ResolvedCredential::StoreService(_)) => None,
            Err(e) => {
                metrics::counter!(
                    "rio_scheduler_pull_rejected_total",
                    "rpc" => "pull_assignment",
                    "reason" => if e.code() == tonic::Code::PermissionDenied {
                        "kind_unauthorized"
                    } else {
                        "unauthenticated"
                    }
                )
                .increment(1);
                return Err(e);
            }
        };
        let auth_intent = auth_claims.as_ref().map(|c| c.intent_id.clone());
        let req = request.into_inner();
        if req.intent_id.is_empty() {
            return Err(Status::invalid_argument("intent_id is required"));
        }
        // r[impl sched.executor.input-bounds+2]
        rio_common::grpc::check_bound("intent_id bytes", req.intent_id.len(), MAX_IDENT_LEN)?;
        // Identity hygiene (security review, separator confusion): `@`
        // is the composite-identity separator (`{intent}@{instance}`).
        // An intent carrying it would let a build identity collide with
        // a materialization composite (intent `a@b` vs intent `a` on
        // replica `b`), confusing the kernel's same-identity
        // re-delivery arm. Intent ids are scheduler-generated drv
        // hashes and never contain `@` — reject closed (defense in
        // depth), every kind, every carrier.
        // r[impl sched.executor.input-bounds+2]
        if req.intent_id.contains('@') {
            return Err(Status::invalid_argument(
                "intent_id must not contain '@' (the composite-identity separator)",
            ));
        }
        // BC-1: the per-replica identity is mandatory for the
        // materialization kind (it is what makes the kernel's
        // one-winner arbitration per-replica); for build pulls the
        // field is ignored — build identity stays the attested intent.
        let executor_instance = match kind {
            rio_evidence_kernel::pull::PullKind::Materialization => {
                if req.executor_instance.is_empty() {
                    return Err(Status::invalid_argument(
                        "executor_instance is required for materialization pulls",
                    ));
                }
                // Identity hygiene (security review): the instance is
                // interpolated into the composite ExecutorId
                // (`{intent}@{instance}`), so it must be a clean
                // DNS-1123 label — anything else (uppercase, a second
                // `@`, underscores, overlength) would make the
                // composite ambiguous or spoofable.
                // r[impl sched.executor.input-bounds+2]
                if !is_dns1123_label(&req.executor_instance) {
                    return Err(Status::invalid_argument(
                        "executor_instance must be a DNS-1123 label \
                         (lowercase alphanumerics and interior hyphens, at most 63 chars)",
                    ));
                }
                Some(req.executor_instance)
            }
            rio_evidence_kernel::pull::PullKind::Build => None,
        };
        // merged_bug_158: the re-delivery resume token. Parse-don't-
        // validate at the boundary: a malformed token is treated as
        // ABSENT (deny-by-default — the kernel answers NotYetReady and
        // the establishment window settles the attempt), never as an
        // error the caller can distinguish (no probe oracle). Build
        // pulls never carry one (the field is ignored if set — build
        // re-delivery stays tokenless per the as-built contract).
        let resume_exec_id = match kind {
            rio_evidence_kernel::pull::PullKind::Materialization => {
                (!req.resume_exec_id.is_empty())
                    .then(|| req.resume_exec_id.parse::<uuid::Uuid>().ok())
                    .flatten()
            }
            rio_evidence_kernel::pull::PullKind::Build => None,
        };
        // bug_251 (rule-4b): the claim nonce — same parse-don't-
        // validate posture as the resume token (malformed == absent,
        // deny-by-default, no probe oracle). Build pulls never carry
        // one.
        let claim_nonce = match kind {
            rio_evidence_kernel::pull::PullKind::Materialization => (!req.claim_nonce.is_empty())
                .then(|| req.claim_nonce.parse::<uuid::Uuid>().ok())
                .flatten(),
            rio_evidence_kernel::pull::PullKind::Build => None,
        };

        // send_unchecked: a dropped pull would park the pod for a full
        // backoff interval; the pod retries anyway, so backpressure
        // surfaces as retried pulls, never lost work.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.actor
            .send_unchecked(ActorCommand::PullAssignment {
                intent_id: req.intent_id,
                auth_intent,
                kind,
                executor_instance,
                resume_exec_id,
                claim_nonce,
                reply: reply_tx,
            })
            .await
            .map_err(Self::actor_error_to_status)?;
        let outcome = reply_rx
            .await
            .map_err(|_| Status::internal("actor dropped PullAssignment reply"))?;

        use rio_proto::types::pull_assignment_response::Outcome;
        let outcome = match outcome {
            Ok(crate::actor::PullOutcome::Deliver(assignment)) => Outcome::Assignment(*assignment),
            Ok(crate::actor::PullOutcome::Gone) => Outcome::Gone(rio_proto::types::Gone {}),
            Ok(crate::actor::PullOutcome::NotYetReady { retry_after_secs }) => {
                Outcome::NotYetReady(rio_proto::types::NotYetReady {
                    retry_after_seconds: retry_after_secs,
                })
            }
            Err(
                r @ (crate::actor::PullRejection::NotLeader
                | crate::actor::PullRejection::StaleGeneration
                | crate::actor::PullRejection::ConsumptionNotDurable),
            ) => {
                // Retryable class (the shared mapping).
                // ConsumptionNotDurable is unreachable from the pull
                // path (no consumption close runs in a pull) — wired
                // for exhaustiveness with its retryable siblings.
                return Err(super::actor_guards::pull_rejection_to_status(&r));
            }
            Err(r @ crate::actor::PullRejection::TokenMismatch) => {
                metrics::counter!(
                    "rio_scheduler_pull_rejected_total",
                    "rpc" => "pull_assignment",
                    "reason" => "token_mismatch"
                )
                .increment(1);
                return Err(super::actor_guards::pull_rejection_to_status(&r));
            }
            Err(r @ crate::actor::PullRejection::Internal(_)) => {
                return Err(super::actor_guards::pull_rejection_to_status(&r));
            }
        };
        Ok(Response::new(rio_proto::types::PullAssignmentResponse {
            outcome: Some(outcome),
        }))
    }

    /// Terminal-outcome intake for a pulled attempt, idempotent by
    /// exec_id. The empty ack is returned only after the scheduler's
    /// classification (and its appending transaction) has run — the
    /// pod's exit-0 depends on it.
    // r[impl sched.executor.report-idempotent]
    #[instrument(skip(self, request), fields(rpc = "ReportOutcome"))]
    async fn report_outcome(
        &self,
        request: Request<rio_proto::types::ReportOutcomeRequest>,
    ) -> Result<Response<rio_proto::types::ReportOutcomeResponse>, Status> {
        self.executor_prologue(&request)?;
        // r[impl sec.executor.identity-token+3]
        // Identity, per payload class — the family is selected from the
        // PAYLOAD (field presence), never from which keys are
        // configured (merged_bug_084's half-config hole: the store gate
        // used to nest under require_executor's Err arm, so
        // {hmac: None, service: Some} consumed credential-less
        // materialization reports):
        //
        // BUILD reports (the as-built path, byte-identical): the
        // metadata header is the report's only identity carrier (the
        // frozen signature has no body token field), so a missing or
        // invalid token under the enforced HMAC posture is counted for
        // alertability — the rejected pod's own logs are ephemeral.
        //
        // MATERIALIZATION reports (`materialization_outcome` set): the
        // reporter is a store replica, whose identity is the
        // kind-attested instance-bound store-service credential — not a
        // per-intent executor token (the store holds none; T-5.1:
        // Some-ness is the check, the exec_id names the attempt). The
        // actor's kind witness guarantees a store-credential report can
        // only ever consume materialization-kind attempts.
        let credential_kind = if request.get_ref().materialization_outcome.is_some() {
            PayloadCredentialKind::MaterializationStateEffecting {
                executor_body_token: "",
                service_body_token: "",
                request_instance: None,
            }
        } else {
            // No body token field on the frozen report signature.
            PayloadCredentialKind::Build { body_token: "" }
        };
        let auth_intent = match self.credential_for(credential_kind, &request) {
            Ok(ResolvedCredential::Executor(claims)) => claims.map(|c| c.intent_id),
            // Authorized store replica (or full dev mode): fleet-level
            // credential, no per-intent binding.
            Ok(ResolvedCredential::StoreService(_)) => None,
            Err(e) => {
                metrics::counter!(
                    "rio_scheduler_pull_rejected_total",
                    "rpc" => "report_outcome",
                    "reason" => if e.code() == tonic::Code::PermissionDenied {
                        "kind_unauthorized"
                    } else {
                        "unauthenticated"
                    }
                )
                .increment(1);
                return Err(e);
            }
        };
        let req = request.into_inner();

        let exec_id: uuid::Uuid = req
            .exec_id
            .parse()
            .map_err(|_| Status::invalid_argument("exec_id must be a UUID"))?;
        // Substitution-replacement (defense in depth, symmetric with the
        // pull-side rejection): an EXECUTOR-authenticated report never
        // carries a materialization outcome — a builder pod that somehow
        // learned a materialization attempt's exec_id must not be able to
        // consume it. Store-credential reports have `auth_intent == None`
        // (fleet-level identity), so this arm never fires for them; full
        // dev mode also has `None` and stays open.
        if auth_intent.is_some() && req.materialization_outcome.is_some() {
            metrics::counter!(
                "rio_scheduler_pull_rejected_total",
                "rpc" => "report_outcome",
                "reason" => "kind_unauthorized"
            )
            .increment(1);
            return Err(Status::permission_denied(
                "executor tokens do not authorize materialization outcome reports \
                 (a store-service credential is required)",
            ));
        }
        // bug_121: the payload class is parsed at the boundary, BEFORE
        // any defaulting — the contractual materialization wire shape
        // (report unset, materialization_outcome set) never synthesizes
        // a phantom InfrastructureFailure BuildResult and never logs
        // the no-result warn. (Some, Some) is malformed; (None, None)
        // is the legacy no-result build report, synthesized exactly as
        // before (the warn is genuine there).
        let payload = match parse_report_payload(req.report, req.materialization_outcome)? {
            ParsedReportPayload::Materialization(m) => crate::actor::pull::PullReportPayload {
                // Never read on the materialization arm (the witness
                // dispatch routes on the outcome, not the result).
                result: rio_proto::types::BuildResult::default(),
                peak_memory_bytes: 0,
                peak_cpu_cores: 0.0,
                node_name: None,
                hw_class: None,
                final_resources: None,
                final_line_count: 0,
                materialization_outcome: Some(m),
            },
            ParsedReportPayload::Build(report) => {
                let mut result = report.result.unwrap_or_else(|| {
                    warn!(exec_id = %req.exec_id, "ReportOutcome with no result, synthesizing InfrastructureFailure");
                    rio_proto::types::BuildResult {
                        status: rio_proto::types::BuildResultStatus::InfrastructureFailure.into(),
                        error_msg: "pod sent ReportOutcome with no result".into(),
                        ..Default::default()
                    }
                });
                // r[impl sched.executor.input-bounds+2]
                rio_common::grpc::truncate_utf8(&mut result.error_msg, MAX_ERROR_MSG_LEN);
                crate::actor::pull::PullReportPayload {
                    result,
                    peak_memory_bytes: report.peak_memory_bytes,
                    peak_cpu_cores: report.peak_cpu_cores,
                    node_name: report.node_name.filter(|s| s.len() <= MAX_IDENT_LEN),
                    hw_class: report.hw_class.filter(|s| s.len() <= MAX_IDENT_LEN),
                    final_resources: report.final_resources,
                    final_line_count: report.final_line_count,
                    materialization_outcome: None,
                }
            }
        };

        // send_unchecked: a dropped report would strand the attempt
        // until the establishment sweep; the pod retries until acked.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.actor
            .send_unchecked(ActorCommand::ReportPullOutcome {
                exec_id,
                auth_intent,
                payload,
                reply: reply_tx,
            })
            .await
            .map_err(Self::actor_error_to_status)?;
        match reply_rx
            .await
            .map_err(|_| Status::internal("actor dropped ReportOutcome reply"))?
        {
            Ok(()) => Ok(Response::new(rio_proto::types::ReportOutcomeResponse {})),
            Err(
                r @ (crate::actor::PullRejection::NotLeader
                | crate::actor::PullRejection::StaleGeneration),
            ) => Err(super::actor_guards::pull_rejection_to_status(&r)),
            // bug_182: the consumption close did not become durable —
            // the NACK rides UNAVAILABLE and the store's report
            // redelivery (600 s) re-presents the SAME outcome. Counted
            // so a PG brownout's NACK wave is visible.
            Err(r @ crate::actor::PullRejection::ConsumptionNotDurable) => {
                metrics::counter!(
                    "rio_scheduler_pull_rejected_total",
                    "rpc" => "report_outcome",
                    "reason" => "consumption_not_durable"
                )
                .increment(1);
                Err(super::actor_guards::pull_rejection_to_status(&r))
            }
            Err(r @ crate::actor::PullRejection::TokenMismatch) => {
                metrics::counter!(
                    "rio_scheduler_pull_rejected_total",
                    "rpc" => "report_outcome",
                    "reason" => "token_mismatch"
                )
                .increment(1);
                Err(super::actor_guards::pull_rejection_to_status(&r))
            }
            Err(r @ crate::actor::PullRejection::Internal(_)) => {
                Err(super::actor_guards::pull_rejection_to_status(&r))
            }
        }
    }

    /// Store-replica poll for claimable materialization jobs
    /// (substitution-replacement Phase A, design §2.2 item 1).
    ///
    /// Leader-served and read-only. The actor answers the empty list
    /// whenever materialization dispatch is disabled (the Phase A
    /// deployed state), the replica is standby, or no job is claimable
    /// — never an error (the AS-6 mixed-flag posture: a flag-on store
    /// polling a flag-off scheduler hangs harmlessly on empty lists).
    ///
    /// Identity (security review — dormant ≠ unprotected): job
    /// descriptors carry cross-tenant drv hashes and tenant ids, so the
    /// listing is NOT a per-intent executor surface. An executor token
    /// (the per-intent builder/fetcher credential) on either carrier is
    /// rejected `PermissionDenied`; the kind-attested store-service
    /// credential (`ServiceClaims{caller="rio-store"}`, the Wave-4
    /// obligation discharged here) is what authorizes it. Full dev mode
    /// (neither key family configured) stays open and answers the
    /// flag-off empty list.
    // r[impl sched.materialize.job+2]
    #[instrument(skip(self, request), fields(rpc = "ListMaterializationJobs"))]
    async fn list_materialization_jobs(
        &self,
        request: Request<rio_proto::types::ListMaterializationJobsRequest>,
    ) -> Result<Response<rio_proto::types::ListMaterializationJobsResponse>, Status> {
        self.executor_prologue(&request)?;
        // r[impl sec.executor.identity-token+3]
        // The state-effecting materialization family through the
        // chokepoint: an executor credential on either carrier is
        // rejected first (the Wave-3 rejection, kept verbatim), then
        // the instance-bound store credential is required (T-5.1: the
        // listing exposes cross-tenant job descriptors; Some-ness is
        // the check, the listing carries no executor_instance to
        // match). Full dev mode passes through to the flag gate
        // (empty list).
        self.credential_for(
            PayloadCredentialKind::MaterializationStateEffecting {
                executor_body_token: request.get_ref().service_token.as_str(),
                service_body_token: request.get_ref().service_token.as_str(),
                request_instance: None,
            },
            &request,
        )?;
        let req = request.into_inner();

        // send_unchecked: the store polls on an interval; a dropped
        // command is a delayed listing, never lost state.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.actor
            .send_unchecked(ActorCommand::ListMaterializationJobs {
                limit: req.limit,
                reply: reply_tx,
            })
            .await
            .map_err(Self::actor_error_to_status)?;
        let jobs = reply_rx
            .await
            .map_err(|_| Status::internal("actor dropped ListMaterializationJobs reply"))?;

        Ok(Response::new(
            rio_proto::types::ListMaterializationJobsResponse {
                jobs: jobs
                    .into_iter()
                    .map(|j| rio_proto::types::MaterializationJobDescriptor {
                        job_id: j.job_id.to_string(),
                        drv_hash: j.drv_hash,
                        tenant_id: j.tenant_id.map(|t| t.to_string()).unwrap_or_default(),
                        origin: j.origin.as_str().to_string(),
                    })
                    .collect(),
            },
        ))
    }

    /// Fire-and-forget byte progress for a running materialization
    /// attempt. Display-only, droppable.
    ///
    /// Phase B (BC-4): the relay — resolve the exec_id to its attempt's
    /// derivation and re-emit the byte progress as the same
    /// display-only `Event::SubstituteProgress` the walk's progress
    /// path emitted, on every interested build's log broadcast ring.
    /// Droppable end-to-end: a failed lookup, a missing attempt, and a
    /// full actor mailbox all drop the event silently (`try_send` —
    /// progress must never block or error; the terminal Cached/Completed
    /// event covers any dropped tick).
    ///
    /// Identity (security review — dormant ≠ unprotected): progress
    /// reports are fleet-level STORE operations like the listing, not
    /// per-intent executor operations. An executor token (the
    /// per-intent builder/fetcher credential) is rejected
    /// `PermissionDenied`; the kind-attested store-service credential
    /// (`ServiceClaims{caller="rio-store"}`) is what authorizes it.
    /// Full dev mode (neither key family configured) stays open. The
    /// gate prologue runs BEFORE any request-body parse and is pinned
    /// by the T-1.9 authentication sweep.
    #[instrument(skip(self, request), fields(rpc = "ReportMaterializationProgress"))]
    async fn report_materialization_progress(
        &self,
        request: Request<rio_proto::types::ReportMaterializationProgressRequest>,
    ) -> Result<Response<rio_proto::types::ReportMaterializationProgressResponse>, Status> {
        // bug_362: the prologue was MISSING here — a standby replica
        // ACKed progress reports, defeating the store client's
        // UNAVAILABLE-based leader failover (the balanced channel only
        // re-routes on UNAVAILABLE). Display-only does not mean
        // gate-free.
        self.executor_prologue(&request)?;
        // r[impl sec.executor.identity-token+3]
        // The display-only materialization family through the
        // chokepoint: executor credentials rejected, store-service
        // credential required (without the instance binding — no state
        // effect to bind). The proto carries no body token field, so
        // the metadata header is the only carrier. Full dev mode passes
        // through to the relay below.
        // r[impl sched.materialize.job+2]
        self.credential_for(PayloadCredentialKind::MaterializationDisplayOnly, &request)?;
        let req = request.into_inner();
        // A malformed exec_id is a caller bug worth surfacing even
        // though the payload itself is droppable.
        let exec_id: uuid::Uuid = req
            .exec_id
            .parse()
            .map_err(|_| Status::invalid_argument("exec_id must be a UUID"))?;
        // The relay (BC-4): exec_id → open attempt → derivation, then
        // the same actor command the walk's detached fetch posts. Every
        // failure arm acknowledges (display-only — never an error to
        // the reporting store).
        if let Some(db) = &self.db
            && let Ok(Some(attempt)) = db.find_attempt_by_exec_id(exec_id).await
        {
            // r[impl sched.executor.input-bounds+2]
            // upstream_uri lands in a display-only event ring; bound it
            // like the other worker-supplied identifier fields.
            let mut upstream_uri = req.upstream_uri;
            rio_common::grpc::truncate_utf8(&mut upstream_uri, MAX_IDENT_LEN);
            let _ = self.actor.try_send(ActorCommand::SubstituteProgress {
                drv_hash: attempt.core().drv_hash.clone().into(),
                bytes_done: req.bytes_done,
                bytes_expected: req.bytes_expected,
                upstream_uri,
            });
        } else {
            tracing::debug!(
                %exec_id,
                bytes_done = req.bytes_done,
                bytes_expected = req.bytes_expected,
                "materialization progress for an unknown/superseded exec dropped (display-only)"
            );
        }
        Ok(Response::new(
            rio_proto::types::ReportMaterializationProgressResponse {},
        ))
    }
}

/// One `ReportOutcomeRequest` payload, classified at the wire boundary
/// (bug_121): the class is decided from FIELD PRESENCE before any
/// defaulting, so a contractual materialization report never grows a
/// synthesized build result.
#[derive(Debug)]
pub(super) enum ParsedReportPayload {
    /// A build report (the `report` field; `None` inside means the
    /// legacy no-result shape, synthesized by the caller WITH the warn).
    /// Boxed: the report dwarfs the materialization arm
    /// (clippy::large_enum_variant) and is unboxed exactly once.
    Build(Box<rio_proto::types::CompletionReport>),
    /// A materialization outcome (`materialization_outcome` set,
    /// `report` unset — the contractual store-client wire shape).
    Materialization(rio_proto::types::MaterializationOutcome),
}

/// Classify the mutually-exclusive payload pair. Pure; the unit table
/// below is its contract.
pub(super) fn parse_report_payload(
    report: Option<rio_proto::types::CompletionReport>,
    materialization_outcome: Option<rio_proto::types::MaterializationOutcome>,
) -> Result<ParsedReportPayload, Status> {
    match (report, materialization_outcome) {
        (Some(_), Some(_)) => Err(Status::invalid_argument(
            "report and materialization_outcome are mutually exclusive",
        )),
        (None, Some(m)) => Ok(ParsedReportPayload::Materialization(m)),
        (report, None) => Ok(ParsedReportPayload::Build(Box::new(
            report.unwrap_or_default(),
        ))),
    }
}

#[cfg(test)]
mod payload_tests {
    use super::*;

    /// bug_121's table: the materialization wire shape parses as
    /// Materialization with NO synthesized BuildResult; both-set is
    /// malformed; the legacy no-result shape stays a Build whose inner
    /// result is None (the caller synthesizes + warns there, and only
    /// there).
    #[test]
    fn report_payload_parse_table() {
        let mat = rio_proto::types::MaterializationOutcome::default();
        let report = rio_proto::types::CompletionReport::default();

        match parse_report_payload(None, Some(mat.clone())) {
            Ok(ParsedReportPayload::Materialization(_)) => {}
            other => panic!("(None, Some) must be Materialization, got {other:?}"),
        }
        assert_eq!(
            parse_report_payload(Some(report.clone()), Some(mat))
                .unwrap_err()
                .code(),
            tonic::Code::InvalidArgument
        );
        match parse_report_payload(Some(report), None) {
            Ok(ParsedReportPayload::Build(r)) => assert!(r.result.is_none() || r.result.is_some()),
            other => panic!("(Some, None) must be Build, got {other:?}"),
        }
        match parse_report_payload(None, None) {
            Ok(ParsedReportPayload::Build(r)) => {
                assert!(
                    r.result.is_none(),
                    "legacy shape keeps result=None for the caller's synthesis"
                )
            }
            other => panic!("(None, None) must be the legacy Build shape, got {other:?}"),
        }
    }
}
