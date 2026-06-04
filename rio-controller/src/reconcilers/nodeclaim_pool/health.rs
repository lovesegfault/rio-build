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
//! - **Dead** (the OA2 wedge clustering): per-node clustering of
//!   pull-mode attempt-deadline expiries over the open-attempt ledger
//!   view (`wedge::WedgeTracker` — the successor of the stream-era
//!   heartbeat-fed hung-node detector). Delete the NodeClaim
//!   (Karpenter handles cordon+drain via finalizer).
//!
//! Dead reaping is capped at `min(3, ⌈5%·|registered|⌉)` per tick
//! (registered ∧ ¬terminating, the only population `classify` can emit
//! `Dead` for) — a false-positive wedge signal can't drain the
//! fleet in one tick.

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

/// Per-tick dead-node reap cap: `min(3, ⌈5%·N⌉)`. ICE/boot-timeout
/// reaps are NOT capped — those NodeClaims have no backing capacity.
///
/// `N` is the **registered, non-terminating** population — the only
/// claims `classify` can ever emit `ReapReason::Dead` for. Counting
/// in-flight or already-terminating claims would inflate the cap during
/// scale-up bursts (5 registered + 60 in-flight → cap=3 → 60% of the
/// registered fleet reapable in one tick, contradicting the module-doc
/// "false-positive scheduler signal can't drain the fleet" claim).
fn dead_reap_cap(registered: usize) -> usize {
    3.min((0.05 * registered as f64).ceil() as usize).max(1)
}

