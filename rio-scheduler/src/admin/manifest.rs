//! `AdminService.GetCapacityManifest` implementation.
//!
//! Queries the actor for per-ready-derivation bucketed resource
//! estimates, converts to proto. Polled by the controller's manifest
//! reconciler under `BuilderPool.spec.sizing=Manifest` (ADR-020).

use rio_proto::types::{DerivationResourceEstimate, GetCapacityManifestResponse};
use tonic::Status;

use crate::actor::{ActorCommand, ActorHandle};
use crate::estimator::BucketedEstimate;

/// Walk the ready queue inside the actor, bucket via the Estimator,
/// convert to proto.
// r[impl sched.admin.capacity-manifest]
pub(super) async fn get_capacity_manifest(
    actor: &ActorHandle,
) -> Result<GetCapacityManifestResponse, Status> {
    // send_unchecked: same reasoning as ClusterStatus. The controller
    // polls this to size the builder fleet — dropping the query under
    // backpressure would blind it exactly when the queue is deepest.
    let estimates = actor
        .query_unchecked(|reply| ActorCommand::CapacityManifest { reply })
        .await
        .map_err(crate::grpc::SchedulerGrpc::actor_error_to_status)?;

    Ok(GetCapacityManifestResponse {
        estimates: estimates.into_iter().map(to_proto).collect(),
    })
}

fn to_proto(b: BucketedEstimate) -> DerivationResourceEstimate {
    DerivationResourceEstimate {
        est_memory_bytes: b.memory_bytes,
        est_cpu_millicores: b.cpu_millicores,
        est_duration_secs: b.duration_secs,
    }
}
