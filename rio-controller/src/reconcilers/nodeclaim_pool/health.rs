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

use super::ffd::LiveNode;
use super::sketch::{Cell, CellSketches};
use super::{InflightClaim, NodeClaimPoolConfig};

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
/// THE shared consequence chokepoint of the eviction-source law
/// (`ctrl.pool.delete-outcome`): called from the
/// [`DeleteOutcome::OkDeleted`] AND [`DeleteOutcome::Committed404`]
/// arms of BOTH reap lanes ([`reap_unhealthy`] here,
/// `consolidate::reap_idle`) — a 404 means Karpenter GC raced the
/// controller's delete, but the claim *was* reaped and the cell is
/// just as unfulfillable as if the controller had won the race.
/// Diverging the arms (the original bug, re-shipped at reap_idle as
/// bug_112) leaves the cell unmasked for the rest of the tick →
/// `cover_deficit` re-mints into it → `report_unfulfillable` never
/// marks `IceBackoff` → the `RioNodeclaimPoolIceMaskedHigh` alert
/// undercounts — and on the idle lane drops the wedge-eviction feed,
/// the reap counter, and the censored NA sample.
// r[impl ctrl.pool.delete-outcome]
pub(super) fn record_reap(
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
    /// Idle past the consolidation threshold (`consolidate::reap_idle`)
    /// — joined the alphabet when the idle lane was re-typed onto
    /// [`DeleteOutcome`] (bug_112): an ambiguous idle delete carries
    /// the same tombstone obligation as the health reaps, so its
    /// reason is a first-class letter, not a lane-local string.
    Idle,
}

impl ReapReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ice => "ice",
            Self::BootTimeout => "boot-timeout",
            Self::Dead => "dead",
            Self::Idle => "idle",
        }
    }

    /// Every reason exactly once — census iteration domain (the
    /// `WITNESSED_LETTERS` form: pinned by `reap_reason_alphabet_total`
    /// so a new variant cannot ship without joining this set and
    /// taking a row in every reason-indexed census).
    #[cfg(test)]
    pub(super) const LETTERS: [ReapReason; 4] =
        [Self::Ice, Self::BootTimeout, Self::Dead, Self::Idle];
}

/// One `Api::delete` call's outcome at a reap lane, as the
/// eviction-source law sees it — the TYPED TOTAL the law lives on
/// (bug_042 + bug_112): *404 = the full `Ok` consequence (the claim
/// WAS reaped — Karpenter GC raced us); ambiguous non-404 `Err` = a
/// tombstone whose reason's full consequence must eventually fire.*
/// Pre-type, the law existed only as per-site match-arm discipline:
/// each lane re-derived the arms independently and `reap_idle`
/// discharged only the success arm (404 `=> {}`, Err warn-only).
/// [`Self::classify`] is the SOLE translator from a raw kube delete
/// `Result` — a lane matching the raw `Result` instead is the
/// strawman the `delete_lane_census` red plants — and a `match` on
/// the returned value is rustc-exhaustive: no lane can discharge one
/// arm without naming the other two.
// r[impl ctrl.pool.delete-outcome]
#[derive(Debug)]
#[must_use = "every DeleteOutcome arm carries a consequence decision (ctrl.pool.delete-outcome)"]
pub enum DeleteOutcome {
    /// `Ok(_)` — the controller's delete committed. Full consequence
    /// NOW (counter, mask iff ICE, eviction feed, lane-local samples).
    OkDeleted,
    /// 404 — already gone: Karpenter GC raced the delete, but the
    /// claim WAS reaped. The same full consequence as [`Self::OkDeleted`]
    /// (the law's parity arm — diverging them is the original bug).
    Committed404,
    /// Non-404 error — AMBIGUOUS: the apiserver may have committed
    /// the delete before erring. The lane MUST tombstone the attempt
    /// (an [`AmbiguousDelete`] into [`DeleteTombstones`]) so the
    /// reason's full consequence fires when a later observation
    /// confirms the commit — never re-attributed as foreign teardown
    /// evidence (bug_094), never silently dropped (bug_042).
    AmbiguousErr(kube::Error),
}

impl DeleteOutcome {
    /// The sole production translator from `Api::delete`'s result.
    /// Total over the result domain: every kube error shape lands in
    /// exactly one arm, and 404 is the only error that counts as a
    /// completed reap (`delete_outcome_partition_total` walks the
    /// product).
    pub fn classify<T>(r: Result<T, kube::Error>) -> Self {
        match r {
            Ok(_) => Self::OkDeleted,
            Err(kube::Error::Api(ae)) if ae.code == 404 => Self::Committed404,
            Err(e) => Self::AmbiguousErr(e),
        }
    }
}

/// One ambiguous delete attempt, as produced by a reap lane's
/// [`DeleteOutcome::AmbiguousErr`] arm — the REQUIRED product of that
/// arm under the eviction-source law. Carries everything the reason's
/// deferred FULL consequence needs, so confirmation does not depend
/// on the claim still being readable (an absent claim's cell and
/// backing node are unreadable at confirm time — the same argument as
/// the `GcVanish` row's unreadable `Launched` axis):
///
/// - `cell`: the reap counter's label + the ICE mask target (iff
///   `reason == Ice`).
/// - `node_name`: the wedge-eviction feed half (bug_042 — a Dead
///   reap's backing node must leave the wedge populations; `None` for
///   never-registered claims, which have no backing node).
/// - `idle_gap_secs`: `reap_idle`'s censored NA sample at delete time
///   (`Some` iff `reason == Idle`).
#[derive(Debug, Clone, PartialEq)]
pub struct AmbiguousDelete {
    pub name: String,
    pub reason: ReapReason,
    pub cell: Cell,
    pub node_name: Option<String>,
    pub idle_gap_secs: Option<f64>,
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
            // Launched=Unknown past timeout — Karpenter throttle/
            // backlog (sh-043 closes the (b) escape at :375). Masks
            // at IceBackoff step 0 (60s); same-tick reaps ship as
            // ONE per-cell mark (`buffer_marks`) so do NOT climb
            // (rio-scheduler `sla/cost.rs` `next_mark_step`
            // in-window=refresh); clears on first `registered_cells`/
            // §13a pull-clear in the cell. ice_timeout is per-CELL
            // `2×seed_for(cell)`: 32-42s builder, 60s fetcher,
            // **1200s metal** — NOT a uniform 60s floor. If
            // `nodeclaim_reaped_total{reason=ice}` rate spikes on
            // cells with healthy capacity, split to
            // `ReapReason::RequestLimited` (no mask) — Q1.
            Some(_) => out.push((i, ReapReason::Ice)),
        }
    }
    out
}

