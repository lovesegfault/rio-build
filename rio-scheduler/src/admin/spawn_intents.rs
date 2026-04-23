//! `AdminService.GetSpawnIntents` implementation.
//!
//! Thin actor query: the actor's `compute_spawn_intents` does the
//! single-pass Ready scan + filter + `solve_intent_for`; this module
//! translates the proto request to the internal `SpawnIntentsRequest`
//! and the actor result to the proto response.

use rio_proto::types::{ExecutorKind, GetSpawnIntentsRequest, GetSpawnIntentsResponse};
use tonic::Status;

use crate::actor::{ActorCommand, ActorHandle, AdminQuery, SpawnIntentsRequest};

/// Query the actor for the spawn-intent snapshot, convert to proto.
// r[impl sched.admin.spawn-intents]
pub(super) async fn get_spawn_intents(
    actor: &ActorHandle,
    req: GetSpawnIntentsRequest,
) -> Result<GetSpawnIntentsResponse, Status> {
    // `optional ExecutorKind` → `Option<i32>` in prost. None =
    // unfiltered; out-of-range falls back to unfiltered.
    let kind = req.kind.and_then(|k| ExecutorKind::try_from(k).ok());
    // I-176: proto3 repeated can't be optional, so the wire shape is
    // `(filter_features: bool, features: Vec)`. Collapse to Option here
    // so the actor sees the tristate directly: false → None
    // (unfiltered); true → Some(vec) (filter, even when vec is empty =
    // "I support no features").
    let features = req.filter_features.then_some(req.features);
    let actor_req = SpawnIntentsRequest {
        kind,
        systems: req.systems,
        features,
    };

    // The pool reconcilers read this to set per-pool spawn targets.
    // Dropping under backpressure blinds the autoscaler exactly when it
    // should scale up — same reasoning as ClusterStatus.
    let snap = super::query_actor(actor, |reply| {
        ActorCommand::Admin(AdminQuery::GetSpawnIntents {
            req: actor_req,
            reply,
        })
    })
    .await?;

    Ok(GetSpawnIntentsResponse {
        intents: snap.intents,
        queued_by_system: snap.queued_by_system,
        // §13b dead-node detector + ICE mask: populated by B11. Empty
        // until the actor's `compute_spawn_intents` carries them.
        dead_nodes: vec![],
        ice_masked_cells: vec![],
    })
}
