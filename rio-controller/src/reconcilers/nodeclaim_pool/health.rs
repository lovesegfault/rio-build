//! Unhealthy-node reaping + ICE detection.
//!
//! Three reap paths:
//!
//! - **ICE** (`Launched=False` past `ice_timeout`): EC2
//!   InsufficientInstanceCapacity. Delete the claim AND mark the cell
//!   unfulfillable so this tick's `cover_deficit` and the scheduler's
//!   `solve_intent_for` both route around it.
//! - **Boot failure** (`Launched=True ∧ Registered=False` past
//!   `ice_timeout`): instance came up but kubelet never registered
//!   (AMI/network/nodeadm failure). Delete; the cell isn't ICE-masked
//!   (capacity exists, the boot failed).
//! - **Dead** (scheduler-reported `dead_nodes`): the §13b hung-node
//!   detector — ≥max(3,⌈0.5·occ⌉) stale-heartbeat executors across ≥2
//!   tenants on one Node. Delete the NodeClaim (Karpenter handles
//!   cordon+drain via finalizer).
//!
//! Dead reaping is capped at `min(3, ⌈5%·|live|⌉)` per tick — a false-
//! positive scheduler signal can't drain the fleet in one tick.

use std::collections::{HashMap, HashSet};

use kube::Api;
use kube::api::DeleteParams;
use rio_crds::karpenter::NodeClaim;
use tracing::{debug, warn};

use super::NodeClaimPoolConfig;
use super::ffd::LiveNode;
use super::sketch::{Cell, CellSketches};

/// `Launched=False` `reason` values Karpenter posts before GCing a
/// claim it can't fulfil. Distinct from a slow-but-progressing launch
/// (e.g. `Pending`/`AwaitingReconciliation`): on these, Karpenter
/// deletes ~1s later, so the `age > timeout` gate never fires —
/// `classify` short-circuits to `Ice` on observing the reason.
const LAUNCH_FAILED_REASONS: &[&str] = &[
    "LaunchFailed",
    "InsufficientCapacity",
    "InsufficientCapacityError",
    "NodeClassNotReady",
];

/// Per-tick dead-node reap cap: `min(3, ⌈5%·|live|⌉)`. ICE/boot-timeout
/// reaps are NOT capped — those NodeClaims have no backing capacity.
fn dead_reap_cap(live: usize) -> usize {
    3.min((0.05 * live as f64).ceil() as usize).max(1)
}

/// Why a NodeClaim is being reaped. `as_str` is the
/// `rio_controller_nodeclaim_reaped_total{reason=...}` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapReason {
    /// `Launched=False` past `ice_timeout` — EC2 ICE. Cell masked.
    Ice,
    /// `Launched=True ∧ Registered=False` past `ice_timeout`.
    BootTimeout,
    /// Scheduler-reported hung node.
    Dead,
}

impl ReapReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ice => "ice",
            Self::BootTimeout => "boot-timeout",
            Self::Dead => "dead",
        }
    }
}

/// Classify each `live` NodeClaim's health. `Some(reason)` ⇒ reapable;
/// `None` ⇒ healthy / still in-flight within timeout. Pure (no kube
/// side-effects) so the policy is unit-testable.
pub fn classify(
    live: &[LiveNode],
    dead_nodes: &HashSet<&str>,
    sketches: &CellSketches,
    cfg: &NodeClaimPoolConfig,
    now_secs: f64,
) -> Vec<(usize, ReapReason)> {
    let mut out = Vec::new();
    for (i, n) in live.iter().enumerate() {
        let Some(cell) = n.cell.as_ref() else {
            continue;
        };
        // Scheduler dead-node signal: keyed on the backing Node name
        // (what executors heartbeat against), not the NodeClaim name.
        if n.registered
            && n.node_name
                .as_deref()
                .is_some_and(|nn| dead_nodes.contains(nn))
        {
            out.push((i, ReapReason::Dead));
            continue;
        }
        if n.registered {
            continue;
        }
        // Terminal launch-failure reason → ICE NOW (no timeout wait).
        // Karpenter GCs the claim ~1s after posting these; the age
        // gate below (~2×seed ≈ 30-90s) would never observe it. The
        // reason check is the only window where the claim is still in
        // `live` with the failure visible.
        if let Some(("False", _)) = n.cond("Launched")
            && n.cond_reason("Launched")
                .is_some_and(|r| LAUNCH_FAILED_REASONS.contains(&r))
        {
            out.push((i, ReapReason::Ice));
            continue;
        }
        // In-flight: check ICE / boot-timeout.
        let Some(age) = n.age_secs(now_secs) else {
            continue;
        };
        let timeout = sketches.get(cell).map_or(2.0 * cfg.seed_for(cell), |s| {
            s.ice_timeout(cfg.seed_for(cell))
        });
        if age <= timeout {
            continue;
        }
        match n.cond("Launched") {
            // Launched=False past timeout (Karpenter writes status=False
            // with reason=InsufficientCapacity), OR no Launched
            // condition at all past timeout (Karpenter never picked it
            // up — also capacity-side).
            Some(("False", _)) | None => out.push((i, ReapReason::Ice)),
            // Launched=True but never Registered → boot/AMI failure.
            Some(("True", _)) => out.push((i, ReapReason::BootTimeout)),
            Some(_) => {}
        }
    }
    out
}

