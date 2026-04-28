//! Nelson-Aalen idle-node consolidation.
//!
//! Per `r[ctrl.nodeclaim.consolidate-na]`: an empty Registered NodeClaim
//! is kept while `λ(t)·𝔼[c_arr·𝟙{≤cores}] > cores/q_0.5(boot)`; λ via
//! Nelson-Aalen on right-censored `idle_gap` events.
//!
// TODO(B10): full implementation. Skeleton provides the `IdleGapEvent`
// type (persisted as jsonb in migration 059) and a no-op `reap_idle`.

use kube::Api;
use rio_crds::karpenter::NodeClaim;
use serde::{Deserialize, Serialize};

use super::NodeClaimPoolConfig;
use super::ffd::{LiveNode, Placement};
use super::sketch::CellSketches;

/// One observed gap between a node going idle and the next intent
/// arriving (or the node being reaped — `censored=true`). Persisted as
/// jsonb (`nodeclaim_cell_state.idle_gap_events`); shape changes need
/// no migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdleGapEvent {
    /// Seconds the node was idle.
    pub gap_secs: f64,
    /// `true` if the node was reaped before the next arrival
    /// (right-censored observation).
    pub censored: bool,
}

/// Reap idle Registered NodeClaims past their break-even threshold.
// TODO(B10): `na_hazard` + `consolidate_after` + hold-open ε.
pub async fn reap_idle(
    nodeclaims: &Api<NodeClaim>,
    live: &[LiveNode],
    placeable: &[Placement],
    sketches: &CellSketches,
    cfg: &NodeClaimPoolConfig,
) -> anyhow::Result<()> {
    let _ = (nodeclaims, live, placeable, sketches, cfg);
    Ok(())
}
