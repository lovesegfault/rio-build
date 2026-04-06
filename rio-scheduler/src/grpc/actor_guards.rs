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
    match err {
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
    }
}