/// Record a successfully-reaped NodeClaim's consequences: increment
/// `rio_controller_nodeclaim_reaped_total`, mask the cell if ICE, and
/// queue the name for `inflight_created` cleanup.
///
/// Called from BOTH the `Ok(_)` and `Err(404)` arms of the
/// `delete()` match in [`reap_unhealthy`] — a 404 means Karpenter GC
/// raced the controller's delete, but the claim *was* reaped and the
/// cell is just as unfulfillable as if the controller had won the race.
/// Diverging the arms (the original bug) leaves the cell unmasked for
/// the rest of the tick → `cover_deficit` re-mints into it →
/// `report_unfulfillable` never marks `IceBackoff` → the
/// `RioNodeclaimPoolIceMaskedHigh` alert undercounts.
fn record_reap(
    reason: ReapReason,
    cell: Cell,
    name: &str,
    ice_cells: &mut Vec<Cell>,
    reaped_names: &mut Vec<String>,
) {
    metrics::counter!(
        "rio_controller_nodeclaim_reaped_total",
        "reason" => reason.as_str(),
        "cell" => cell.to_string(),
    )
    .increment(1);
    if reason == ReapReason::Ice {
        ice_cells.push(cell);
    }
    reaped_names.push(name.to_string());
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
        // Already terminating: a redundant `delete` is accepted by the
        // apiserver (not 404 — the object still exists), so it'd
        // double-increment `reaped_total` and re-ICE-mask the cell.
        if n.terminating() {
            continue;
        }
        // Dead-node signal (OA2 wedge clustering): keyed on the
        // backing Node name (the attempt rows' source attribution),
        // not the NodeClaim name.
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
/// hasn't yet observed Registered, terminating, or absent.
///
/// Drop rules (all `→ false` removes the entry):
/// - **Registered**: `observe_registered`/FFD own it now.
/// - **Terminating**: the controller (or Karpenter expiration) is
///   tearing it down deliberately; not an ICE signal.
/// - **Absent from `live`**: vanished without ever Registering.
///   Karpenter GC'd it (the controller's own reaps are removed from
///   `inflight` by the caller before this runs) ⇒ the cell is
///   unfulfillable. ICE-mask + `reaped_total{reason=vanished}`.
/// - **In-flight (present, not Registered, not terminating)**: KEEP.
///   r40 bug_020: dropping on first sighting let a claim observed at
///   age ~10s and GC'd at ~13–16s escape every detection path —
///   `detect_vanished` (already pruned), `classify`'s reason
///   short-circuit (claim no longer in `live`), `classify`'s
///   `age > ice_timeout` (`2×seed ≈ 1200s` for metal cells; the claim
///   is gone long before then). The cell churns unmasked NodeClaims
///   for many cycles, burns EC2 API quota, never feeds the ICE alert.
///
/// This is the structural fix for the live B11 finding: Karpenter's
/// `Launched=False reason=LaunchFailed` → GC happens in ~1s, faster
/// than the 10s tick, so neither `classify`'s timeout path NOR its
/// reason short-circuit ever sees the claim. Tick-over-tick absence
/// detection is the only signal that survives.
///
/// No TTL on retained entries. The old drop-on-first-sighting bounded
/// `inflight` to one tick's creates implicitly; the KEEP arm removes
/// that cap. Still bounded: the `None` arm prunes every entry absent
/// from `live` each tick, so after a `detect_vanished` call the map is
/// a subset of `live` (only `cover_deficit`'s post-call `extend` adds
/// not-yet-listed names). The map can't outgrow the cluster's
/// stuck-Pending population plus one tick of creates. Every retain path
/// is also finite for a *progressing* claim: it Registers
/// (`n.registered` arm), terminates (`n.terminating()` arm), is reaped
/// by `classify`'s `BootTimeout`/`Ice` and removed by the caller's
/// `reap_unhealthy` name drain, or is GC'd by Karpenter (the `None`
/// arm). Two `classify` escapes can park an entry indefinitely:
/// (a) `creationTimestamp` absent — `age_secs` returns `None` and
/// `classify` `continue`s before the timeout gate; the apiserver sets
/// it on every persisted object, near-impossible; (b) `Launched`
/// present with status ∉ `{True, False}` past timeout — `classify`'s
/// final match has no `Some(("Unknown", _))` arm. (b) is plausible:
/// Karpenter's `InitializeConditions` writes `Launched=Unknown` before
/// the launch attempt, so a Karpenter outage/backlog parks claims
/// there. Either way the claim is itself stuck in the cluster —
/// already operator-visible via `nodeclaim_inflight_age_max_seconds` —
/// and the entry frees the moment the claim resolves. If r41 finds the
/// bound insufficient, add the TTL the bug_020 report recommends
/// (`now - created_at > 2×ice_timeout`; needs an insertion timestamp in
/// the map value) AND a `rio_controller_nodeclaim_inflight_tracked`
/// gauge in `emit_live_gauges` so the leak is observable.
pub fn detect_vanished(inflight: &mut HashMap<String, Cell>, live: &[LiveNode]) -> Vec<Cell> {
    let live_by_name: HashMap<&str, &LiveNode> =
        live.iter().map(|n| (n.name.as_str(), n)).collect();
    let mut ice = Vec::new();
    inflight.retain(|name, cell| match live_by_name.get(name.as_str()) {
        // Registered or terminating → done tracking; `classify` /
        // `observe_registered` own it.
        Some(n) if n.registered || n.terminating() => false,
        // Still in-flight: KEEP. classify's reason short-circuit and
        // ice_timeout don't cover the GC'd-between-observations
        // window for slow-ICE cells.
        Some(_) => true,
        // Vanished without ever Registering → ICE.
        None => {
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

/// One `reap_unhealthy` tick's outcome.
///
/// - `ice_cells`: cells hit by ICE this tick (fed to
///   `report_unfulfillable` → `AckSpawnedIntents.unfulfillable_cells`).
/// - `reaped_claims`: the NodeClaim `metadata.name`s the controller
///   `delete()`d (dropped from `inflight_created` — the controller's
///   own reaps, not Karpenter GC, must NOT feed `detect_vanished`'s
///   ICE path next tick).
/// - `reaped_nodes`: the backing `Node` names of those claims (where
///   known) — the wedge tracker's REQUIRED eviction feed
///   (merged_bug_009): a reaped node's expiry evidence is dead and
///   must not re-feed the Dead arm or inflate the systemic
///   populations.
///
/// `Api::delete` 404 is ignored; other errors warn + skip (next tick
/// retries).
#[derive(Debug, Default)]
pub struct ReapOutcome {
    pub ice_cells: Vec<Cell>,
    pub reaped_claims: Vec<String>,
    pub reaped_nodes: Vec<String>,
}

pub async fn reap_unhealthy(
    nodeclaims: &Api<NodeClaim>,
    live: &[LiveNode],
    dead_nodes: &[String],
    sketches: &CellSketches,
    cfg: &NodeClaimPoolConfig,
    now_secs: f64,
) -> anyhow::Result<ReapOutcome> {
    let dead: HashSet<&str> = dead_nodes.iter().map(String::as_str).collect();
    let to_reap = classify(live, &dead, sketches, cfg, now_secs);
    // Cap the dead-reap rate against the population it can actually reap
    // from — `classify` only emits `ReapReason::Dead` for
    // `registered && !terminating()`. See `dead_reap_cap` doc.
    let registered_count = live
        .iter()
        .filter(|n| n.registered && !n.terminating())
        .count();
    let cap = dead_reap_cap(registered_count);
    let mut dead_reaped = 0usize;
    let mut out = ReapOutcome::default();
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
                record_reap(
                    reason,
                    cell,
                    &n.name,
                    &mut out.ice_cells,
                    &mut out.reaped_claims,
                );
                out.reaped_nodes.extend(n.node_name.clone());
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                // Already gone (Karpenter GC raced us). Apply the FULL
                // `Ok(_)` consequence — the claim *was* reaped, just not
                // by us. See `record_reap` doc.
                debug!(name = %n.name, %cell, reason = reason.as_str(), "unhealthy NodeClaim already gone (GC raced); recorded reap");
                record_reap(
                    reason,
                    cell,
                    &n.name,
                    &mut out.ice_cells,
                    &mut out.reaped_claims,
                );
                out.reaped_nodes.extend(n.node_name.clone());
            }
            Err(e) => {
                warn!(name = %n.name, error = %e, "unhealthy NodeClaim delete failed; skipping");
            }
        }
    }
    Ok(out)
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

    /// bug_040: a cell absent from `lead_time_seed` (a new hw-class
    /// added without re-running `xtask k8s probe-boot`) gets
    /// `default_lead_time_seed` (30s) → timeout `2×30=60s`, NOT
    /// `2×0=0s`. With seed=0 a NodeClaim at age=10s would be reaped
    /// before ~18s real boot completes; the cell could never accrue a
    /// sample to escape the floor.
    #[test]
    fn unseeded_cell_timeout_nonzero() {
        let cfg = NodeClaimPoolConfig {
            lead_time_seed: HashMap::new(),
            default_lead_time_seed: 30.0,
            ..Default::default()
        };
        let sk = CellSketches::default();
        let mut n = with_conds(
            node("nc", "new-h", CapacityType::Spot, 8, 0, 0),
            &[("Launched", "True", 1001.0)],
        );
        n.registered = false;
        // age=10s; with seed=30 → timeout=60s → healthy. With the OLD
        // seed=0 default → timeout=0 → reaped as BootTimeout here.
        let r = classify(&[n], &HashSet::new(), &sk, &cfg, 1010.0);
        assert!(r.is_empty(), "unseeded cell at age<2×default_seed: {r:?}");
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
    /// ICE'd + entry removed. r40 bug_020: in-flight claim PRESENT in
    /// `live` is KEPT (not dropped on first sighting); registered or
    /// terminating claims are dropped (handed off to `classify` /
    /// `observe_registered`).
    #[test]
    fn detect_vanished_masks_gcd_claims() {
        use super::super::ffd::tests::set_terminating;
        let h = Cell("h".into(), CapacityType::Spot);
        let mut inflight: HashMap<String, Cell> = [
            ("nc-gone".into(), h.clone()),
            ("nc-inflight".into(), h.clone()),
            ("nc-reg".into(), h.clone()),
            ("nc-term".into(), h.clone()),
        ]
        .into();
        let mut reg = node("nc-reg", "h", CapacityType::Spot, 8, 0, 0);
        reg.registered = true;
        // node() defaults registered=true; force unregistered so the test
        // pins the n.terminating() arm of the retain `||` independently —
        // otherwise n.registered short-circuits and a regression that drops
        // the terminating() clause stays green.
        let mut term = node("nc-term", "h", CapacityType::Spot, 8, 0, 0);
        term.registered = false;
        let term = set_terminating(term);
        // node() defaults registered=true; force in-flight.
        let mut inflight_node = node("nc-inflight", "h", CapacityType::Spot, 8, 0, 0);
        inflight_node.registered = false;
        let live = [inflight_node, reg, term];

        let ice = detect_vanished(&mut inflight, &live);
        assert_eq!(ice, vec![h], "only nc-gone (absent) ICE-masked");
        assert_eq!(
            inflight.keys().collect::<Vec<_>>(),
            vec!["nc-inflight"],
            "in-flight present claim kept; registered/terminating/vanished dropped"
        );
        // Second call (claim still in-flight, still live): nothing new
        // ICE'd, claim stays tracked.
        assert!(detect_vanished(&mut inflight, &live).is_empty());
        assert_eq!(inflight.len(), 1);
        // Third call: claim GC'd between ticks → ICE.
        assert_eq!(
            detect_vanished(&mut inflight, &[]),
            vec![Cell("h".into(), CapacityType::Spot)]
        );
        assert!(inflight.is_empty());
    }

    /// A NodeClaim already terminating (`metadata.deletionTimestamp`
    /// set — Karpenter finalizer draining) is NOT re-classified
    /// regardless of its conditions. Re-deleting an already-terminating
    /// object is accepted by the apiserver (NOT 404 — the object still
    /// exists), so the `Ok` branch in `reap_unhealthy` would
    /// double-increment `reaped_total{reason=ice}` and re-ICE-mask the
    /// cell every tick for the whole ~60-90s drain window.
    #[test]
    fn classify_skips_terminating() {
        use super::super::ffd::tests::set_terminating;
        let cfg = cfg_seeded("h", 45.0);
        let sk = CellSketches::default();
        // Would be ICE (Launched=False past timeout) if not terminating.
        let mut ice = set_terminating(with_conds(
            node("ice", "h", CapacityType::Spot, 8, 0, 0),
            &[("Launched", "False", 1005.0)],
        ));
        ice.registered = false;
        // Would be Dead if not terminating.
        let dead = set_terminating(node("dead", "h", CapacityType::Spot, 8, 0, 0));
        let dead_set: HashSet<&str> = ["node-dead"].into();
        let r = classify(&[ice, dead], &dead_set, &sk, &cfg, 1100.0);
        assert!(
            r.is_empty(),
            "terminating NodeClaims are already being reaped; no re-classify: {r:?}"
        );
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
        // Cap: 10 registered → min(3, ⌈0.5⌉)=min(3,1)=1.
        assert_eq!(dead_reap_cap(10), 1);
        // 100 registered → min(3, ⌈5⌉)=3.
        assert_eq!(dead_reap_cap(100), 3);
        assert_eq!(dead_reap_cap(40), 2);
        // 0 registered → 1 floor (avoids 0-cap on empty fleet edge).
        assert_eq!(dead_reap_cap(0), 1);
    }

    /// `dead_reap_cap` is computed over the **registered, non-terminating**
    /// population, not all owned NodeClaims. During scale-up
    /// (5 registered + 60 in-flight + 2 terminating) the cap must stay
    /// at 1 (`⌈5%·5⌉`), not inflate to 3 (`⌈5%·67⌉` clamped) —
    /// `classify` can only emit `ReapReason::Dead` for the registered
    /// 5, so a cap of 3 would let 60% of the reachable fleet be reaped
    /// in one tick.
    #[test]
    fn dead_reap_cap_filters_to_reapable_population() {
        use super::super::ffd::tests::set_terminating;
        // 5 registered + 60 in-flight + 2 terminating.
        let mut live: Vec<LiveNode> = (0..5)
            .map(|k| node(&format!("reg{k}"), "h", CapacityType::Spot, 8, 0, 0))
            .collect();
        for k in 0..60 {
            let mut n = node(&format!("inf{k}"), "h", CapacityType::Spot, 8, 0, 0);
            n.registered = false;
            live.push(n);
        }
        for k in 0..2 {
            live.push(set_terminating(node(
                &format!("term{k}"),
                "h",
                CapacityType::Spot,
                8,
                0,
                0,
            )));
        }
        // The exact filter expression `reap_unhealthy` feeds `dead_reap_cap`.
        let registered_count = live
            .iter()
            .filter(|n| n.registered && !n.terminating())
            .count();
        assert_eq!(registered_count, 5);
        assert_eq!(
            dead_reap_cap(registered_count),
            1,
            "5 registered → ⌈5%·5⌉=1"
        );
        // The pre-fix denominator (all owned claims) would inflate the
        // cap to 3, allowing 60% of the registered fleet to drain.
        assert_eq!(
            dead_reap_cap(live.len()),
            3,
            "67 owned → ⌈5%·67⌉ clamped to 3"
        );
    }

    /// `record_reap` applies the FULL reap consequence — ICE-mask the
    /// cell when `reason == Ice`, and queue the name. Both the `Ok(_)`
    /// and `Err(404)` arms of `reap_unhealthy` call it; this pins the
    /// shared-arm semantics so the 404 arm can never silently drop the
    /// ICE mask again (the original bug).
    #[test]
    fn record_reap_pushes_ice_mask_and_name() {
        let cell = Cell("h".into(), CapacityType::Spot);
        let mut ice_cells = Vec::new();
        let mut reaped_names = Vec::new();
        // ICE: cell is masked AND name queued.
        record_reap(
            ReapReason::Ice,
            cell.clone(),
            "nc-ice",
            &mut ice_cells,
            &mut reaped_names,
        );
        assert_eq!(ice_cells, vec![cell.clone()]);
        assert_eq!(reaped_names, vec!["nc-ice"]);
        // Non-ICE: name queued but cell NOT masked (capacity exists,
        // boot/heartbeat failed — `cover_deficit` may re-mint there).
        for reason in [ReapReason::BootTimeout, ReapReason::Dead] {
            let mut ice_cells = Vec::new();
            let mut reaped_names = Vec::new();
            record_reap(
                reason,
                cell.clone(),
                "nc-other",
                &mut ice_cells,
                &mut reaped_names,
            );
            assert!(ice_cells.is_empty(), "{reason:?} must not ICE-mask");
            assert_eq!(reaped_names, vec!["nc-other"]);
        }
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