/// ICE detection for claims Karpenter GC'd between ticks. `inflight`
/// holds `(name, cell)` for everything `cover_deficit` created and
/// hasn't yet observed Registered/reaped. Any name absent from `live`
/// was deleted by Karpenter (the controller never deletes its own
/// in-flight claims) ⇒ the cell is unfulfillable. Names PRESENT in
/// `live` are pruned (still in flight or registered — `classify` /
/// `observe_registered` handle them).
///
/// This is the structural fix for the live B11 finding: Karpenter's
/// `Launched=False reason=LaunchFailed` → GC happens in ~1s, faster
/// than the 10s tick, so neither `classify`'s timeout path NOR its
/// reason short-circuit ever sees the claim. Tick-over-tick absence
/// detection is the only signal that survives.
pub fn detect_vanished(inflight: &mut HashMap<String, Cell>, live: &[LiveNode]) -> Vec<Cell> {
    let live_names: HashSet<&str> = live.iter().map(|n| n.name.as_str()).collect();
    let mut ice = Vec::new();
    inflight.retain(|name, cell| {
        if live_names.contains(name.as_str()) {
            // Still in flight (or Registered) — drop from tracking;
            // `classify` / `observe_registered` own it now.
            false
        } else {
            warn!(%name, %cell, "in-flight NodeClaim vanished (Karpenter GC); ICE-masking cell");
            metrics::counter!(
                "rio_controller_nodeclaim_reaped_total",
                "reason" => "vanished",
                "cell" => cell.to_string(),
            )
            .increment(1);
            ice.push(cell.clone());
            false
        }
    });
    ice
}