/// ICE detection for claims Karpenter GC'd between ticks. `inflight`
/// holds `(name, cell)` for everything `cover_deficit` created and
/// hasn't yet observed Registered, terminating, or absent.
/// `tombstones` carries the delete-provenance axis (bug_094): names
/// whose delete THIS controller already attempted with an ambiguous
/// error — consulted so a committed-but-errored delete classifies as
/// our own reap, and consumed on every exit.
///
/// Drop rules (all `→ false` removes the entry; the closed
/// [`VanishClass`] alphabet is the classification law):
/// - **RegisteredHandoff**: `observe_registered`/FFD own it now.
/// - **DeliberateTeardown** (REGISTERED ∧ terminating): the controller
///   (or Karpenter expiration/consolidation) is tearing down a node
///   that proved launchable; not an ICE signal. live_050(b): this
///   rationale is REGISTERED-ONLY — the pre-fix arm applied it to
///   never-Registered claims too, silently eating launch-failure
///   teardowns.
/// - **SelfReap** (tombstoned ∧ (terminating ∨ absent)): the
///   ambiguous-commit delete CONFIRMED — the teardown is this
///   controller's own reap arriving one tick late, never Karpenter
///   evidence. The exit applies the ORIGINAL classification's
///   consequence (`record_reap` parity: counter under the original
///   reason; ICE-mask iff the reason was `Ice`) so deferred
///   confirmation loses no evidence and mints no false ICE (bug_094:
///   pre-fix this row classified `LaunchFailureTeardown`/`GcVanish` —
///   the exact mask the `record_reap` pin forbids for `BootTimeout`).
/// - **BootFailureTeardown** (NEVER-Registered ∧ terminating ∧
///   `Launched=True`, no tombstone): an EXTERNAL teardown of a claim
///   whose capacity provably materialized — Karpenter's registration
///   TTL fires before our `ice_timeout` on slow cells (15min TTL <
///   2×seed ≈ 20min metal). Boot failure, not capacity failure:
///   counted `reason=boot-timeout` (feeding the boot-failure alert),
///   NO ICE mask — the vanish-path mirror of `classify`'s
///   `BootTimeout` posture.
/// - **LaunchFailureTeardown** (NEVER-Registered ∧ terminating ∧
///   `Launched` ∈ {False, absent/Unknown}, no tombstone): Karpenter
///   terminal launch failure → deletionTimestamp → finalize, observed
///   mid-GC-transit (the window straddles a 10s tick whenever
///   finalization outlasts the tick boundary). Launch-failure
///   teardown, NOT deliberate consolidation: produces the SAME
///   unfulfillable evidence as a vanish — ICE-mask +
///   `reaped_total{reason=vanished}`.
/// - **GcVanish** (absent from `live`, no tombstone, never observed
///   Registered): vanished without ever Registering. Karpenter GC'd it
///   (the controller's own COMPLETED reaps are removed from `inflight`
///   by the caller before this runs; its AMBIGUOUS ones carry
///   tombstones) ⇒ the cell is unfulfillable. ICE-mask +
///   `reaped_total{reason=vanished}`. The `Launched` axis is
///   unreadable here (the object is gone) — absent stays capacity-side
///   CONDITIONALLY: the fast (~1s) GC that evades the terminating
///   observation is the `Launched=False LaunchFailed` path, while a
///   `Launched=True` teardown rides a ~60–90s finalizer and is
///   observed terminating across multiple 10s ticks (the
///   BootFailureTeardown row) — PROVIDED ticks fire at the 10 s
///   cadence. sh-030 falsified the unconditionality: a 295 s tick (the
///   inferred kube-rs `Config.timeout` — bounded since at
///   3×[`TICK`](super::TICK) by
///   [`super::NodeClaimPoolReconciler::tick`]) let 69 claims Register,
///   sit empty past Karpenter `consolidateAfter`, and vanish between
///   ticks; the next fold ICE-masked the cell for what was Karpenter
///   cleanup, not capacity failure. The
///   [`InflightClaim::ever_registered`] axis splits that row out
///   ([`VanishClass::EmptyConsolidation`]).
/// - **EmptyConsolidation** (absent from `live`, no tombstone, this
///   controller observed it Registered on an earlier tick): empty-node
///   consolidation across a controller-blind window — capacity provably
///   materialized. `reaped_total{reason=vanished}` (the alert still
///   sees the churn), NO ICE mask. Defense-in-depth beside the 3×TICK
///   bound; the B11 never-Registered fast-GC row is unaffected.
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
/// present with status ∉ `{True, False}` past timeout — CLOSED at
/// sh-043: `classify`'s `Some(_)` arm now reaps `Launched=Unknown`
/// past `ice_timeout` as `Ice` (Karpenter throttle/backlog ≡
/// capacity-side). (a) leaves the claim itself stuck in the cluster —
/// already operator-visible via `nodeclaim_inflight_age_max_seconds` —
/// and the entry frees the moment the claim resolves. The gauge half
/// the bug_020 report recommended lands as
/// `rio_controller_nodeclaim_inflight_tracked` in
/// `emit_inflight_tracked_gauge` (sh-043).
pub fn detect_vanished(
    inflight: &mut HashMap<String, InflightClaim>,
    tombstones: &mut DeleteTombstones,
    live: &[LiveNode],
) -> Vec<Cell> {
    let live_by_name: HashMap<&str, &LiveNode> =
        live.iter().map(|n| (n.name.as_str(), n)).collect();
    // sh-030: latch ever_registered BEFORE the retain — a claim
    // observed Registered on THIS tick exits via RegisteredHandoff (no
    // mark either way), but the latch is the controller's accumulated
    // direct observation, not a one-tick snapshot.
    for (name, e) in inflight.iter_mut() {
        e.ever_registered |= live_by_name
            .get(name.as_str())
            .is_some_and(|n| n.registered);
    }
    let mut ice = Vec::new();
    inflight.retain(|name, e| {
        let cell = &e.cell;
        let observed = live_by_name.get(name.as_str()).copied();
        // merged_bug_050 freshness gate (R29′): the provenance axis
        // is consultable only by a fold whose LIST post-dates the
        // stamp — a same-tick stamp's pre-delete LIST structurally
        // cannot carry the delete's evidence (three of the four
        // lane-by-mode stamp cells are stamp-before-fold).
        let Some(class) = classify_vanish(
            observed,
            tombstones.consultable_reason(name),
            e.ever_registered,
        ) else {
            // Still in-flight: KEEP. classify's reason short-circuit
            // and ice_timeout don't cover the GC'd-between-
            // observations window for slow-ICE cells. A tombstone (if
            // any) stays armed — the apiserver view may be one LIST
            // behind the committed delete; expiry bounds it.
            return true;
        };
        // Total fold over the exit alphabet (zero wildcard arms):
        // every exit either hands off quietly or produces its
        // classification's OWN evidence — never a silent
        // launch-failure exit, never Karpenter-attributed evidence
        // for this controller's own delete.
        // r[impl ctrl.nodeclaim.ice-mark-clear+6]
        match class {
            VanishClass::RegisteredHandoff | VanishClass::DeliberateTeardown => {}
            VanishClass::SelfReap(reason) => {
                debug!(
                    %name, %cell, reason = reason.as_str(),
                    "tracked NodeClaim exited via this controller's own earlier \
                     delete (ambiguous commit confirmed); applying the original \
                     reap consequence"
                );
                metrics::counter!(
                    "rio_controller_nodeclaim_reaped_total",
                    "reason" => reason.as_str(),
                    "cell" => cell.to_string(),
                )
                .increment(1);
                if reason == ReapReason::Ice {
                    ice.push(cell.clone());
                }
            }
            VanishClass::BootFailureTeardown => {
                warn!(
                    %name, %cell,
                    "in-flight NodeClaim torn down before Registering with \
                     Launched=True (external boot-failure teardown, e.g. \
                     Karpenter registration TTL); counting boot-timeout, NOT \
                     ICE-masking (capacity existed)"
                );
                metrics::counter!(
                    "rio_controller_nodeclaim_reaped_total",
                    "reason" => ReapReason::BootTimeout.as_str(),
                    "cell" => cell.to_string(),
                )
                .increment(1);
            }
            VanishClass::LaunchFailureTeardown | VanishClass::GcVanish => {
                warn!(
                    %name, %cell, ?class,
                    "in-flight NodeClaim {}; ICE-masking cell",
                    match class {
                        VanishClass::LaunchFailureTeardown =>
                            "terminating without ever Registering (launch-failure teardown)",
                        _ => "vanished (Karpenter GC)",
                    }
                );
                metrics::counter!(
                    "rio_controller_nodeclaim_reaped_total",
                    "reason" => "vanished",
                    "cell" => cell.to_string(),
                )
                .increment(1);
                ice.push(cell.clone());
            }
            VanishClass::EmptyConsolidation => {
                warn!(
                    %name, %cell,
                    "tracked NodeClaim consolidated empty (Registered on \
                     an earlier tick, never pod-bound, vanished across a \
                     controller-blind window); NOT ICE-masking — capacity \
                     materialized (sh-030)"
                );
                metrics::counter!(
                    "rio_controller_nodeclaim_reaped_total",
                    "reason" => "vanished",
                    "cell" => cell.to_string(),
                )
                .increment(1);
            }
        }
        // merged_bug_050 (R32, the `sys.obligation.linear-discharge`
        // doctrine): tombstone disposition is a TYPED per-exit
        // property, not a uniform epilogue — an exit cannot drop a
        // tombstone untyped. Only an exit that FIRED the packet may
        // consume; every other exit with a live tombstone HANDS it
        // to the registered-population sweep (the name leaves
        // `inflight` here, so the sweep owns it from the next
        // statement of the chokepoint on: disconfirm-keeps,
        // confirm-fires-the-packet, expiry-is-disclosed). Pre-fix
        // the uniform `remove` consumed the RegisteredHandoff exit's
        // tombstone on disconfirmed-only evidence — the committed
        // delete's consequence packet (counter, Ice mask, wedge
        // eviction) was silently lost while the sweep kept identical
        // evidence armed one population over.
        match tombstone_disposition(class) {
            TombstoneDisposition::ConsumedWithPacket => tombstones.remove(name),
            TombstoneDisposition::HandedToSweep => {}
        }
        false
    });
    ice
}

/// The typed per-exit tombstone discharge (merged_bug_050, R32 — an
/// instance of the `sys.obligation.linear-discharge` doctrine): every
/// [`VanishClass`] exit names what happens to the name's delete
/// tombstone, and the only consuming arm is the one that fired the
/// packet. The match below is TOTAL over the exit alphabet — a new
/// exit class cannot compile without declaring its disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TombstoneDisposition {
    /// The exit applied the tombstone's own consequence packet
    /// ([`VanishClass::SelfReap`] — reachable only through the
    /// freshness-gated provenance consult): the obligation
    /// discharges by consumption.
    ConsumedWithPacket,
    /// The exit did NOT fire the packet: a live tombstone (if any)
    /// stays armed and the registered-population sweep owns it —
    /// the name leaves the vanish fold's population at this exit, so
    /// the sweep's partition takes over (disconfirm keeps, confirm
    /// fires the packet through `record_reap`, expiry is the typed
    /// disclosed disposition). Covers the disconfirmed
    /// RegisteredHandoff cell AND the freshness-gated same-tick
    /// cells uniformly; for the no-tombstone exits the hand-off is
    /// vacuous.
    HandedToSweep,
}

// r[impl ctrl.pool.delete-outcome]
pub fn tombstone_disposition(class: VanishClass) -> TombstoneDisposition {
    match class {
        VanishClass::SelfReap(_) => TombstoneDisposition::ConsumedWithPacket,
        VanishClass::RegisteredHandoff
        | VanishClass::DeliberateTeardown
        | VanishClass::BootFailureTeardown
        | VanishClass::LaunchFailureTeardown
        | VanishClass::GcVanish
        | VanishClass::EmptyConsolidation => TombstoneDisposition::HandedToSweep,
    }
}

/// The closed exit alphabet at the vanish seam (live_050(b), widened
/// by bug_094): *every launch-failure observation, on every
/// observation path, produces the same unfulfillable evidence — and
/// only genuinely capacity-shaped, genuinely foreign teardowns
/// produce it.* `None` = still in-flight (KEEP — not an exit). The
/// live_050(b) pre-fix retain arm conflated the registered and
/// never-registered teardown rows, starving IceBackoff (vanished=101
/// vs ice=0; zero `:od` claims minted). The bug_094 pre-fix alphabet
/// classified over (present × registered × terminating) ONLY — the
/// deciding axes (delete provenance, `Launched`) were never in the
/// product, so an ambiguous-commit own-reap and a Karpenter
/// registration-TTL boot teardown both minted the FALSE ICE mask the
/// `record_reap` pin forbids for `BootTimeout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VanishClass {
    /// Registered (∧ not terminating): `observe_registered`/FFD own it.
    RegisteredHandoff,
    /// Registered ∧ terminating (no tombstone): deliberate teardown of
    /// a node that proved launchable — not an ICE signal.
    DeliberateTeardown,
    /// Tombstoned ∧ (terminating ∨ absent): this controller's OWN
    /// delete, confirmed after an ambiguous error. Carries the
    /// original [`ReapReason`]; the exit applies that reason's
    /// `record_reap` consequence (mask iff `Ice`) — never the
    /// vanish-attributed evidence.
    SelfReap(ReapReason),
    /// NEVER-Registered ∧ terminating ∧ `Launched=True` (no
    /// tombstone): external boot-failure teardown (Karpenter
    /// registration TTL) — capacity existed; counts `boot-timeout`,
    /// never masks.
    BootFailureTeardown,
    /// NEVER-Registered ∧ terminating ∧ `Launched` ∈ {False,
    /// absent/Unknown} (no tombstone): launch-failure teardown caught
    /// mid-GC-transit — IS ICE evidence (marks exactly like GcVanish).
    LaunchFailureTeardown,
    /// Absent from `live` (no tombstone), never observed Registered:
    /// GC'd between ticks without Registering — capacity-side (see
    /// the [`detect_vanished`] GcVanish row for why `Launched` is
    /// unreadable AND immaterial here, conditional on the 10 s tick
    /// cadence the 3×TICK bound enforces).
    GcVanish,
    /// Absent from `live` (no tombstone), this controller OBSERVED it
    /// Registered on an earlier tick (sh-030): Karpenter empty-node
    /// consolidation across a controller-blind window — capacity
    /// provably materialized; counts `vanished`, never masks.
    EmptyConsolidation,
}

