//! Unhealthy-node reaping + ICE detection.
//!
//! Reaps NodeClaims whose backing Node is in the scheduler's
//! `dead_nodes` list (≥max(3,⌈0.5·occupancy⌉) stale-heartbeat executors
//! across ≥2 tenants), and NodeClaims stuck `Launched=False` past the
//! cell's `ice_timeout`.
//!
// TODO(B11): full implementation.

use kube::Api;
use rio_crds::karpenter::NodeClaim;

use super::ffd::LiveNode;
use super::sketch::CellSketches;

/// Reap unhealthy/ICE-stuck NodeClaims and record ICE events into
/// `sketches`.
// TODO(B11): cordon+evict on dead_nodes; ICE-timeout on Launched=False.
pub async fn reap_unhealthy(
    nodeclaims: &Api<NodeClaim>,
    live: &[LiveNode],
    dead_nodes: &[String],
    sketches: &mut CellSketches,
) -> anyhow::Result<()> {
    let _ = (nodeclaims, live, dead_nodes, sketches);
    Ok(())
}
