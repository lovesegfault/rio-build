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
//                                       kind=MATERIALIZATION (mandatory); ignored/dropped for kind=BUILD
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
// ReportMaterializationProgress RPC (Phase A: validate + acknowledge-and-drop, per the wire contract):
//   AUTHZ: the same store-service credential gate as the other materialization
//   operations (an executor token on the metadata carrier → reject RPC
//   PermissionDenied; no credential under a configured key family → reject RPC
//   Unauthenticated; only full dev mode is open). The proto carries no body
//   token field, so x-rio-service-token metadata is the only carrier.
//   exec_id                           → UUID-parsed for validation, then dropped → reject RPC if not a
//                                       valid UUID
//   upstream_uri / bytes_*            → never read in Phase A (droppable display payload) → dropped
//                                       before any state; bounds land with the Phase-B relay

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
/// component): lowercase alphanumerics and interior hyphens, 1–63
/// chars. The instance is interpolated into the composite materialization
/// ExecutorId (`{intent}@{instance}`), so the alphabet exclusion of `@`
/// (and everything else) is what keeps that composite unambiguous.
pub(super) fn is_dns1123_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

/// `ServiceClaims.caller` values whose service token authorizes
/// materialization operations (the Wave-4 kind-attested store
/// credential). Exactly the store service: the credential attests the
/// caller IS a store replica — the work class materialization belongs
/// to — not merely "some control-plane service".
const MATERIALIZATION_SERVICE_CALLERS: &[&str] = &["rio-store"];

