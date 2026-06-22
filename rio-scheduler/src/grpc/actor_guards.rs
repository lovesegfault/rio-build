//! Shared actor-liveness and leader guards for gRPC handlers.
//!
//! Both [`SchedulerGrpc`](super::SchedulerGrpc) and
//! [`AdminServiceImpl`](crate::admin::AdminServiceImpl) call these at
//! the top of every handler. Free functions rather than methods so the
//! same body serves both structs without drift — the three call sites
//! had three different error strings before consolidation (P0383).

use std::sync::atomic::{AtomicBool, Ordering};

use tonic::Status;

use crate::actor::{ActorError, ActorHandle};

/// Canonical actor-dead message. Used by both [`check_actor_alive`]
/// (pre-send liveness probe) and [`actor_error_to_status`] (post-send
/// ChannelSend failure). Operators grep for one signature.
pub(crate) const ACTOR_UNAVAILABLE_MSG: &str =
    "scheduler actor is unavailable (panicked or exited)";

/// Actor-dead check. If the actor panicked, all commands would hang on
/// a closed channel — return UNAVAILABLE early instead so clients retry
/// on a healthy replica.
pub(crate) fn check_actor_alive(actor: &ActorHandle) -> Result<(), Status> {
    if !actor.is_alive() {
        return Err(Status::unavailable(ACTOR_UNAVAILABLE_MSG));
    }
    Ok(())
}

// r[impl sched.grpc.leader-guard]
/// Return UNAVAILABLE when this replica is not the leader. Called at
/// the top of every handler, before any actor interaction. Standby
/// replicas keep the gRPC server up (so the process is Ready from
/// K8s's PoV) but refuse all RPCs — clients with a health-aware
/// balanced channel see NOT_SERVING from grpc.health.v1 and route
/// elsewhere.
///
/// A bare `Status::unavailable` (not `Status::failed_precondition`)
/// because tonic's p2c balancer ejects endpoints on
/// UNAVAILABLE-at-connection but NOT on RPC-level errors; clients
/// retry on UNAVAILABLE by convention (health-aware balancer has
/// already removed us, so retry goes to leader).
pub(crate) fn ensure_leader(is_leader: &AtomicBool) -> Result<(), Status> {
    if !is_leader.load(Ordering::Relaxed) {
        return Err(Status::unavailable("not leader (standby replica)"));
    }
    Ok(())
}

/// Map an [`ActorError`] to a gRPC `Status`. Shared by `SchedulerGrpc` +
/// `AdminServiceImpl` handlers that forward actor-send errors. Free
/// function so callers don't need the awkward `crate::grpc::SchedulerGrpc::`
/// path — seven admin submodules + worker_service all call this.
pub(crate) fn actor_error_to_status(err: ActorError) -> Status {
    let status = match &err {
        ActorError::BuildNotFound(id) => Status::not_found(format!("build not found: {id}")),
        ActorError::Backpressure => {
            Status::resource_exhausted("scheduler is overloaded, please retry later")
        }
        // ChannelSend = actor's mpsc receiver dropped. Either the
        // actor panicked OR it exited on its shutdown-token arm
        // during drain. UNAVAILABLE (retriable) not INTERNAL —
        // BalancedChannel clients retry on the next replica; with
        // INTERNAL they'd surface the error to the user. Same
        // string as `check_actor_alive` so operators grep for one
        // signature, not two.
        ActorError::ChannelSend => Status::unavailable(ACTOR_UNAVAILABLE_MSG),
        ActorError::Database(e) => Status::internal(format!("database error: {e}")),
        ActorError::Dag(e) => Status::internal(format!("DAG merge failed: {e}")),
        ActorError::MissingDbId { .. } => Status::internal(err.to_string()),
        // UNAVAILABLE — gateway/client sees this as a retriable error.
        // They should back off and retry; the breaker auto-closes in 30s
        // or on the next successful probe.
        ActorError::StoreUnavailable => {
            Status::unavailable("store service is unreachable; cache-check circuit breaker is open")
        }
        ActorError::PermissionDenied { .. } => Status::permission_denied(err.to_string()),
        // Same string as `ensure_leader` above so operators grep for
        // one signature.
        ActorError::NotLeader => Status::unavailable("not leader (standby replica)"),
        // r[impl sched.evidence.durability+4]
        // r[impl sched.grpc.fence-retryable]
        // UNAVAILABLE (the ensure_leader family), NOT
        // FAILED_PRECONDITION: the fence trips in the deposed-believer
        // window and the request is perfectly valid on the live
        // leader — it must surface as a retryable refusal. No client
        // retries FAILED_PRECONDITION (the gateway's bounded
        // SubmitBuild retry keys on `is_retryable_refusal_code`), so
        // the previous mapping turned every fence trip into a
        // user-visible failure.
        ActorError::StaleGeneration { .. } => Status::unavailable(err.to_string()),
    };
    // The code DERIVES from the retry class
    // (sched.grpc.fence-retryable): pinned exhaustively by the
    // retry_class_code_consistency unit test; the debug_assert keeps
    // the derivation honest on every dev-mode mapping.
    // refusal-census: allow(consistency debug_assert of a derivation the
    // retry_class_code_consistency census pins — not an adjudication site)
    debug_assert_eq!(
        err.retry_class() == crate::actor::RetryClass::Retryable,
        rio_proto::is_retryable_refusal_code(status.code()),
        "actor_error_to_status: code/class divergence for {err:?}"
    );
    status
}