/// Reap unhealthy/ICE-stuck NodeClaims. Returns the set of cells hit
/// by ICE this tick (fed to `report_unfulfillable` →
/// `AckSpawnedIntents.unfulfillable_cells`). `Api::delete` 404 is
/// ignored; other errors warn + skip (next tick retries).
pub async fn reap_unhealthy(
    nodeclaims: &Api<NodeClaim>,
    live: &[LiveNode],
    dead_nodes: &[String],
    sketches: &CellSketches,
    cfg: &NodeClaimPoolConfig,
    now_secs: f64,
) -> anyhow::Result<Vec<Cell>> {
    let dead: HashSet<&str> = dead_nodes.iter().map(String::as_str).collect();
    let to_reap = classify(live, &dead, sketches, cfg, now_secs);
    let cap = dead_reap_cap(live.len());
    let mut dead_reaped = 0usize;
    let mut ice_cells = Vec::new();
    for (i, reason) in to_reap {
        let n = &live[i];
        if reason == ReapReason::Dead {
            if dead_reaped >= cap {
                continue;
            }
            dead_reaped += 1;
        }
        let cell = n.cell.clone().expect("classify filtered cell-less");
        match nodeclaims.delete(&n.name, &DeleteParams::default()).await {
            Ok(_) => {
                debug!(name = %n.name, %cell, reason = reason.as_str(), "reaped unhealthy NodeClaim");
                metrics::counter!(
                    "rio_controller_nodeclaim_reaped_total",
                    "reason" => reason.as_str(),
                    "cell" => cell.to_string(),
                )
                .increment(1);
                if reason == ReapReason::Ice {
                    ice_cells.push(cell);
                }
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {}
            Err(e) => {
                warn!(name = %n.name, error = %e, "unhealthy NodeClaim delete failed; skipping");
            }
        }
    }
    Ok(ice_cells)
}

#[cfg(test)]
mod tests {
    use super::super::ffd::tests::{node, with_conds, with_conds_reason};
    use super::super::sketch::CapacityType;
    use super::*;

    fn cfg_seeded(h: &str, seed: f64) -> NodeClaimPoolConfig {
        NodeClaimPoolConfig {
            lead_time_seed: [(format!("{h}:spot"), seed)].into(),
            ..Default::default()
        }
    }

    /// `Launched=False reason=LaunchFailed` → ICE immediately, even
    /// well under timeout. Karpenter GCs the claim ~1s after posting
    /// this; the timeout-based path never observes it. Live B11
    /// finding: 0 instance types matched `rio.build/*` requirements →
    /// `InsufficientCapacityError` → GC'd ~1s later → controller
    /// re-created every tick.
    #[test]
    fn ice_immediate_on_launch_failed_reason() {
        let cfg = cfg_seeded("h", 45.0);
        let sk = CellSketches::default();
        let mut ice = with_conds_reason(
            node("ice", "h", CapacityType::Spot, 8, 0, 0),
            &[("Launched", "False", 1001.0, "LaunchFailed")],
        );
        ice.registered = false;
        // age=2s ≪ 2×45=90s timeout → reason short-circuit fires anyway.
        let r = classify(&[ice], &HashSet::new(), &sk, &cfg, 1002.0);
        assert_eq!(r, vec![(0, ReapReason::Ice)]);
        // Non-terminal reason (e.g. empty / Pending) at age=2s → healthy
        // (still in timeout window).
        let mut pending = with_conds_reason(
            node("p", "h", CapacityType::Spot, 8, 0, 0),
            &[("Launched", "False", 1001.0, "Pending")],
        );
        pending.registered = false;
        assert!(classify(&[pending], &HashSet::new(), &sk, &cfg, 1002.0).is_empty());
    }

    /// `detect_vanished`: in-flight claim absent from `live` → cell
    /// ICE'd + entry removed. Present → entry removed (handed off to
    /// classify), no ICE.
    #[test]
    fn detect_vanished_masks_gcd_claims() {
        let h = Cell("h".into(), CapacityType::Spot);
        let mut inflight: HashMap<String, Cell> =
            [("nc-gone".into(), h.clone()), ("nc-live".into(), h.clone())].into();
        let live = [node("nc-live", "h", CapacityType::Spot, 8, 0, 0)];
        let ice = detect_vanished(&mut inflight, &live);
        assert_eq!(ice, vec![h]);
        assert!(inflight.is_empty(), "both entries pruned");
        // Second call: nothing tracked → no ICE (idempotent).
        assert!(detect_vanished(&mut inflight, &live).is_empty());
    }

    /// `Launched=False` past `2×seed` → ICE. Under timeout → healthy.
    #[test]
    fn ice_on_launched_false_past_timeout() {
        let cfg = cfg_seeded("h", 45.0);
        let sk = CellSketches::default();
        // created=1000, now=1100 → age=100 > 2×45=90. Launched=False.
        let mut ice = with_conds(
            node("ice", "h", CapacityType::Spot, 8, 0, 0),
            &[("Launched", "False", 1005.0)],
        );
        ice.registered = false;
        let r = classify(&[ice.clone()], &HashSet::new(), &sk, &cfg, 1100.0);
        assert_eq!(r, vec![(0, ReapReason::Ice)]);
        // Under timeout: now=1080 → age=80 < 90 → healthy.
        let r2 = classify(&[ice], &HashSet::new(), &sk, &cfg, 1080.0);
        assert!(r2.is_empty());
    }

    /// `Launched=True ∧ Registered=False` past timeout → BootTimeout
    /// (not ICE — capacity exists).
    #[test]
    fn boot_timeout_on_launched_true_unregistered() {
        let cfg = cfg_seeded("h", 45.0);
        let sk = CellSketches::default();
        let mut bt = with_conds(
            node("bt", "h", CapacityType::Spot, 8, 0, 0),
            &[
                ("Launched", "True", 1010.0),
                ("Registered", "False", 1010.0),
            ],
        );
        bt.registered = false;
        let r = classify(&[bt], &HashSet::new(), &sk, &cfg, 1100.0);
        assert_eq!(r, vec![(0, ReapReason::BootTimeout)]);
    }

    /// No Launched condition at all past timeout → ICE (Karpenter
    /// never picked the claim up).
    #[test]
    fn ice_on_no_launched_condition() {
        let cfg = cfg_seeded("h", 45.0);
        let sk = CellSketches::default();
        let mut stuck = node("stuck", "h", CapacityType::Spot, 8, 0, 0);
        stuck.registered = false;
        let r = classify(&[stuck], &HashSet::new(), &sk, &cfg, 1100.0);
        assert_eq!(r, vec![(0, ReapReason::Ice)]);
    }

    /// `n_real < 100` → timeout = `2×seed`, not q_0.99(boot). With
    /// 50 boot samples at 30s and seed=45s, timeout stays 90s.
    #[test]
    fn ice_timeout_uses_seed_floor_below_100_real() {
        let cfg = cfg_seeded("h", 45.0);
        let mut sk = CellSketches::default();
        let cell = Cell("h".into(), CapacityType::Spot);
        for _ in 0..50 {
            sk.cell_mut(&cell).record(30.0, 0.0);
        }
        let mut n = with_conds(
            node("n", "h", CapacityType::Spot, 8, 0, 0),
            &[("Launched", "False", 1005.0)],
        );
        n.registered = false;
        // age=85 < 2×45=90 → healthy (NOT q_0.99(boot)≈30 → would
        // have fired at age>30 if seed-floor weren't applied).
        let r = classify(&[n.clone()], &HashSet::new(), &sk, &cfg, 1085.0);
        assert!(r.is_empty(), "seed floor holds at n=50");
        // age=95 > 90 → ICE.
        let r2 = classify(&[n], &HashSet::new(), &sk, &cfg, 1095.0);
        assert_eq!(r2, vec![(0, ReapReason::Ice)]);
    }

    /// Dead-node reap keyed on backing `node_name`, capped at
    /// `min(3, ⌈5%⌉)`.
    #[test]
    fn dead_nodes_reaped_with_cap() {
        let cfg = cfg_seeded("h", 45.0);
        let sk = CellSketches::default();
        // 10 registered nodes; scheduler reports 5 dead by node_name.
        let live: Vec<_> = (0..10)
            .map(|k| {
                with_conds(
                    node(&format!("nc{k}"), "h", CapacityType::Spot, 8, 0, 0),
                    &[("Registered", "True", 1042.0)],
                )
            })
            .collect();
        let dead: HashSet<&str> =
            ["node-nc0", "node-nc1", "node-nc2", "node-nc3", "node-nc4"].into();
        let r = classify(&live, &dead, &sk, &cfg, 1100.0);
        // All 5 classified Dead; cap applied at delete-time, not here.
        assert_eq!(r.len(), 5);
        assert!(r.iter().all(|(_, reason)| *reason == ReapReason::Dead));
        // Cap: 10 live → min(3, ⌈0.5⌉)=min(3,1)=1.
        assert_eq!(dead_reap_cap(10), 1);
        // 100 live → min(3, ⌈5⌉)=3.
        assert_eq!(dead_reap_cap(100), 3);
        assert_eq!(dead_reap_cap(40), 2);
        // 0 live → 1 floor (avoids 0-cap on empty fleet edge).
        assert_eq!(dead_reap_cap(0), 1);
    }

    /// Registered nodes are never ICE/BootTimeout (they made it).
    /// Cell-less nodes skipped entirely.
    #[test]
    fn registered_and_cellless_skipped() {
        let cfg = cfg_seeded("h", 45.0);
        let sk = CellSketches::default();
        let reg = with_conds(
            node("ok", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", 1042.0)],
        );
        let mut cellless = node("cl", "h", CapacityType::Spot, 8, 0, 0);
        cellless.cell = None;
        cellless.registered = false;
        let r = classify(&[reg, cellless], &HashSet::new(), &sk, &cfg, 1200.0);
        assert!(r.is_empty());
    }

    /// ICE cells propagate: classify→Ice ⇒ cell ends up in the
    /// returned `ice_cells` (asserted on the pure path; kube delete
    /// covered in VM tests).
    #[test]
    fn masked_cell_propagation() {
        let cfg = cfg_seeded("h", 45.0);
        let sk = CellSketches::default();
        let mut a = with_conds(
            node("a", "h", CapacityType::Spot, 8, 0, 0),
            &[("Launched", "False", 1005.0)],
        );
        a.registered = false;
        let mut b = with_conds(
            node("b", "h", CapacityType::Spot, 8, 0, 0),
            &[("Launched", "True", 1010.0)],
        );
        b.registered = false;
        let r = classify(&[a, b], &HashSet::new(), &sk, &cfg, 1100.0);
        let ice_cells: Vec<_> = r
            .iter()
            .filter(|(_, reason)| *reason == ReapReason::Ice)
            .map(|(i, _)| i)
            .collect();
        // Only `a` (Launched=False) is ICE; `b` is BootTimeout.
        assert_eq!(ice_cells, vec![&0]);
        assert_eq!(r[1].1, ReapReason::BootTimeout);
    }
}
