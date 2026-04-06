//! Shared actor-liveness and leader guards for gRPC handlers.
//!
//! Both [`SchedulerGrpc`](super::SchedulerGrpc) and
//! [`AdminServiceImpl`](crate::admin::AdminServiceImpl) call these at
//! the top of every handler. Free functions rather than methods so the
//! same body serves both structs without drift — the three call sites
//! had three different error strings before consolidation (P0383).

use std::sync::atomic::{AtomicBool, Ordering};

use tonic::Status;

use crate::actor::ActorHandle;

/// Actor-dead check. If the actor panicked, all commands would hang on
/// a closed channel — return UNAVAILABLE early instead so clients retry
/// on a healthy replica.
pub(crate) fn check_actor_alive(actor: &ActorHandle) -> Result<(), Status> {
    if !actor.is_alive() {
        return Err(Status::unavailable(
            "scheduler actor is unavailable (panicked or exited)",
        ));
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