// r[impl sched.grpc.fence-retryable]
/// The shared status mapping for `PullRejection` — every executor-
/// facing handler derives its refusal Status here (the per-RPC metrics
/// stay at the call sites). Same class law as
/// [`actor_error_to_status`].
pub(crate) fn pull_rejection_to_status(rejection: &crate::actor::PullRejection) -> Status {
    use crate::actor::PullRejection;
    let status = match rejection {
        // The retryable not-leader class `ensure_leader` produces —
        // the pod retries against the real leader.
        PullRejection::NotLeader | PullRejection::StaleGeneration => {
            Status::unavailable("not leader (standby replica)")
        }
        // bug_182 (the NACK law): the consumption close did not become
        // durable — UNAVAILABLE so the store's report redelivery
        // retries the SAME outcome against this replica.
        // merged_bug_031: the refusal is LEADER-emitted (only the
        // leader's consumption path reaches it), so it carries the
        // typed leader-NACK marker — the store's transport keeps the
        // pinned connection instead of re-rolling away from the
        // leader on every PG-brownout NACK.
        PullRejection::ConsumptionNotDurable => {
            let mut status =
                Status::unavailable("consumption close not durable; re-deliver the report");
            status.metadata_mut().insert(
                rio_common::grpc::LEADER_NACK_METADATA_KEY,
                rio_common::grpc::LEADER_NACK_NOT_DURABLE
                    .parse()
                    .expect("static ascii metadata value"),
            );
            status
        }
        PullRejection::TokenMismatch => {
            Status::permission_denied("executor token is bound to a different intent")
        }
        PullRejection::Internal(msg) => Status::internal(msg.clone()),
    };
    // refusal-census: allow(consistency debug_assert of a derivation the
    // retry_class_code_consistency census pins — not an adjudication site)
    debug_assert_eq!(
        rejection.retry_class() == crate::actor::RetryClass::Retryable,
        rio_proto::is_retryable_refusal_code(status.code()),
        "pull_rejection_to_status: code/class divergence for {rejection:?}"
    );
    status
}

#[cfg(test)]
mod tests {
    /// merged_bug_031: the ConsumptionNotDurable NACK carries the
    /// leader-NACK metadata marker (x-rio-leader-nack) so the store's
    /// transport can separate retry-class from peer-identity — the
    /// NACK retries against THIS replica instead of abandoning the
    /// leader-pinned connection. Bare-UNAVAILABLE refusals (standby
    /// shapes) stay unmarked.
    #[test]
    fn consumption_nack_carries_the_leader_marker() {
        let nack =
            super::pull_rejection_to_status(&crate::actor::PullRejection::ConsumptionNotDurable);
        assert_eq!(nack.code(), tonic::Code::Unavailable);
        assert!(
            rio_common::grpc::is_leader_nack(&nack),
            "the leader-emitted NACK must carry {}",
            rio_common::grpc::LEADER_NACK_METADATA_KEY
        );

        let standby = super::pull_rejection_to_status(&crate::actor::PullRejection::NotLeader);
        assert_eq!(standby.code(), tonic::Code::Unavailable);
        assert!(
            !rio_common::grpc::is_leader_nack(&standby),
            "standby refusals stay bare — they MUST re-roll the connection"
        );
    }
}