/// Pure classification law for one tracked claim's observation —
/// `observed = None` ⇔ absent from `live`; `self_delete = Some(r)` ⇔
/// this controller already attempted the claim's delete for reason
/// `r` and got an ambiguous (non-404) error back. Product-censused by
/// `vanish_class_census_over_the_observation_product` (present ×
/// registered × terminating × launched × provenance — the cells come
/// FROM the alphabet's axes; bug_094's R22 lesson: the pre-fix census
/// was enrolled and green over a product MISSING the deciding axes).
pub fn classify_vanish(
    observed: Option<&LiveNode>,
    self_delete: Option<ReapReason>,
    ever_registered: bool,
) -> Option<VanishClass> {
    match (observed, self_delete) {
        // Provenance rows first: a tombstoned claim observed
        // terminating or absent is the controller's own delete
        // confirmed — regardless of registered/Launched (the original
        // classification already adjudicated those).
        (None, Some(r)) => Some(VanishClass::SelfReap(r)),
        (Some(n), Some(r)) if n.terminating() => Some(VanishClass::SelfReap(r)),
        // Observation rows (tombstone absent, not yet consultable
        // under the freshness gate, or present but DISCONFIRMED —
        // the claim is alive and not terminating ON THIS LIST, which
        // may pre-date the stamp: disconfirmation is
        // evidence-so-far, never proof of non-commit, which is why
        // these exits' tombstone discharge is HandedToSweep, never
        // consumption — merged_bug_050).
        (Some(n), _) if n.registered && n.terminating() => Some(VanishClass::DeliberateTeardown),
        (Some(n), _) if n.registered => Some(VanishClass::RegisteredHandoff),
        (Some(n), _) if n.terminating() => match n.launched() {
            Some(true) => Some(VanishClass::BootFailureTeardown),
            _ => Some(VanishClass::LaunchFailureTeardown),
        },
        (Some(_), _) => None,
        // sh-030: an absent claim the controller saw Registered is
        // Karpenter empty-node consolidation across a blind window —
        // not capacity failure. The B11 never-Registered fast-GC row
        // (the only sub-tick launch-failure detector) is the false
        // arm — preserved exactly.
        (None, None) if ever_registered => Some(VanishClass::EmptyConsolidation),
        (None, None) => Some(VanishClass::GcVanish),
    }
}

/// How many CONSUMER FOLD EXECUTIONS an unconfirmed delete tombstone
/// survives. THE CLOCK IS THE CONSUMING FOLD'S OWN EXECUTION COUNT
/// (`DeleteTombstones::folds`, advanced by [`vanish_fold`] strictly
/// AFTER each consult) — never the wall tick counter (R29,
/// `ctrl.pool.fold-clock`): the fold is skipped on pre-threshold
/// ⊥ ticks and failed-LIST ticks while `tick_counter` keeps
/// advancing, so a wall-denominated grace silently shortened toward
/// zero as skip-paths accumulated — bug_043: a tombstone stamped just
/// before a ≥3-tick foldless window was pruned before its first
/// consult, and the next fold minted GcVanish (false ICE mask +
/// `reaped_total{reason=vanished}`) for the controller's own
/// BootTimeout self-reap. Denominated in fold executions, every
/// future skip-path LENGTHENS real grace instead of shortening it.
///
/// Violable envelope (R17), derivation: confirmation normally lands
/// at the next CONSULTED LIST (a committed delete shows
/// `deletionTimestamp` immediately; the ~60–90s finalizer keeps it
/// observable across many folds), so 3 covers one stale-LIST consult
/// plus one missed consult — while bounding the window in which an
/// INDEPENDENT same-name teardown could be mis-attributed to this
/// controller (that suppression additionally requires the claim to
/// flip into a capacity-failure shape after our non-ICE
/// classification — near-contradictory, since Karpenter never
/// transitions `Launched` True→False; an `Ice`-reason tombstone's
/// consequence is the mask either way, so nothing is suppressed on
/// that arm).
const TOMBSTONE_TTL_FOLDS: u64 = 3;

/// One stored ambiguous delete attempt: the full consequence packet
/// the reap lane produced ([`AmbiguousDelete`]) plus the consumer
/// fold count at which it was stamped (the R29 clock — see
/// [`TOMBSTONE_TTL_FOLDS`]).
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteAttempt {
    pub seed: AmbiguousDelete,
    pub stamped_fold: u64,
}

/// Delete-provenance tombstones (bug_094): NodeClaim names whose
/// `delete()` THIS controller attempted and got a non-404 error back
/// — an AMBIGUOUS outcome (the apiserver may have committed the
/// delete before the error). Carried across ticks so the next
/// observation of the claim terminating/absent classifies as
/// [`VanishClass::SelfReap`] instead of Karpenter teardown evidence.
///
/// Mutators (the `inflight_created` discipline, same shape — together
/// they make every tombstone exit a CONSUMED consequence or a
/// disclosed expiry, the `ctrl.pool.delete-outcome` consumer law):
/// 1. [`stamp`](Self::stamp) — the callers fold BOTH lanes'
///    `delete_attempted` packets in ([`ReapOutcome`] and
///    `consolidate::ReapIdleOutcome`), stamped with the current
///    leader tick (re-stamping an existing name refreshes its TTL —
///    a repeated Err is a fresh ambiguous attempt).
/// 2. [`remove`](Self::remove) — consumed on every vanish-fold exit
///    (`detect_vanished`), and by the callers for names a RETRIED
///    delete completed (`Ok`/404 → `reaped_claims` — the completed
///    reap's `record_reap` already applied the consequence).
/// 3. [`sweep_registered_tombstones`] — consumed on every CONFIRMED
///    registered-claim exit (terminating/absent observation of a
///    name outside the vanish fold's population), applying the
///    original reason's full consequence (bug_042: pre-sweep these
///    tombstones had no consumer at all — the Dead-reap wedge
///    eviction never fired and `reaped_total{reason=dead}`
///    permanently undercounted).
/// 4. the post-consult prune inside [`vanish_fold`] — DISCONFIRMED
///    entries older than [`TOMBSTONE_TTL_FOLDS`] drop as a typed,
///    DISCLOSED disposition (warn + expiry counter), never a silent
///    prune; the clock advance and the prune are private to the
///    chokepoint (consult-then-prune by construction).
/// 5. `clear()` on the ACQUISITION EDGE (suppress polarity, the
///    `inflight_created` row's rationale: a stale previous-tenure
///    tombstone could suppress a genuine vanish ICE of a same-named
///    successor claim).
#[derive(Debug, Default)]
pub struct DeleteTombstones {
    entries: HashMap<String, DeleteAttempt>,
    /// The CONSUMER CLOCK (R29): completed [`vanish_fold`] executions.
    /// Stamps record it; expiry is measured against it; it advances
    /// strictly AFTER each consult — so a tombstone is structurally
    /// never pruned unconsulted, and wall ticks that skip the fold
    /// (⊥ ticks, failed LISTs) do not age the grace.
    folds: u64,
}

impl DeleteTombstones {
    /// Record an ambiguous delete attempt at the CURRENT fold count
    /// (re-stamping an existing name refreshes its grace and
    /// consequence packet — a repeated `Err` is a fresh ambiguous
    /// attempt). A stamp made while the count reads F is first
    /// CONSUMABLE by the fold that completes count F+1 (the R29′
    /// freshness gate, merged_bug_050: the same-window fold's LIST
    /// may pre-date the delete) and cannot expire before the count
    /// reads F+`TTL`+1 — a fresh stamp is never pruned by its own
    /// tick BY CONSTRUCTION, and every stamp gets ≥3 consultable
    /// folds before expiry whichever lane-by-mode cell stamped it.
    pub fn stamp(&mut self, seed: AmbiguousDelete) {
        let stamped_fold = self.folds;
        self.entries
            .insert(seed.name.clone(), DeleteAttempt { seed, stamped_fold });
    }

    /// The tombstoned reason for `name`, IF its stamp is consultable
    /// by the current fold — the provenance axis [`classify_vanish`]
    /// consumes. R29′ freshness (merged_bug_050): a stamp made while
    /// the fold count reads F is first consultable by the fold that
    /// COMPLETES count F+1 (i.e. `stamped_fold < folds` at consult
    /// time) — the same-window fold's LIST may pre-date the delete
    /// and structurally cannot carry its evidence. For post-fold
    /// stamps (the idle × reconcile_once cell) this is one fold
    /// conservative — the safe direction: grace only lengthens, and
    /// the TTL still guarantees ≥3 consultable folds per stamp.
    pub fn consultable_reason(&self, name: &str) -> Option<ReapReason> {
        self.entries
            .get(name)
            .filter(|a| self.folds.wrapping_sub(a.stamped_fold) >= 1)
            .map(|a| a.seed.reason)
    }

    /// The RAW tombstoned reason for `name` — test assertions only
    /// (no freshness gating); every PRODUCTION provenance consult
    /// goes through [`Self::consultable_reason`].
    #[cfg(test)]
    pub fn reason(&self, name: &str) -> Option<ReapReason> {
        self.entries.get(name).map(|a| a.seed.reason)
    }

    /// Consume `name`'s tombstone (exit observed or retry completed).
    pub fn remove(&mut self, name: &str) {
        self.entries.remove(name);
    }

    /// Advance the consumer clock by one completed fold, then drop
    /// entries older than [`TOMBSTONE_TTL_FOLDS`] folds. Called only
    /// from [`vanish_fold`], strictly AFTER the consult — the
    /// consult-then-prune order is owned by one fn, not by call-site
    /// discipline (bug_043: the pre-fix callers each ran prune_expired
    /// BEFORE detect_vanished). `wrapping_sub` matches the clock's wrap.
    ///
    /// Expiry is a TYPED, DISCLOSED disposition, never a silent prune
    /// (`ctrl.pool.delete-outcome`, bug_042): with the vanish fold
    /// consuming every in-flight exit and
    /// [`sweep_registered_tombstones`] consuming every confirmed
    /// registered exit each fold, an entry can only reach expiry
    /// DISCONFIRMED — its claim was observed alive (the delete
    /// provably had not committed) or was never re-observed at all.
    /// Each drop warns with the attempt's reason and increments
    /// `rio_controller_nodeclaim_tombstone_expired_total{reason}`.
    fn advance_fold_and_prune(&mut self) {
        self.folds = self.folds.wrapping_add(1);
        let now_fold = self.folds;
        self.entries.retain(|name, a| {
            let keep = now_fold.wrapping_sub(a.stamped_fold) <= TOMBSTONE_TTL_FOLDS;
            if !keep {
                warn!(
                    %name, reason = a.seed.reason.as_str(),
                    "delete tombstone expired unconfirmed (disconfirmed or never \
                     re-observed) after its fold-denominated grace; dropping \
                     provenance — a later same-name teardown is foreign \
                     evidence again"
                );
                metrics::counter!(
                    "rio_controller_nodeclaim_tombstone_expired_total",
                    "reason" => a.seed.reason.as_str(),
                )
                .increment(1);
            }
            keep
        });
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(test)]
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }
}