/// Outcome of the store-service credential gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StoreServiceAuth {
    /// A valid `ServiceClaims{caller="rio-store"}` token was presented.
    Authorized,
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
    // r[impl sched.materialize.job]
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
        Ok(StoreServiceAuth::Authorized)
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
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
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
        // r[impl sec.executor.identity-token+2]
        // Identity gates, per work class:
        //
        // BUILD pulls (the as-built path, byte-identical): the executor
        // token↔intent binding the stream/heartbeat path enforces,
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
        let auth_claims: Option<rio_auth::hmac::ExecutorClaims> =
            if kind == rio_evidence_kernel::pull::PullKind::Materialization {
                let body_token = request.get_ref().executor_token.as_str();
                if self
                    .verified_executor_claims(&request, body_token)
                    .is_some()
                {
                    metrics::counter!(
                        "rio_scheduler_pull_rejected_total",
                        "rpc" => "pull_assignment",
                        "reason" => "kind_unauthorized"
                    )
                    .increment(1);
                    return Err(Status::permission_denied(
                        "executor tokens do not authorize materialization pulls \
                     (a store-service credential is required)",
                    ));
                }
                // r[impl sched.materialize.job]
                if let Err(e) = self.require_store_service(&request, "") {
                    metrics::counter!(
                        "rio_scheduler_pull_rejected_total",
                        "rpc" => "pull_assignment",
                        "reason" => "kind_unauthorized"
                    )
                    .increment(1);
                    return Err(e);
                }
                // The store credential is fleet-level, not per-intent: the
                // pulling identity is the composite (intent, replica) pair
                // the kernel arbitrates on, so there is no auth_intent.
                None
            } else {
                match self.require_executor(&request) {
                    Ok(claims) => claims,
                    Err(metadata_err) => {
                        let body_token = request.get_ref().executor_token.as_str();
                        if body_token.is_empty() {
                            metrics::counter!(
                                "rio_scheduler_pull_rejected_total",
                                "rpc" => "pull_assignment",
                                "reason" => "unauthenticated"
                            )
                            .increment(1);
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
                                metrics::counter!(
                                    "rio_scheduler_pull_rejected_total",
                                    "rpc" => "pull_assignment",
                                    "reason" => "unauthenticated"
                                )
                                .increment(1);
                                Status::unauthenticated(format!(
                                    "executor_token verification failed: {e}"
                                ))
                            })?;
                        Some(claims)
                    }
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
            Err(crate::actor::PullRejection::NotLeader)
            | Err(crate::actor::PullRejection::StaleGeneration) => {
                // The same retryable not-leader class `ensure_leader`
                // produces — the pod retries against the real leader.
                return Err(Status::unavailable("not leader (standby replica)"));
            }
            Err(crate::actor::PullRejection::TokenMismatch) => {
                metrics::counter!(
                    "rio_scheduler_pull_rejected_total",
                    "rpc" => "pull_assignment",
                    "reason" => "token_mismatch"
                )
                .increment(1);
                return Err(Status::permission_denied(
                    "executor token is bound to a different intent",
                ));
            }
            Err(crate::actor::PullRejection::Internal(msg)) => {
                return Err(Status::internal(msg));
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
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        // r[impl sec.executor.identity-token+2]
        // Identity, per payload class:
        //
        // BUILD reports (the as-built path, byte-identical): the
        // metadata header is the report's only identity carrier (the
        // frozen signature has no body token field), so a missing or
        // invalid token under the enforced HMAC posture is counted for
        // alertability — the rejected pod's own logs are ephemeral.
        //
        // MATERIALIZATION reports (`materialization_outcome` set): the
        // reporter is a store replica, whose identity is the
        // kind-attested store-service credential — not a per-intent
        // executor token (the store holds none). The exec_id names the
        // attempt; the actor's kind dispatch guarantees a
        // store-credential report can only ever consume
        // materialization-kind attempts (a build attempt
        // acknowledges-and-ignores the payload).
        let auth_intent = match self.require_executor(&request) {
            Ok(claims) => claims.map(|c| c.intent_id),
            Err(metadata_err) => {
                if request.get_ref().materialization_outcome.is_some() {
                    // r[impl sched.materialize.job]
                    match self.require_store_service(&request, "") {
                        // Authorized store replica (or full dev mode):
                        // fleet-level credential, no per-intent binding.
                        Ok(_) => None,
                        Err(e) => {
                            metrics::counter!(
                                "rio_scheduler_pull_rejected_total",
                                "rpc" => "report_outcome",
                                "reason" => "kind_unauthorized"
                            )
                            .increment(1);
                            return Err(e);
                        }
                    }
                } else {
                    metrics::counter!(
                        "rio_scheduler_pull_rejected_total",
                        "rpc" => "report_outcome",
                        "reason" => "unauthenticated"
                    )
                    .increment(1);
                    return Err(metadata_err);
                }
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
        let has_report = req.report.is_some();
        let report = req.report.unwrap_or_default();
        // The exec_id names the attempt; the report's drv_path is not
        // used for routing (and is therefore not length-gated here —
        // it is dropped before the actor sees it). Payload fields get
        // the same bound-don't-reject treatment as the stream arm.
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
        // Substitution-replacement: a request carrying BOTH a build
        // report and a materialization outcome is malformed (the proto
        // documents them as mutually exclusive).
        if req.materialization_outcome.is_some() && has_report {
            return Err(Status::invalid_argument(
                "report and materialization_outcome are mutually exclusive",
            ));
        }
        let payload = crate::actor::pull::PullReportPayload {
            result,
            peak_memory_bytes: report.peak_memory_bytes,
            peak_cpu_cores: report.peak_cpu_cores,
            node_name: report.node_name.filter(|s| s.len() <= MAX_IDENT_LEN),
            hw_class: report.hw_class.filter(|s| s.len() <= MAX_IDENT_LEN),
            final_resources: report.final_resources,
            final_line_count: report.final_line_count,
            materialization_outcome: req.materialization_outcome,
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
            Err(crate::actor::PullRejection::NotLeader)
            | Err(crate::actor::PullRejection::StaleGeneration) => {
                Err(Status::unavailable("not leader (standby replica)"))
            }
            Err(crate::actor::PullRejection::TokenMismatch) => {
                metrics::counter!(
                    "rio_scheduler_pull_rejected_total",
                    "rpc" => "report_outcome",
                    "reason" => "token_mismatch"
                )
                .increment(1);
                Err(Status::permission_denied(
                    "executor token is bound to a different intent",
                ))
            }
            Err(crate::actor::PullRejection::Internal(msg)) => Err(Status::internal(msg)),
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
    // r[impl sched.materialize.job]
    #[instrument(skip(self, request), fields(rpc = "ListMaterializationJobs"))]
    async fn list_materialization_jobs(
        &self,
        request: Request<rio_proto::types::ListMaterializationJobsRequest>,
    ) -> Result<Response<rio_proto::types::ListMaterializationJobsResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        // r[impl sec.executor.identity-token+2]
        // A token that verifies as an EXECUTOR credential (on either
        // carrier — the x-rio-executor-token header or the body
        // service_token field) never authorizes fleet-wide listing:
        // reject closed (the Wave-3 rejection, kept verbatim).
        let body_token = request.get_ref().service_token.as_str();
        if self
            .verified_executor_claims(&request, body_token)
            .is_some()
        {
            return Err(Status::permission_denied(
                "executor tokens do not authorize materialization-job listing \
                 (a store-service credential is required)",
            ));
        }
        // The store-service credential gate: x-rio-service-token
        // metadata (the ServiceTokenInterceptor carrier) or the body
        // service_token field, verified as ServiceClaims by the
        // service-HMAC key with caller="rio-store". Full dev mode
        // passes through to the flag gate (empty list).
        self.require_store_service(&request, body_token)?;
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
    /// Phase A: validates the exec_id format, acknowledges, and drops —
    /// exactly as the wire contract documents (no materialization
    /// attempt can exist while the flags are off, and progress is
    /// droppable by contract even when one does). Phase B wires the
    /// relay to build events (BC-4).
    ///
    /// Identity (security review — dormant ≠ unprotected): progress
    /// reports are fleet-level STORE operations like the listing, not
    /// per-intent executor operations. An executor token (the
    /// per-intent builder/fetcher credential) is rejected
    /// `PermissionDenied`; the kind-attested store-service credential
    /// (`ServiceClaims{caller="rio-store"}`) is what authorizes it.
    /// Full dev mode (neither key family configured) stays open and
    /// answers the Phase-A ack-and-drop.
    #[instrument(skip(self, request), fields(rpc = "ReportMaterializationProgress"))]
    async fn report_materialization_progress(
        &self,
        request: Request<rio_proto::types::ReportMaterializationProgressRequest>,
    ) -> Result<Response<rio_proto::types::ReportMaterializationProgressResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // r[impl sec.executor.identity-token+2]
        // A token that verifies as an EXECUTOR credential never
        // authorizes materialization operations: reject closed (the
        // Wave-3 rejection, kept verbatim). The proto carries no body
        // token field, so the metadata header is the only carrier.
        if self.verified_executor_claims(&request, "").is_some() {
            return Err(Status::permission_denied(
                "executor tokens do not authorize materialization progress reports \
                 (a store-service credential is required)",
            ));
        }
        // The store-service credential gate: x-rio-service-token
        // metadata (the ServiceTokenInterceptor carrier), verified as
        // ServiceClaims by the service-HMAC key with caller="rio-store".
        // Full dev mode passes through to the Phase-A ack-and-drop.
        // r[impl sched.materialize.job]
        self.require_store_service(&request, "")?;
        let req = request.into_inner();
        // Validate-then-drop: a malformed exec_id is a caller bug worth
        // surfacing even though the payload itself is droppable.
        let exec_id: uuid::Uuid = req
            .exec_id
            .parse()
            .map_err(|_| Status::invalid_argument("exec_id must be a UUID"))?;
        tracing::debug!(
            %exec_id,
            bytes_done = req.bytes_done,
            bytes_expected = req.bytes_expected,
            "materialization progress acknowledged and dropped (Phase A: relay is dormant)"
        );
        Ok(Response::new(
            rio_proto::types::ReportMaterializationProgressResponse {},
        ))
    }
}
