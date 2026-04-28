//! First-Fit-Decreasing bin-packing simulation.
//!
//! Per `r[ctrl.nodeclaim.ffd-sim]`: sort intents by `(eta=0, c*)`
//! descending, bin-select MostAllocated on `allocatable` divisor — the
//! same scoring `kube-scheduler-packed` (B2) uses, so the simulation's
//! `placeable` set predicts what the real scheduler will do.
//!
// TODO(B7): full implementation. This file currently provides only the
// `LiveNode` view (so `list_live_nodeclaims` and the consolidator have a
// concrete type) and a no-op `simulate` that places nothing.

use rio_crds::karpenter::{NodeClaim, NodeClaimStatus};
use rio_proto::types::SpawnIntent;

use super::sketch::CellSketches;

/// View of one owned NodeClaim for FFD + consolidation. Built from the
/// typed `NodeClaim` (B4) so condition/allocatable parsing lives here.
#[derive(Debug, Clone)]
pub struct LiveNode {
    /// `metadata.name` — the delete key.
    pub name: String,
    /// Backing `Node` name once `Registered=True`; `None` in-flight.
    pub node_name: Option<String>,
    /// `status.conditions[type=Registered].status == "True"`. FFD treats
    /// in-flight (`!registered`) claims as projected capacity (their
    /// `status.capacity`, populated at Launch); Registered claims use
    /// `Node.status.allocatable`.
    pub registered: bool,
    /// Full `status` for B7/B10's allocatable/capacity reads.
    pub status: NodeClaimStatus,
}

impl From<NodeClaim> for LiveNode {
    fn from(nc: NodeClaim) -> Self {
        let status = nc.status.unwrap_or_default();
        let registered = status
            .conditions
            .iter()
            .any(|c| c.type_ == "Registered" && c.status == "True");
        Self {
            name: nc.metadata.name.unwrap_or_default(),
            node_name: status.node_name.clone(),
            registered,
            status,
        }
    }
}

/// `(intent, target_nodeclaim_name, in_flight)` for placeable intents.
/// `in_flight = !registered` so the consolidator can distinguish
/// "reserved on a live node" from "reserved on a node that hasn't
/// landed yet".
pub type Placement = (SpawnIntent, String, bool);

/// FFD-simulate placing `intents` onto `live`. Returns
/// `(placeable, unplaced)`.
// TODO(B7): real MostAllocated bin-select. Skeleton: nothing places,
// every intent is unplaced — `cover_deficit` (B8) sees the full demand.
pub fn simulate(
    intents: &[SpawnIntent],
    live: &[LiveNode],
    sketches: &CellSketches,
) -> (Vec<Placement>, Vec<SpawnIntent>) {
    let _ = (live, sketches);
    (Vec::new(), intents.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn nc(name: &str, registered: bool) -> NodeClaim {
        // `Condition` has non-Default `last_transition_time` (Time);
        // build via JSON so the test stays decoupled from k8s-openapi's
        // jiff/chrono choice.
        let status: NodeClaimStatus = serde_json::from_value(serde_json::json!({
            "conditions": [{
                "type": "Registered",
                "status": if registered { "True" } else { "False" },
                "lastTransitionTime": "2026-01-01T00:00:00Z",
                "reason": "", "message": "",
            }],
            "nodeName": registered.then(|| format!("node-{name}")),
        }))
        .unwrap();
        NodeClaim {
            metadata: kube::api::ObjectMeta {
                name: Some(name.into()),
                ..Default::default()
            },
            spec: Default::default(),
            status: Some(status),
        }
    }

    #[test]
    fn live_node_from_nodeclaim_reads_registered() {
        let live: LiveNode = nc("a", true).into();
        assert_eq!(live.name, "a");
        assert!(live.registered);
        assert_eq!(live.node_name.as_deref(), Some("node-a"));

        let inflight: LiveNode = nc("b", false).into();
        assert!(!inflight.registered);
        assert!(inflight.node_name.is_none());
    }

    #[test]
    fn live_node_from_statusless_nodeclaim() {
        let nc = NodeClaim {
            metadata: kube::api::ObjectMeta {
                name: Some("fresh".into()),
                ..Default::default()
            },
            spec: Default::default(),
            status: None,
        };
        let live: LiveNode = nc.into();
        assert!(!live.registered, "no status → not registered");
    }

    #[test]
    fn simulate_skeleton_places_nothing() {
        let intents = vec![SpawnIntent::default(), SpawnIntent::default()];
        let (p, u) = simulate(&intents, &[], &CellSketches::default());
        assert!(p.is_empty());
        assert_eq!(u.len(), 2);
    }
}