/// One registered-tombstone sweep's applied consequences — the caller
/// routes each half to its plane (evictions → the wedge stash, gaps →
/// the cell's idle-gap ring, masks → the evidence buffer).
#[derive(Debug, Default)]
pub struct TombstoneSweep {
    /// Cells masked by a confirmed `Ice`-reason tombstone (total over
    /// the reason alphabet; unpopulated today — registered-claim reaps
    /// are `Dead`/`Idle` — but the arm exists so a future
    /// registered-ICE lane inherits the law, not a silent gap).
    pub ice_cells: Vec<Cell>,
    /// Backing-node names of confirmed reaps — the wedge tracker's
    /// REQUIRED eviction feed (bug_042: this is the half whose loss
    /// kept a dead node's expiry evidence wedge-admissible for the
    /// ~60-90s finalizer window).
    pub evicted_nodes: Vec<String>,
    /// `(cell, gap_secs)` censored idle samples carried by confirmed
    /// `Idle` tombstones — pushed through `consolidate::push_idle_gap`
    /// by the caller (the lane's own ring).
    pub censored_gaps: Vec<(Cell, f64)>,
    /// Names whose tombstones confirmed this sweep (consumed; the
    /// claims are registered, so no `inflight_created` cleanup rides
    /// this list — it exists for observability and the unit census).
    pub confirmed: Vec<String>,
}

/// The registered-claim tombstone consumer (bug_042, the
/// `ctrl.pool.delete-outcome` consumer-census close): every tick,
/// tombstones for names OUTSIDE the vanish fold's population
/// (`inflight_created` — those are [`detect_vanished`]'s to consume)
/// are matched against the live view. A tombstoned claim observed
/// TERMINATING (`deletionTimestamp` set — the delete committed; the
/// finalizer is draining) or ABSENT (committed and finalized between
/// ticks) is this controller's own delete CONFIRMED — the sweep
/// applies the original reason's FULL consequence through the same
/// code path as the prompt arms ([`record_reap`]: counter under the
/// original reason, mask iff `Ice`) plus the carried halves (the
/// wedge eviction via the packet's `node_name`, the censored idle
/// gap), and consumes the tombstone. A claim observed alive and not
/// terminating keeps its tombstone armed (DISCONFIRMED so far — the
/// reap lane retries while its conditions persist, and a completed
/// retry consumes via the callers' `reaped_claims` loop).
///
/// Mis-attribution bound: within [`TOMBSTONE_TTL_FOLDS`] of OUR
/// delete attempt, an independent foreign teardown of the same claim
/// would be counted as ours — the same bounded suppression window the
/// TTL derivation prices for the vanish fold, and the consequence is
/// safe under it (a terminating node's evidence is evicted exactly
/// like fleet absence would evict it one window later). The R29′
/// freshness gate (merged_bug_050) NARROWS the window's front edge:
/// an observation on a LIST pre-dating the stamp is never consumed
/// as ours — the same-tick fold keeps the entry armed.
// r[impl ctrl.pool.delete-outcome]
pub fn sweep_registered_tombstones(
    tombstones: &mut DeleteTombstones,
    inflight: &HashMap<String, InflightClaim>,
    live: &[LiveNode],
) -> TombstoneSweep {
    let live_by_name: HashMap<&str, &LiveNode> =
        live.iter().map(|n| (n.name.as_str(), n)).collect();
    let mut out = TombstoneSweep::default();
    let current_fold = tombstones.folds;
    tombstones.entries.retain(|name, a| {
        if inflight.contains_key(name) {
            // The vanish fold's population — its exits consume these
            // (one consumer per tombstone, partitioned by population).
            return true;
        }
        if current_fold.wrapping_sub(a.stamped_fold) == 0 {
            // R29′ freshness (merged_bug_050): a same-window stamp's
            // LIST may pre-date the delete — adjudicating it here
            // would consume (and fire the packet for) an observation
            // that structurally cannot carry the delete's evidence.
            // Keep armed; the next fold's LIST post-dates the stamp.
            return true;
        }
        let confirmed = match live_by_name.get(name.as_str()) {
            None => true,
            Some(n) if n.terminating() => true,
            Some(_) => false,
        };
        if !confirmed {
            return true;
        }
        debug!(
            %name, reason = a.seed.reason.as_str(), cell = %a.seed.cell,
            "registered NodeClaim's ambiguous delete confirmed \
             (terminating/absent); applying the original reap consequence"
        );
        record_reap(
            a.seed.reason,
            a.seed.cell.clone(),
            name,
            &mut out.ice_cells,
            &mut out.confirmed,
        );
        out.evicted_nodes.extend(a.seed.node_name.clone());
        if let Some(gap) = a.seed.idle_gap_secs {
            out.censored_gaps.push((a.seed.cell.clone(), gap));
        }
        false
    });
    out
}

/// One [`vanish_fold`] execution's combined consequences — the union
/// of the in-flight fold's masks and the registered sweep's halves.
#[derive(Debug, Default)]
pub struct VanishFold {
    /// ICE masks: the vanish fold's exits plus any confirmed
    /// `Ice`-reason sweep consequence (alphabet-total).
    pub ice_cells: Vec<Cell>,
    /// Confirmed reaps' backing nodes — the wedge eviction feed.
    pub evicted_nodes: Vec<String>,
    /// Confirmed `Idle` tombstones' censored samples.
    pub censored_gaps: Vec<(Cell, f64)>,
}

