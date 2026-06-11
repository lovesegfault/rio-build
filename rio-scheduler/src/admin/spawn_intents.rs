//! `AdminService.GetSpawnIntents` implementation.
//!
//! Thin actor query: the actor's `compute_spawn_intents` does the
//! single-pass Ready scan + filter + `solve_intent_for`; this module
//! translates the proto request to the internal `SpawnIntentsRequest`
//! and the actor result to the proto response.

use rio_proto::types::{
    ExecutorKind, GetSpawnIntentsRequest, GetSpawnIntentsResponse, MintExecutorTokensRequest,
    MintExecutorTokensResponse,
};
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

    let resp = GetSpawnIntentsResponse {
        intents: snap.intents,
        queued_by_system: snap.queued_by_system,
        ice_masked_cells: snap.ice_masked_cells,
    };
    // Wire-size measurement (round-9 dossier E2 — the B-2 gate for
    // the GetSpawnIntents pagination constants): the response is the
    // largest unpaginated rio surface (full Ready set per call, 379
    // calls/12min at the incident fleet) and until now NO rio gRPC
    // surface measured encoded response bytes — the 150-400 B/intent
    // figure the admission census used was derived, not observed.
    // `encoded_len` is the exact prost wire size without re-encoding;
    // the per-response intent count alongside it lets PromQL derive
    // observed bytes-per-intent. Emitted at the serving chokepoint so
    // every consumer (per-pool reconcilers, cover sizing) is counted.
    let encoded_len = rio_proto::prost::Message::encoded_len(&resp);
    metrics::histogram!("rio_scheduler_spawn_intents_response_bytes").record(encoded_len as f64);
    metrics::histogram!("rio_scheduler_spawn_intents_per_response")
        .record(resp.intents.len() as f64);
    Ok(resp)
}

/// Query the actor for per-intent `ExecutorClaims` tokens.
/// Controller-only — callers MUST have passed
/// `ensure_service_caller(&["rio-controller"])`.
pub(super) async fn mint_executor_tokens(
    actor: &ActorHandle,
    req: MintExecutorTokensRequest,
) -> Result<MintExecutorTokensResponse, Status> {
    let tokens = super::query_actor(actor, |reply| {
        ActorCommand::Admin(AdminQuery::MintExecutorTokens {
            intent_ids: req.intent_ids,
            reply,
        })
    })
    .await?;
    Ok(MintExecutorTokensResponse { tokens })
}
