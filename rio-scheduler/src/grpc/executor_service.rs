//! `ExecutorService` gRPC implementation for [`SchedulerGrpc`].
//!
//! Worker-facing RPCs: the pull-mode `PullAssignment`/`ReportOutcome`
//! unaries. The legacy `BuildExecution` stream and `Heartbeat` unary
//! are unconditional error stubs — the session machinery behind them
//! was deleted; the RPCs themselves (and the generated trait methods)
//! stay until the 1d proto sweep so a stray stream-mode executor gets
//! a clear error instead of a hang.

use tokio_stream::wrappers::ReceiverStream;
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
// (BuildExecution / Heartbeat are unconditional error stubs: no field
// of theirs reaches the actor.)
//
// PullAssignment RPC (pull-mode dispatch):
//   intent_id                         → DAG lookup, assignments/executions rows → reject RPC > MAX_IDENT_LEN
//   executor_token                    → HMAC-verified then dropped        → reject RPC > MAX_EXECUTOR_TOKEN_LEN
//                                       (verification fails on any tamper; the bound only caps the hash work)
// ReportOutcome RPC (pull-mode dispatch):
//   exec_id                           → UUID parse → attempt lookup       → reject RPC if not a valid UUID
//   report.result.error_msg           → build_event_log × N, ring, term   → truncate to MAX_ERROR_MSG_LEN
//   report.node_name / report.hw_class → build_samples row                → None if > MAX_IDENT_LEN
//   report.drv_path                   → never read (exec_id names the attempt) → dropped before the actor
//   report numerics (peak_*, final_resources.*) → build_samples row       → validated actor-side (completion.rs
//                                       record_build_sample: finite/in-domain or NULL; .min(i64::MAX) clamps)

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
/// `(1 + cascaded_ancestors) × interested_builds` `build_event_log` rows,
/// state-ring slots, and `nix build -L` terminals.
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

#[tonic::async_trait]
impl ExecutorService for SchedulerGrpc {
    type BuildExecutionStream = ReceiverStream<Result<rio_proto::types::SchedulerMessage, Status>>;

    /// Unconditional error stub. The stream dispatch protocol was
    /// removed with the scheduler session machinery: work is delivered
    /// by `PullAssignment` and reported by `ReportOutcome`. The RPC and
    /// the generated trait method survive only until the 1d proto sweep
    /// (the tonic server trait has no default method bodies), so a
    /// stray stream-mode executor gets a clear error instead of a hang.
    #[instrument(skip(self, _request), fields(rpc = "BuildExecution"))]
    async fn build_execution(
        &self,
        _request: Request<tonic::Streaming<rio_proto::types::ExecutorMessage>>,
    ) -> Result<Response<Self::BuildExecutionStream>, Status> {
        warn!("BuildExecution called: the stream dispatch protocol has been removed");
        // Deletion-gate / post-deletion-watch successor signal (rows
        // D6/D7): a sustained rate here means a stream-mode executor
        // image is still deployed and trying to register.
        metrics::counter!("rio_scheduler_stream_stub_calls_total", "rpc" => "build_execution")
            .increment(1);
        Err(Status::unimplemented(
            "the BuildExecution stream protocol has been removed; \
             executors are dispatched via PullAssignment/ReportOutcome (pull mode)",
        ))
    }

    /// Unconditional error stub, same rationale as
    /// [`Self::build_execution`]: there is no scheduler-side liveness
    /// or registration state to feed. Pod liveness belongs to the
    /// kubelet/Job controller; attempt liveness is the durable
    /// open-attempt row plus the establishment sweep.
    #[instrument(skip(self, _request), fields(rpc = "Heartbeat"))]
    async fn heartbeat(
        &self,
        _request: Request<rio_proto::types::HeartbeatRequest>,
    ) -> Result<Response<rio_proto::types::HeartbeatResponse>, Status> {
        warn!("Heartbeat called: the stream session protocol has been removed");
        metrics::counter!("rio_scheduler_stream_stub_calls_total", "rpc" => "heartbeat")
            .increment(1);
        Err(Status::unimplemented(
            "the Heartbeat RPC has been removed with the stream session protocol; \
             pull-mode executors need no heartbeat",
        ))
    }

    // Pull-mode dispatch surface — the only work-delivery path.

    /// The pull-mode pod's single ask. Leader-served; the actor turn
    /// runs the admission kernel and (on Deliver) the one fenced
    /// transaction. A re-pull while the attempt is open returns the
    /// identical payload and exec_id.
    // r[impl sched.executor.pull-transaction]
    #[instrument(skip(self, request), fields(rpc = "PullAssignment"))]
    async fn pull_assignment(
        &self,
        request: Request<rio_proto::types::PullAssignmentRequest>,
    ) -> Result<Response<rio_proto::types::PullAssignmentResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        // r[impl sec.executor.identity-token+2]
        // The same token↔intent binding the stream/heartbeat path
        // enforces, applied per-unary. The token may arrive in metadata
        // (x-rio-executor-token) or in the request body (the unary is
        // self-contained for clients that cannot set per-call
        // metadata); either carrier is verified by the same key.
        let auth_intent = match self.require_executor(&request) {
            Ok(claims) => claims.map(|c| c.intent_id),
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
                        Status::unauthenticated(format!("executor_token verification failed: {e}"))
                    })?;
                Some(claims.intent_id)
            }
        };
        let req = request.into_inner();
        if req.intent_id.is_empty() {
            return Err(Status::invalid_argument("intent_id is required"));
        }
        // r[impl sched.executor.input-bounds+2]
        rio_common::grpc::check_bound("intent_id bytes", req.intent_id.len(), MAX_IDENT_LEN)?;

        // send_unchecked: a dropped pull would park the pod for a full
        // backoff interval; the pod retries anyway, so backpressure
        // surfaces as retried pulls, never lost work.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.actor
            .send_unchecked(ActorCommand::PullAssignment {
                intent_id: req.intent_id,
                auth_intent,
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
        // The metadata header is the report's only identity carrier
        // (the frozen signature has no body token field), so a missing
        // or invalid token under the enforced HMAC posture is counted
        // for alertability — the rejected pod's own logs are ephemeral.
        let auth_intent = match self.require_executor(&request) {
            Ok(claims) => claims.map(|c| c.intent_id),
            Err(e) => {
                metrics::counter!(
                    "rio_scheduler_pull_rejected_total",
                    "rpc" => "report_outcome",
                    "reason" => "unauthenticated"
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
        let payload = crate::actor::pull::PullReportPayload {
            result,
            peak_memory_bytes: report.peak_memory_bytes,
            peak_cpu_cores: report.peak_cpu_cores,
            node_name: report.node_name.filter(|s| s.len() <= MAX_IDENT_LEN),
            hw_class: report.hw_class.filter(|s| s.len() <= MAX_IDENT_LEN),
            final_resources: report.final_resources,
            final_line_count: report.final_line_count,
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
}