/// THE per-tick tombstone/vanish consumer chokepoint
/// (`ctrl.pool.fold-clock`, R29): one fn owns the whole
/// consult-then-prune sequence —
///
/// 1. the in-flight vanish fold ([`detect_vanished`]: every tracked
///    exit consumes its tombstone with its classification's
///    consequence),
/// 2. the registered-tombstone sweep
///    ([`sweep_registered_tombstones`]: every confirmed
///    registered exit applies its reason's full consequence),
/// 3. only then the consumer clock advances and DISCONFIRMED entries
///    older than [`TOMBSTONE_TTL_FOLDS`] expire as the disclosed
///    disposition.
///
/// The ordering is structural, not call-site discipline: a tombstone
/// can never be pruned unconsulted, because the only prune lives
/// after the only consult, inside this fn — and the grace is
/// denominated in executions OF THIS FN, so the skip-paths that do
/// not call it (pre-threshold ⊥ ticks, failed-LIST ticks) lengthen
/// real grace instead of consuming it (bug_043's wall-clock law
/// shortened it toward zero).
// r[impl ctrl.pool.fold-clock]
// r[impl ctrl.pool.delete-outcome]
pub fn vanish_fold(
    inflight: &mut HashMap<String, InflightClaim>,
    tombstones: &mut DeleteTombstones,
    live: &[LiveNode],
) -> VanishFold {
    let mut out = VanishFold {
        ice_cells: detect_vanished(inflight, tombstones, live),
        ..Default::default()
    };
    let swept = sweep_registered_tombstones(tombstones, inflight, live);
    out.ice_cells.extend(swept.ice_cells);
    out.evicted_nodes = swept.evicted_nodes;
    out.censored_gaps = swept.censored_gaps;
    tombstones.advance_fold_and_prune();
    out
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
/// `Api::delete` outcomes are the [`DeleteOutcome`] total: 404 takes
/// the full `Ok` consequence (the claim WAS reaped — GC raced);
/// other errors warn, tombstone the attempt (`delete_attempted` —
/// the outcome is AMBIGUOUS: the apiserver may have committed before
/// erring), and skip (next tick retries if the claim is still alive).
#[derive(Debug, Default)]
pub struct ReapOutcome {
    pub ice_cells: Vec<Cell>,
    pub reaped_claims: Vec<String>,
    pub reaped_nodes: Vec<String>,
    /// Consequence packets for deletes that returned a non-404 error.
    /// The callers stamp these into their [`DeleteTombstones`] so the
    /// next tick's vanish fold classifies a committed-but-errored
    /// delete as [`VanishClass::SelfReap`] — this controller's own
    /// reap, never Karpenter evidence (bug_094).
    pub delete_attempted: Vec<AmbiguousDelete>,
}

pub async fn reap_unhealthy(
    nodeclaims: &Api<NodeClaim>,
    live: &[LiveNode],
    dead_nodes: &[String],
    sketches: &CellSketches,
    cfg: &NodeClaimPoolConfig,
    now_secs: f64,
    pass_fence: &crate::reconcilers::fence::MutationFence,
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
        // D4 mutation seam: a deposed pass deletes nothing more.
        if pass_fence.check("nodeclaim-reap-unhealthy").is_err() {
            break;
        }
        // The eviction-source law's typed total (ctrl.pool.delete-
        // outcome): the raw kube Result is translated ONCE, and the
        // exhaustive match below is the lane's per-arm consequence
        // decision — the 404 arm shares the OkDeleted consequence
        // path by or-pattern (parity unwritable-to-diverge).
        let outcome =
            DeleteOutcome::classify(nodeclaims.delete(&n.name, &DeleteParams::default()).await);
        let raced = matches!(outcome, DeleteOutcome::Committed404);
        match outcome {
            DeleteOutcome::OkDeleted | DeleteOutcome::Committed404 => {
                debug!(
                    name = %n.name, %cell, reason = reason.as_str(), raced,
                    "reaped unhealthy NodeClaim (404 = Karpenter GC raced; same full consequence)"
                );
                record_reap(
                    reason,
                    cell,
                    &n.name,
                    &mut out.ice_cells,
                    &mut out.reaped_claims,
                );
                out.reaped_nodes.extend(n.node_name.clone());
            }
            DeleteOutcome::AmbiguousErr(e) => {
                // AMBIGUOUS: the apiserver may have committed the
                // delete before erring. The name must not vanish from
                // provenance — a next-tick terminating/absent
                // observation is OUR reap, not Karpenter evidence.
                warn!(
                    name = %n.name, error = %e,
                    "unhealthy NodeClaim delete failed; tombstoning the attempt \
                     (ambiguous commit) and retrying next tick if still alive"
                );
                out.delete_attempted.push(AmbiguousDelete {
                    name: n.name.clone(),
                    reason,
                    cell,
                    node_name: n.node_name.clone(),
                    idle_gap_secs: None,
                });
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

    /// Minimal tombstone seed for the provenance-plane tests (cell
    /// `h:spot`, no backing node, no idle gap — the axes under test
    /// are name/reason/clock).
    fn ts_seed(name: &str, reason: ReapReason) -> AmbiguousDelete {
        AmbiguousDelete {
            name: name.into(),
            reason,
            cell: Cell("h".into(), CapacityType::Spot),
            node_name: None,
            idle_gap_secs: None,
        }
    }

    /// `ReapReason::LETTERS` is total over the alphabet — rustc
    /// exhaustiveness pins the index map, so a new variant cannot
    /// compile without joining `LETTERS` (and thereby every reason-indexed
    /// census product).
    #[test]
    fn reap_reason_alphabet_total() {
        let idx = |r: ReapReason| -> usize {
            match r {
                ReapReason::Ice => 0,
                ReapReason::BootTimeout => 1,
                ReapReason::Dead => 2,
                ReapReason::Idle => 3,
            }
        };
        let mut seen = [false; ReapReason::LETTERS.len()];
        for r in ReapReason::LETTERS {
            assert!(!seen[idx(r)], "duplicate {r:?} in LETTERS");
            seen[idx(r)] = true;
        }
        assert!(seen.iter().all(|s| *s), "LETTERS covers every variant");
    }

    /// The [`DeleteOutcome::classify`] partition, walked over the
    /// reachable result product (Ok + every distinct kube error
    /// shape): total — every input lands in exactly one arm — and 404
    /// is the only error that counts as a completed reap. This is the
    /// (outcome × lane) totality proof at the classifier; the per-lane
    /// arm decisions are rustc-exhaustive matches at the two delete
    /// sites (`delete_lane_census` pins the lane population).
    // r[verify ctrl.pool.delete-outcome]
    #[test]
    fn delete_outcome_partition_total() {
        let api_err = |code: u16| {
            kube::Error::Api(
                kube::core::Status::failure("m", "r")
                    .with_code(code)
                    .boxed(),
            )
        };
        assert!(matches!(
            DeleteOutcome::classify(Ok::<(), _>(())),
            DeleteOutcome::OkDeleted
        ));
        assert!(matches!(
            DeleteOutcome::classify(Err::<(), _>(api_err(404))),
            DeleteOutcome::Committed404
        ));
        // Every NON-404 api code is ambiguous (the apiserver may have
        // committed before erring) — including the conflict/forbidden
        // shapes a wrong fix might special-case as "definitely not
        // deleted".
        for code in [400u16, 403, 409, 410, 422, 429, 500, 503] {
            assert!(
                matches!(
                    DeleteOutcome::classify(Err::<(), _>(api_err(code))),
                    DeleteOutcome::AmbiguousErr(_)
                ),
                "api code {code} must classify ambiguous"
            );
        }
        // Non-API transport shapes (connection reset mid-flight is the
        // canonical committed-but-errored case) are ambiguous too.
        let transport = kube::Error::Service(Box::new(std::io::Error::other("conn reset")));
        assert!(matches!(
            DeleteOutcome::classify(Err::<(), _>(transport)),
            DeleteOutcome::AmbiguousErr(_)
        ));
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
        let mut inflight: HashMap<String, InflightClaim> = [
            ("nc-gone".into(), h.clone().into()),
            ("nc-inflight".into(), h.clone().into()),
            ("nc-reg".into(), h.clone().into()),
            ("nc-term".into(), h.clone().into()),
        ]
        .into();
        let mut reg = node("nc-reg", "h", CapacityType::Spot, 8, 0, 0);
        reg.registered = true;
        // live_050(b) re-derivation: the typed exit alphabet narrows
        // the quiet-teardown arm to REGISTERED claims (DeliberateTeardown)
        // — a never-registered terminating claim is LaunchFailureTeardown
        // and MARKS (pinned by `never_registered_terminating_claim_
        // marks_its_cell` and the product census); this battery keeps
        // the quiet arms quiet.
        let mut term = node("nc-term", "h", CapacityType::Spot, 8, 0, 0);
        term.registered = true;
        let term = set_terminating(term);
        // node() defaults registered=true; force in-flight.
        let mut inflight_node = node("nc-inflight", "h", CapacityType::Spot, 8, 0, 0);
        inflight_node.registered = false;
        let live = [inflight_node, reg, term];

        let mut ts = DeleteTombstones::default();
        let ice = detect_vanished(&mut inflight, &mut ts, &live);
        assert_eq!(ice, vec![h], "only nc-gone (absent) ICE-masked");
        assert_eq!(
            inflight.keys().collect::<Vec<_>>(),
            vec!["nc-inflight"],
            "in-flight present claim kept; registered/terminating/vanished dropped"
        );
        // Second call (claim still in-flight, still live): nothing new
        // ICE'd, claim stays tracked.
        assert!(detect_vanished(&mut inflight, &mut ts, &live).is_empty());
        assert_eq!(inflight.len(), 1);
        // Third call: claim GC'd between ticks → ICE.
        assert_eq!(
            detect_vanished(&mut inflight, &mut ts, &[]),
            vec![Cell("h".into(), CapacityType::Spot)]
        );
        assert!(inflight.is_empty());
    }

    // r[verify ctrl.nodeclaim.ice-mark-clear+6]
    /// live_050(b) red R3 / witness W7-C — certifies: *a never-Registered
    /// terminating claim produces a buffered-able mark with the vanish
    /// warn/counter — through the production retain path, not a
    /// hand-rolled map.* Pre-fix red (verbatim): `left: inflight entry
    /// dropped; ice == [] (no mark; misread as deliberate teardown) /
    /// right: ice == [cell]; reaped_total{reason=vanished} incremented;
    /// entry exits tracking`.
    #[test]
    fn never_registered_terminating_claim_marks_its_cell() {
        use super::super::ffd::tests::set_terminating;
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let h = Cell("h".into(), CapacityType::Spot);
        let mut inflight: HashMap<String, InflightClaim> =
            [("nc-doomed".to_string(), h.clone().into())].into();
        let mut doomed = node("nc-doomed", "h", CapacityType::Spot, 8, 0, 0);
        doomed.registered = false;
        let doomed = set_terminating(doomed);
        let rec = DebuggingRecorder::new();
        let _g = ::metrics::set_default_local_recorder(&rec);
        let ice = detect_vanished(&mut inflight, &mut DeleteTombstones::default(), &[doomed]);
        assert_eq!(
            ice,
            vec![h.clone()],
            "launch-failure teardown IS ICE evidence"
        );
        assert!(
            inflight.is_empty(),
            "exited tracking (classify owns nothing here)"
        );
        // ppppp: snapshot exactly once.
        let snap = rec.snapshotter().snapshot().into_vec();
        let vanished = snap.into_iter().find_map(|(k, _, _, v)| {
            let key = k.key();
            (key.name() == "rio_controller_nodeclaim_reaped_total"
                && key
                    .labels()
                    .any(|l| l.key() == "reason" && l.value() == "vanished")
                && key
                    .labels()
                    .any(|l| l.key() == "cell" && l.value() == h.to_string()))
            .then_some(v)
        });
        assert_eq!(
            vanished,
            Some(DebugValue::Counter(1)),
            "the same unfulfillable evidence as a GC vanish"
        );
        // Kill-isolation (the deliberate-teardown arm stays quiet): a
        // REGISTERED terminating claim exits with ZERO mark.
        let mut inflight: HashMap<String, InflightClaim> =
            [("nc-reg-term".to_string(), h.clone().into())].into();
        let mut reg_term = node("nc-reg-term", "h", CapacityType::Spot, 8, 0, 0);
        reg_term.registered = true;
        let reg_term = set_terminating(reg_term);
        assert!(
            detect_vanished(&mut inflight, &mut DeleteTombstones::default(), &[reg_term])
                .is_empty(),
            "registered teardown is deliberate — not an ICE signal"
        );
        assert!(inflight.is_empty());
    }

    /// W9-BB unit red (bug_094, the Launched-axis half): a NEVER-
    /// Registered terminating claim whose `Launched` condition is
    /// `True` is a BOOT failure observed via external teardown
    /// (Karpenter registration TTL on slow cells) — capacity provably
    /// materialized, so it must NOT produce ICE evidence (the same
    /// non-mask posture `record_reap` pins for `BootTimeout`), and the
    /// exit counts under `reason=boot-timeout` (the boot-failure alert
    /// sees it), never `reason=vanished`. Pre-fix red (verbatim):
    /// `Launched=True teardown is a boot failure, not capacity
    /// evidence: [Cell("h", Spot)]`.
    #[test]
    fn boot_failure_teardown_spares_the_cell() {
        use super::super::ffd::tests::set_terminating;
        use metrics_util::debugging::DebuggingRecorder;
        let h = Cell("h".into(), CapacityType::Spot);
        let mut inflight: HashMap<String, InflightClaim> =
            [("nc-ttl".to_string(), h.clone().into())].into();
        let mut n = with_conds(
            node("nc-ttl", "h", CapacityType::Spot, 8, 0, 0),
            &[("Launched", "True", 1001.0)],
        );
        n.registered = false;
        let n = set_terminating(n);
        let rec = DebuggingRecorder::new();
        let _g = ::metrics::set_default_local_recorder(&rec);
        let ice = detect_vanished(&mut inflight, &mut DeleteTombstones::default(), &[n]);
        assert!(
            ice.is_empty(),
            "Launched=True teardown is a boot failure, not capacity evidence: {ice:?}"
        );
        assert!(inflight.is_empty(), "the exit still leaves tracking");
        // ppppp: snapshot exactly once.
        let counts = reap_counts(rec.snapshotter().snapshot().into_vec(), &h);
        assert_eq!(
            counts,
            vec![("boot-timeout".to_string(), 1)],
            "counted as a boot failure, not vanish evidence"
        );
    }

    /// `(reason, count)` rows of `nodeclaim_reaped_total` for `cell`
    /// from one recorder snapshot, sorted (the per-exit counter
    /// comparator for the vanish-fold tests).
    fn reap_counts(
        snap: Vec<(
            metrics_util::CompositeKey,
            Option<metrics::Unit>,
            Option<metrics::SharedString>,
            metrics_util::debugging::DebugValue,
        )>,
        cell: &Cell,
    ) -> Vec<(String, u64)> {
        use metrics_util::debugging::DebugValue;
        let mut out: Vec<(String, u64)> = snap
            .into_iter()
            .filter_map(|(k, _, _, v)| {
                let key = k.key();
                if key.name() != "rio_controller_nodeclaim_reaped_total"
                    || !key
                        .labels()
                        .any(|l| l.key() == "cell" && l.value() == cell.to_string())
                {
                    return None;
                }
                let reason = key
                    .labels()
                    .find(|l| l.key() == "reason")?
                    .value()
                    .to_string();
                match v {
                    DebugValue::Counter(c) if c > 0 => Some((reason, c)),
                    _ => None,
                }
            })
            .collect();
        out.sort();
        out
    }

    /// bug_094 provenance half at the unit fold: a tombstoned claim
    /// observed terminating or absent is [`VanishClass::SelfReap`] —
    /// the exit applies the ORIGINAL reap's consequence (counter under
    /// the original reason; mask iff `Ice`), never the
    /// vanish-attributed evidence, and consumes the tombstone.
    #[test]
    fn self_reap_applies_the_original_consequence() {
        use super::super::ffd::tests::set_terminating;
        use metrics_util::debugging::DebuggingRecorder;
        let h = Cell("h".into(), CapacityType::Spot);

        // BootTimeout tombstone + terminating observation → no mask,
        // counter under boot-timeout (the W9-BB lifecycle red's unit
        // face).
        let mut inflight: HashMap<String, InflightClaim> =
            [("nc-bt".to_string(), h.clone().into())].into();
        let mut ts = DeleteTombstones::default();
        ts.stamp(ts_seed("nc-bt", ReapReason::BootTimeout));
        // R29′ freshness (merged_bug_050): a stamp is consultable
        // only from the next fold — model that fold boundary.
        ts.advance_fold_and_prune();
        let mut n = with_conds(
            node("nc-bt", "h", CapacityType::Spot, 8, 0, 0),
            &[("Launched", "True", 1001.0)],
        );
        n.registered = false;
        let n = set_terminating(n);
        let rec = DebuggingRecorder::new();
        let _g = ::metrics::set_default_local_recorder(&rec);
        let ice = detect_vanished(&mut inflight, &mut ts, &[n]);
        assert!(ice.is_empty(), "BootTimeout self-reap never masks: {ice:?}");
        assert!(inflight.is_empty() && ts.is_empty(), "exit consumed both");
        let counts = reap_counts(rec.snapshotter().snapshot().into_vec(), &h);
        assert_eq!(counts, vec![("boot-timeout".to_string(), 1)]);
        drop(_g);

        // Ice tombstone + ABSENT observation → the deferred consequence
        // is the mask (record_reap parity — no evidence lost to the
        // ambiguous error), counted under ice, NOT vanished.
        let mut inflight: HashMap<String, InflightClaim> =
            [("nc-ice".to_string(), h.clone().into())].into();
        let mut ts = DeleteTombstones::default();
        ts.stamp(ts_seed("nc-ice", ReapReason::Ice));
        ts.advance_fold_and_prune();
        let rec = DebuggingRecorder::new();
        let _g = ::metrics::set_default_local_recorder(&rec);
        let ice = detect_vanished(&mut inflight, &mut ts, &[]);
        assert_eq!(ice, vec![h.clone()], "Ice self-reap still masks");
        assert!(inflight.is_empty() && ts.is_empty());
        let counts = reap_counts(rec.snapshotter().snapshot().into_vec(), &h);
        assert_eq!(
            counts,
            vec![("ice".to_string(), 1)],
            "the original reason's counter, never vanished"
        );
    }

    /// Tombstone lifecycle on the CONSUMER FOLD CLOCK (R29,
    /// `ctrl.pool.fold-clock`): a DISCONFIRMED attempt (claim
    /// observed alive, not terminating) keeps the entry tracked AND
    /// the tombstone armed across [`TOMBSTONE_TTL_FOLDS`] consults —
    /// every one of which CONSULTED it before the clock moved (the
    /// consult-then-prune fusion makes pruned-unconsulted
    /// structurally unwritable); the fold after the grace expires it
    /// DISCLOSED (warn + per-reason counter).
    #[test]
    fn tombstones_expire_and_disconfirmation_keeps_them_armed() {
        let h = Cell("h".into(), CapacityType::Spot);
        let mut inflight: HashMap<String, InflightClaim> =
            [("nc-x".to_string(), h.clone().into())].into();
        let mut ts = DeleteTombstones::default();
        ts.stamp(ts_seed("nc-x", ReapReason::BootTimeout));

        // Disconfirmed: present ∧ !terminating → KEEP both, for
        // exactly TOMBSTONE_TTL_FOLDS folds (the first is the
        // same-window fold the R29′ gate keeps un-consumable; the
        // rest consult and disconfirm — the stamp is not aged by
        // clock time it was never consulted across, only by
        // executions of THIS fold).
        let mut alive = node("nc-x", "h", CapacityType::Spot, 8, 0, 0);
        alive.registered = false;
        for fold_n in 0..TOMBSTONE_TTL_FOLDS {
            let out = vanish_fold(&mut inflight, &mut ts, std::slice::from_ref(&alive));
            assert!(out.ice_cells.is_empty());
            assert!(
                inflight.contains_key("nc-x") && ts.contains("nc-x"),
                "disconfirmed entry survives consulting fold {fold_n}"
            );
        }

        // The fold AFTER the grace: still disconfirmed → the expiry
        // is DISCLOSED: warn + the per-reason expiry counter (the
        // typed disposition — never a silent prune).
        let rec = metrics_util::debugging::DebuggingRecorder::new();
        let _g = ::metrics::set_default_local_recorder(&rec);
        let out = vanish_fold(&mut inflight, &mut ts, std::slice::from_ref(&alive));
        assert!(out.ice_cells.is_empty());
        assert!(
            !ts.contains("nc-x"),
            "expired past the fold-denominated TTL"
        );
        // ppppp: snapshot exactly once.
        let expired = rec
            .snapshotter()
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(k, _, _, v)| {
                let key = k.key();
                (key.name() == "rio_controller_nodeclaim_tombstone_expired_total"
                    && key
                        .labels()
                        .any(|l| l.key() == "reason" && l.value() == "boot-timeout"))
                .then_some(v)
            });
        assert_eq!(
            expired,
            Some(metrics_util::debugging::DebugValue::Counter(1)),
            "expiry is a typed, disclosed disposition (the per-reason counter)"
        );
        drop(_g);
        let out = vanish_fold(&mut inflight, &mut ts, &[]);
        assert_eq!(
            out.ice_cells,
            vec![h],
            "post-expiry absence is Karpenter evidence"
        );
    }

    // r[verify ctrl.pool.delete-outcome]
    /// W12-AN — proposition: no committed delete's consequence packet
    /// is lost, per-exit TYPED; population: the 4 lane-by-mode stamp
    /// cells (health/idle × reconcile_once/consolidate_only — three
    /// stamp-before-fold, one stamp-after-fold; the freshness pins
    /// below cover both shapes).
    ///
    /// The merged_bug_050 trace: an in-flight tracked claim is
    /// ambiguous-reaped (non-404 error; the delete may have
    /// committed) and the same tick's fold consults a LIST fetched
    /// BEFORE the delete — which still shows the claim alive and
    /// REGISTERED (registration raced the reap). Pre-fix the
    /// RegisteredHandoff exit CONSUMED the tombstone on that
    /// disconfirmed-only evidence; when the delete's commit surfaced
    /// one LIST later the claim read DeliberateTeardown (registered
    /// teardown, no tombstone) and the consequence packet —
    /// reaped_total under the original reason, the Ice mask, the
    /// wedge eviction — was silently lost, with
    /// sweep_registered_tombstones KEEPING disconfirmed tombstones
    /// armed on identical evidence one population over. Post-fix the
    /// exit HANDS the tombstone to the registered-population sweep
    /// (typed disposition) and the next fold's fresh LIST confirms:
    /// the packet fires.
    #[test]
    fn w12_an_registered_handoff_hands_tombstone_to_sweep() {
        use super::super::ffd::tests::set_terminating;
        let h = Cell("h".into(), CapacityType::Spot);
        let mut inflight: HashMap<String, InflightClaim> =
            [("nc-race".to_string(), h.clone().into())].into();
        let mut ts = DeleteTombstones::default();
        // The ambiguous delete's full consequence packet (Ice reason
        // + the backing node for the wedge feed).
        ts.stamp(AmbiguousDelete {
            name: "nc-race".into(),
            reason: ReapReason::Ice,
            cell: h.clone(),
            node_name: Some("node-race".into()),
            idle_gap_secs: None,
        });

        // Fold 1 — the same tick's fold over the PRE-delete LIST:
        // the claim shows alive AND registered (the race).
        let mut reg = node("nc-race", "h", CapacityType::Spot, 8, 0, 0);
        reg.registered = true;
        let f1 = vanish_fold(&mut inflight, &mut ts, std::slice::from_ref(&reg));
        assert!(
            f1.ice_cells.is_empty() && f1.evicted_nodes.is_empty(),
            "no consequence on disconfirmed evidence"
        );
        assert!(
            !inflight.contains_key("nc-race"),
            "the handoff itself stands: observe_registered/FFD own the claim"
        );
        assert!(
            ts.contains("nc-race"),
            "the RegisteredHandoff exit HANDS the tombstone to the \
             registered-population sweep — it must not consume on \
             disconfirmed-only evidence (the sweep keeps identical \
             evidence armed one population over)"
        );

        // Fold 2 — the delete's commit surfaces on a fresh LIST
        // (terminating): the sweep confirms and the ORIGINAL
        // consequence packet fires whole.
        let mut term = node("nc-race", "h", CapacityType::Spot, 8, 0, 0);
        term.registered = true;
        let term = set_terminating(term);
        let f2 = vanish_fold(&mut inflight, &mut ts, std::slice::from_ref(&term));
        assert_eq!(
            f2.ice_cells,
            vec![h],
            "the Ice half of the consequence packet fires on confirm"
        );
        assert_eq!(
            f2.evicted_nodes,
            vec!["node-race".to_string()],
            "the wedge-eviction half fires on confirm"
        );
        assert!(!ts.contains("nc-race"), "confirmed ⇒ consumed");
    }

    /// The R29′ freshness gate on tombstone consumption
    /// (`ctrl.pool.delete-outcome`): a stamp is CONSUMABLE only by a
    /// fold whose LIST post-dates it — the fold clock is the
    /// denominator. Three of the four lane-by-mode stamp cells are
    /// stamp-BEFORE-fold (health × both modes, idle ×
    /// consolidate_only): their same-tick fold consults a pre-delete
    /// LIST that structurally cannot carry the delete's evidence.
    /// The gate makes that consult a no-op for the provenance axis:
    /// same-fold = not yet consultable; the NEXT fold adjudicates.
    /// (The fourth cell — idle × reconcile_once, stamp-after-fold —
    /// pays one conservative extra fold: the safe direction, grace
    /// only lengthens.)
    #[test]
    fn same_tick_stamp_is_not_consumable_by_its_own_fold() {
        use super::super::ffd::tests::set_terminating;
        let h = Cell("h".into(), CapacityType::Spot);
        let mut inflight: HashMap<String, InflightClaim> = HashMap::new();
        let mut ts = DeleteTombstones::default();
        ts.stamp(AmbiguousDelete {
            name: "nc-fresh".into(),
            reason: ReapReason::Dead,
            cell: h.clone(),
            node_name: Some("node-f".into()),
            idle_gap_secs: None,
        });

        // The same tick's fold: the claim shows terminating on the
        // PRE-delete LIST (it was already dying before our delete —
        // foreign teardown, or our commit racing ahead). The stamp
        // is NOT consultable: consuming here would attribute a
        // pre-stamp observation to the stamp's delete.
        let mut term = node("nc-fresh", "h", CapacityType::Spot, 8, 0, 0);
        term.registered = true;
        let term = set_terminating(term);
        let f1 = vanish_fold(&mut inflight, &mut ts, std::slice::from_ref(&term));
        assert!(
            f1.evicted_nodes.is_empty(),
            "a same-tick stamp is not consumable by its own fold \
             (the LIST pre-dates the delete)"
        );
        assert!(ts.contains("nc-fresh"), "kept armed for the next fold");

        // The next fold's LIST post-dates the stamp: consumable —
        // confirm fires the packet.
        let f2 = vanish_fold(&mut inflight, &mut ts, std::slice::from_ref(&term));
        assert_eq!(
            f2.evicted_nodes,
            vec!["node-f".to_string()],
            "first post-stamp fold adjudicates and confirms"
        );
        assert!(!ts.contains("nc-fresh"));
    }

    /// W11-AG unit face — the tombstone consumer census (R25: every
    /// tombstone is consumed by an arm applying its reason's full
    /// consequence, or expires disclosed — never a silent prune), as
    /// a product walk over (fold-population × observation × reason):
    ///
    /// - name IN `inflight_created` → the sweep does NOT touch it
    ///   (the vanish fold owns that population — one consumer per
    ///   tombstone, partitioned by population);
    /// - name OUTSIDE, observed TERMINATING or ABSENT → CONFIRMED:
    ///   consumed, with the reason's counter, the carried wedge
    ///   eviction, the censored gap iff the packet carries one, and
    ///   the mask iff the reason is `Ice`;
    /// - name OUTSIDE, observed alive non-terminating → DISCONFIRMED:
    ///   kept armed, zero consequence.
    ///
    /// The reason axis derives from `ReapReason::LETTERS` (the pinned
    /// alphabet) so a new reason letter joins this census or fails to
    /// compile out of `LETTERS`.
    // r[verify ctrl.pool.delete-outcome]
    #[test]
    fn registered_tombstone_sweep_census_over_the_exit_product() {
        use super::super::ffd::tests::set_terminating;
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        #[derive(Debug, Clone, Copy, PartialEq)]
        enum Obs {
            Absent,
            Terminating,
            Alive,
        }
        let h = Cell("h".into(), CapacityType::Spot);
        for in_fold_population in [false, true] {
            for obs in [Obs::Absent, Obs::Terminating, Obs::Alive] {
                for reason in ReapReason::LETTERS {
                    let row = (in_fold_population, obs, reason);
                    let mut ts = DeleteTombstones::default();
                    ts.stamp(AmbiguousDelete {
                        name: "nc-x".into(),
                        reason,
                        cell: h.clone(),
                        node_name: Some("node-x".into()),
                        idle_gap_secs: (reason == ReapReason::Idle).then_some(7.5),
                    });
                    // R29′ freshness: consultable from the next fold.
                    ts.advance_fold_and_prune();
                    let inflight: HashMap<String, InflightClaim> = if in_fold_population {
                        [("nc-x".to_string(), h.clone().into())].into()
                    } else {
                        HashMap::new()
                    };
                    let live = match obs {
                        Obs::Absent => vec![],
                        Obs::Terminating => {
                            let mut n = node("nc-x", "h", CapacityType::Spot, 8, 0, 0);
                            n.registered = true;
                            vec![set_terminating(n)]
                        }
                        Obs::Alive => {
                            let mut n = node("nc-x", "h", CapacityType::Spot, 8, 0, 0);
                            n.registered = true;
                            vec![n]
                        }
                    };
                    let rec = DebuggingRecorder::new();
                    let _g = ::metrics::set_default_local_recorder(&rec);
                    let swept = sweep_registered_tombstones(&mut ts, &inflight, &live);
                    drop(_g);
                    let confirmed =
                        !in_fold_population && matches!(obs, Obs::Absent | Obs::Terminating);
                    assert_eq!(
                        ts.contains("nc-x"),
                        !confirmed,
                        "consumed iff confirmed for {row:?}"
                    );
                    assert_eq!(
                        swept.confirmed,
                        if confirmed {
                            vec!["nc-x".to_string()]
                        } else {
                            vec![]
                        },
                        "confirm list for {row:?}"
                    );
                    assert_eq!(
                        swept.evicted_nodes,
                        if confirmed {
                            vec!["node-x".to_string()]
                        } else {
                            vec![]
                        },
                        "the wedge-eviction half fires iff confirmed for {row:?}"
                    );
                    assert_eq!(
                        swept.censored_gaps,
                        if confirmed && reason == ReapReason::Idle {
                            vec![(h.clone(), 7.5)]
                        } else {
                            vec![]
                        },
                        "the censored-gap half rides the packet for {row:?}"
                    );
                    assert_eq!(
                        !swept.ice_cells.is_empty(),
                        confirmed && reason == ReapReason::Ice,
                        "mask iff Ice (alphabet-total arm) for {row:?}"
                    );
                    // ppppp: snapshot exactly once.
                    let counted = rec
                        .snapshotter()
                        .snapshot()
                        .into_vec()
                        .into_iter()
                        .find_map(|(k, _, _, v)| {
                            let key = k.key();
                            (key.name() == "rio_controller_nodeclaim_reaped_total"
                                && key
                                    .labels()
                                    .any(|l| l.key() == "reason" && l.value() == reason.as_str()))
                            .then_some(v)
                        });
                    assert_eq!(
                        counted,
                        confirmed.then_some(DebugValue::Counter(1)),
                        "the original reason's counter fires iff confirmed for {row:?}"
                    );
                }
            }
        }
    }

    /// R15 vanish-path census, widened by bug_094 (R22: the pre-fix
    /// census was enrolled and GREEN over a product missing the
    /// deciding axes): `classify_vanish` product-iterated over
    /// (present × registered × terminating × launched × provenance ×
    /// ever_registered) — 240 cells, every axis value FROM the
    /// alphabet (`Option<bool>` is `launched()`'s full range; the
    /// provenance axis derives from `ReapReason::LETTERS`, the pinned
    /// closed enum — bug_112 widened it with `Idle`; `ever_registered`
    /// is the sh-030 controller-observation axis). Each row asserts
    /// its class AND its mark/tracking/tombstone effect through the
    /// production `detect_vanished` fold. Generator: the loop product
    /// + rustc exhaustiveness at the law match.
    #[test]
    fn vanish_class_census_over_the_observation_product() {
        use super::super::ffd::tests::set_terminating;
        let h = Cell("h".into(), CapacityType::Spot);
        for present in [false, true] {
            for registered in [false, true] {
                for terminating in [false, true] {
                    for launched in [None, Some(false), Some(true)] {
                        for ever_registered in [false, true] {
                            for tombstoned in std::iter::once(None)
                                .chain(ReapReason::LETTERS.into_iter().map(Some))
                            {
                                let n = match launched {
                                    None => node("nc-x", "h", CapacityType::Spot, 8, 0, 0),
                                    Some(l) => with_conds(
                                        node("nc-x", "h", CapacityType::Spot, 8, 0, 0),
                                        &[("Launched", if l { "True" } else { "False" }, 1001.0)],
                                    ),
                                };
                                let mut n = n;
                                n.registered = registered;
                                let n = if terminating { set_terminating(n) } else { n };
                                let row = (
                                    present,
                                    registered,
                                    terminating,
                                    launched,
                                    tombstoned,
                                    ever_registered,
                                );
                                // The law table, stated independently
                                // of the production match (two
                                // derivations of one law; rustc
                                // exhaustiveness on both).
                                let want = match row {
                                    (false, _, _, _, Some(r), _) => Some(VanishClass::SelfReap(r)),
                                    (false, _, _, _, None, true) => {
                                        Some(VanishClass::EmptyConsolidation)
                                    }
                                    (false, _, _, _, None, false) => Some(VanishClass::GcVanish),
                                    (true, _, true, _, Some(r), _) => {
                                        Some(VanishClass::SelfReap(r))
                                    }
                                    (true, true, true, _, None, _) => {
                                        Some(VanishClass::DeliberateTeardown)
                                    }
                                    (true, true, false, _, _, _) => {
                                        Some(VanishClass::RegisteredHandoff)
                                    }
                                    (true, false, true, Some(true), None, _) => {
                                        Some(VanishClass::BootFailureTeardown)
                                    }
                                    (true, false, true, _, None, _) => {
                                        Some(VanishClass::LaunchFailureTeardown)
                                    }
                                    (true, false, false, _, _, _) => None,
                                };
                                let got = classify_vanish(
                                    present.then_some(&n),
                                    tombstoned,
                                    ever_registered,
                                );
                                assert_eq!(got, want, "class for {row:?}");
                                // Effect row through the production
                                // fold (the pre-retain latch sets
                                // ever_registered |= n.registered when
                                // present, so the InflightClaim seed
                                // and the latched value coincide on
                                // the (present=true, registered=true)
                                // rows — and the seed is the only
                                // input on present=false rows):
                                let mut inflight: HashMap<String, InflightClaim> = [(
                                    "nc-x".to_string(),
                                    InflightClaim {
                                        cell: h.clone(),
                                        ever_registered,
                                    },
                                )]
                                .into();
                                let mut ts = DeleteTombstones::default();
                                if let Some(r) = tombstoned {
                                    ts.stamp(ts_seed("nc-x", r));
                                    // R29′: consultable from the next fold.
                                    ts.advance_fold_and_prune();
                                }
                                let live = if present { vec![n] } else { vec![] };
                                let ice = detect_vanished(&mut inflight, &mut ts, &live);
                                let marks = matches!(
                                    want,
                                    Some(
                                        VanishClass::LaunchFailureTeardown
                                            | VanishClass::GcVanish
                                            | VanishClass::SelfReap(ReapReason::Ice)
                                    )
                                );
                                assert_eq!(!ice.is_empty(), marks, "mark effect for {row:?}");
                                assert_eq!(
                                    inflight.contains_key("nc-x"),
                                    want.is_none(),
                                    "tracking effect for {row:?}"
                                );
                                // Tombstone disposition (merged_bug_050,
                                // R32 — typed per exit): only the exit
                                // that FIRED the packet (SelfReap)
                                // consumes; every other exit hands the
                                // tombstone to the registered-population
                                // sweep, and a KEEP retains it.
                                assert_eq!(
                                    ts.contains("nc-x"),
                                    tombstoned.is_some()
                                        && !matches!(want, Some(VanishClass::SelfReap(_))),
                                    "tombstone effect for {row:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// R22 planted red (axis-omission corpus row, in-file fixture): a
    /// strawman census generator over the PRE-FIX universe (present ×
    /// registered × terminating — the wave-8 product) cannot be total
    /// over the law: rows identical in that projection classify
    /// DIFFERENTLY along each added axis, so a generator that drops
    /// either axis certifies one census row for two distinct letters
    /// — exactly the enrolled-but-wrong-universe shape bug_094
    /// shipped. If a future edit collapses either axis out of
    /// `classify_vanish`, one of these inequalities goes red.
    #[test]
    fn vanish_census_axis_omission_red_fixture() {
        use super::super::ffd::tests::set_terminating;
        let mk = |launched: Option<bool>| {
            let n = match launched {
                None => node("nc-x", "h", CapacityType::Spot, 8, 0, 0),
                Some(l) => with_conds(
                    node("nc-x", "h", CapacityType::Spot, 8, 0, 0),
                    &[("Launched", if l { "True" } else { "False" }, 1001.0)],
                ),
            };
            let mut n = n;
            n.registered = false;
            set_terminating(n)
        };
        // Same (present=true, registered=false, terminating=true)
        // projection — the launched axis decides boot vs capacity:
        assert_ne!(
            classify_vanish(Some(&mk(Some(true))), None, false),
            classify_vanish(Some(&mk(Some(false))), None, false),
            "a generator without the launched axis maps two letters to one row"
        );
        // — and the provenance axis decides ours vs Karpenter's:
        assert_ne!(
            classify_vanish(Some(&mk(None)), Some(ReapReason::BootTimeout), false),
            classify_vanish(Some(&mk(None)), None, false),
            "a generator without the provenance axis maps two letters to one row"
        );
        // The absent projection splits on provenance too:
        assert_ne!(
            classify_vanish(None, Some(ReapReason::Dead), false),
            classify_vanish(None, None, false),
            "absence is not always GcVanish — provenance decides"
        );
        // sh-030: the absent ∧ no-provenance projection splits on the
        // controller's own observation history — ever_registered=true
        // is empty consolidation (no mask), false is the B11 fast-GC
        // capacity row (mask):
        assert_ne!(
            classify_vanish(None, None, true),
            classify_vanish(None, None, false),
            "a generator without the ever_registered axis maps two letters to one row"
        );
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

    /// **sh-043** — `Launched=Unknown` past `ice_timeout` → ICE
    /// (Karpenter throttle/backlog parked the claim at
    /// `InitializeConditions`'s pre-launch `Unknown` posture; the
    /// same capacity-unproven state as `False`/absent — closes the
    /// (b) escape at `:375`). Pre-fix RED (transcript in the commit
    /// body): the `Some(_)` arm fell through and the claim parked
    /// indefinitely in the `Synced()`-blocking set. Per-cell
    /// `ice_timeout = 2×seed_for(cell)`: 32-42s builder, 60s
    /// fetcher, **1200s metal** — NOT a uniform 60s floor.
    #[test]
    fn classify_reaps_launched_unknown_past_timeout() {
        let cfg = cfg_seeded("h", 45.0);
        let sk = CellSketches::default();
        // created=1000, now=1100 → age=100 > 2×45=90. Launched=Unknown.
        let mut stuck = with_conds(
            node("stuck", "h", CapacityType::Spot, 8, 0, 0),
            &[("Launched", "Unknown", 1000.0)],
        );
        stuck.registered = false;
        let r = classify(&[stuck.clone()], &HashSet::new(), &sk, &cfg, 1100.0);
        assert_eq!(
            r,
            vec![(0, ReapReason::Ice)],
            "sh-043: Launched=Unknown past ice_timeout reaps as Ice \
             (the same capacity-unproven posture as False/absent; \
             pre-fix RED: the Some(_) arm fell through — out == [])"
        );
        // Under timeout: now=1080 → age=80 < 90 → healthy (the age
        // gate still applies; this is NOT a no-timeout short-circuit).
        let r2 = classify(&[stuck], &HashSet::new(), &sk, &cfg, 1080.0);
        assert!(r2.is_empty(), "under ice_timeout: not yet reaped");
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

    /// The production half of one embedded source: everything before
    /// the first `#[cfg(test)]`-gated module BODY (`mod … {`). A
    /// `#[cfg(test)] mod x;` DECLARATION is not a split point —
    /// mod.rs declares `lifecycle_tests` that way near the top, and
    /// splitting there would silently blank the whole reconciler out
    /// of the census (the merged_bug_018 negative-extent shape).
    fn census_prod_half(src: &str) -> String {
        let lines: Vec<&str> = src.lines().collect();
        for i in 0..lines.len() {
            if lines[i].trim() == "#[cfg(test)]"
                && i + 1 < lines.len()
                && lines[i + 1].contains("mod ")
                && lines[i + 1].trim_end().ends_with('{')
            {
                return lines[..i].join("\n");
            }
        }
        lines.join("\n")
    }

    /// The strawman detector (W11-AH): every `.delete(` call in the
    /// production half must be routed through
    /// `DeleteOutcome::classify(` within the 3 lines at/above the
    /// call — a lane matching the raw kube `Result` (the bug_112
    /// shape: arms re-derived per site, one discharged) is returned
    /// as a violation with its 1-based line. Fail-closed: the scan is
    /// a total line walk over the production half — there is no skip
    /// arm, no unparseable-context exemption.
    fn unclassified_delete_lanes(src: &str) -> Vec<usize> {
        let prod = census_prod_half(src);
        let lines: Vec<&str> = prod.lines().collect();
        let mut out = Vec::new();
        for (i, l) in lines.iter().enumerate() {
            if l.contains(".delete(") {
                let lo = i.saturating_sub(3);
                let routed = lines[lo..=i]
                    .iter()
                    .any(|w| w.contains("DeleteOutcome::classify("));
                if !routed {
                    out.push(i + 1);
                }
            }
        }
        out
    }

    /// W11-AH — the delete-lane census ([GEN-SET]; the R28 partition
    /// axis of `ctrl.pool.delete-outcome`): the law's population is
    /// all reap lanes, machine-derived two ways and pinned
    /// bidirectionally per (wwwww):
    ///
    /// 1. **Universe**: the embedded source set IS mod.rs's own
    ///    module-declaration grammar (non-`#[cfg(test)]` `mod x;`
    ///    rows) — adding a submodule without enrolling its source
    ///    here is a red, so a NEW delete lane cannot hide in an
    ///    unembedded file.
    /// 2. **Lanes**: every `.delete(` site in every production half
    ///    routes through `DeleteOutcome::classify` (the sole
    ///    translator); the total lane count is pinned so a new lane
    ///    is a conscious census row, not a silent sibling.
    // r[verify ctrl.pool.delete-outcome]
    #[test]
    fn delete_lane_census() {
        const MOD_SRC: &str = include_str!("mod.rs");
        const SOURCES: &[(&str, &str)] = &[
            ("consolidate", include_str!("consolidate.rs")),
            ("cover", include_str!("cover.rs")),
            ("evidence", include_str!("evidence.rs")),
            ("ffd", include_str!("ffd.rs")),
            ("health", include_str!("health.rs")),
            ("pods", include_str!("pods.rs")),
            ("sketch", include_str!("sketch.rs")),
            ("wedge", include_str!("wedge.rs")),
        ];
        // (1) Universe: derived from mod.rs's declaration grammar.
        let mod_lines: Vec<&str> = MOD_SRC.lines().collect();
        let mut declared: Vec<String> = Vec::new();
        for (i, l) in mod_lines.iter().enumerate() {
            let t = l.trim();
            let decl = t
                .strip_prefix("pub(crate) mod ")
                .or_else(|| t.strip_prefix("pub mod "))
                .or_else(|| t.strip_prefix("mod "));
            if let Some(rest) = decl
                && let Some(name) = rest.strip_suffix(';')
            {
                let test_gated = i > 0 && mod_lines[i - 1].trim() == "#[cfg(test)]";
                if !test_gated {
                    declared.push(name.to_string());
                }
            }
        }
        declared.sort();
        let mut embedded: Vec<&str> = SOURCES.iter().map(|(n, _)| *n).collect();
        embedded.sort();
        assert_eq!(
            declared, embedded,
            "census universe == mod.rs's module list (enroll the new module's \
             source in SOURCES, then classify its delete lanes)"
        );
        // (2) Lanes: zero unclassified delete sites anywhere, and the
        // lane count pinned (health::reap_unhealthy +
        // consolidate::reap_idle — the two lanes of the eviction-
        // source law).
        let mut lanes = 0usize;
        for (name, src) in SOURCES.iter().chain([&("mod", MOD_SRC)]) {
            let bad = unclassified_delete_lanes(src);
            assert!(
                bad.is_empty(),
                "{name}.rs has raw-Result delete lanes at production lines {bad:?} \
                 — route through DeleteOutcome::classify (ctrl.pool.delete-outcome)"
            );
            lanes += census_prod_half(src)
                .lines()
                .filter(|l| l.contains(".delete("))
                .count();
        }
        assert_eq!(
            lanes, 2,
            "delete-lane population drifted — a new reap lane must take \
             its own DeleteOutcome census row (and its consequence arms)"
        );
    }

    /// W11-AH planted reds — one plant per leniency point in the
    /// detector's OWN control flow (R22″): the corpus strings are
    /// driven through the same detector that polices production.
    #[test]
    fn delete_lane_census_plants() {
        // (a) The strawman raw-Result lane (the bug_112 shape) FIRES.
        let raw_lane = "fn lane() {\n    match nodeclaims.delete(&n.name, &dp).await {\n        Ok(_) => {}\n        Err(e) => {}\n    }\n}";
        assert_eq!(
            unclassified_delete_lanes(raw_lane),
            vec![2],
            "planted red: the raw-Result lane must be detected"
        );
        // (b) The radius edge: classify FOUR lines above the call is
        // outside the 3-line co-location window — still a violation
        // (the window is the enforced idiom, not a suggestion).
        let out_of_radius =
            "let o = DeleteOutcome::classify(x);\n// 1\n// 2\n// 3\nlet r = api.delete(&n).await;";
        assert_eq!(
            unclassified_delete_lanes(out_of_radius),
            vec![5],
            "planted red: out-of-radius classify does not launder the lane"
        );
        // (c) Green twins — both production idioms pass: same-line
        // (health) and call-wrapped-next-line (consolidate).
        let same_line = "let o = DeleteOutcome::classify(api.delete(&n, &dp).await);";
        let next_line =
            "let o = health::DeleteOutcome::classify(\n    api.delete(&n, &dp).await,\n);";
        assert!(unclassified_delete_lanes(same_line).is_empty());
        assert!(unclassified_delete_lanes(next_line).is_empty());
        // (d) The prod-split leniency, BOTH sides: a raw lane in the
        // production half fires even when a test module follows; the
        // same lane inside the cfg(test) BODY is exempt; and a
        // cfg(test) mod DECLARATION does not blank what follows it.
        let split_both_sides = "fn lane() {\n    api.delete(&n).await;\n}\n#[cfg(test)]\nmod tests {\n    fn t() { api.delete(&n).await; }\n}";
        assert_eq!(
            unclassified_delete_lanes(split_both_sides),
            vec![2],
            "planted red: prod half fires; cfg(test) body exempt"
        );
        let decl_not_split =
            "#[cfg(test)]\nmod lifecycle_tests;\nfn lane() {\n    api.delete(&n).await;\n}";
        assert_eq!(
            unclassified_delete_lanes(decl_not_split),
            vec![4],
            "planted red: a cfg(test) mod DECLARATION must not blank the file"
        );
    }
}
