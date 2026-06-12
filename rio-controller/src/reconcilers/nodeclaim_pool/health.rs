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
    pub(super) const ALL: [ReapReason; 4] = [Self::Ice, Self::BootTimeout, Self::Dead, Self::Idle];
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
    /// claim WAS reaped. The SAME full consequence as [`Self::OkDeleted`]
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
            Some(_) => {}
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
/// - **GcVanish** (absent from `live`, no tombstone): vanished without
///   ever Registering. Karpenter GC'd it (the controller's own
///   COMPLETED reaps are removed from `inflight` by the caller before
///   this runs; its AMBIGUOUS ones carry tombstones) ⇒ the cell is
///   unfulfillable. ICE-mask + `reaped_total{reason=vanished}`. The
///   `Launched` axis is unreadable here (the object is gone) — absent
///   stays capacity-side by construction: the fast (~1s) GC that
///   evades the terminating observation is exactly the
///   `Launched=False LaunchFailed` path, while a `Launched=True`
///   teardown rides a ~60–90s finalizer and is observed terminating
///   across multiple 10s ticks (the BootFailureTeardown row).
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
pub fn detect_vanished(
    inflight: &mut HashMap<String, Cell>,
    tombstones: &mut DeleteTombstones,
    live: &[LiveNode],
) -> Vec<Cell> {
    let live_by_name: HashMap<&str, &LiveNode> =
        live.iter().map(|n| (n.name.as_str(), n)).collect();
    let mut ice = Vec::new();
    inflight.retain(|name, cell| {
        let observed = live_by_name.get(name.as_str()).copied();
        let Some(class) = classify_vanish(observed, tombstones.reason(name)) else {
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
        // r[impl ctrl.nodeclaim.ice-mark-clear+4]
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
        }
        // Every exit consumes the name's provenance — the tracking
        // story is over; a later same-name claim is fresh evidence.
        tombstones.remove(name);
        false
    });
    ice
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
    /// Absent from `live` (no tombstone): GC'd between ticks without
    /// Registering — capacity-side by construction (see the
    /// [`detect_vanished`] GcVanish row for why `Launched` is
    /// unreadable AND immaterial here).
    GcVanish,
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
) -> Option<VanishClass> {
    match (observed, self_delete) {
        // Provenance rows first: a tombstoned claim observed
        // terminating or absent is the controller's own delete
        // confirmed — regardless of registered/Launched (the original
        // classification already adjudicated those).
        (None, Some(r)) => Some(VanishClass::SelfReap(r)),
        (Some(n), Some(r)) if n.terminating() => Some(VanishClass::SelfReap(r)),
        // Observation rows (tombstone absent, or present but
        // DISCONFIRMED — the claim is alive and not terminating, so
        // the attempted delete provably has not committed yet).
        (Some(n), _) if n.registered && n.terminating() => Some(VanishClass::DeliberateTeardown),
        (Some(n), _) if n.registered => Some(VanishClass::RegisteredHandoff),
        (Some(n), _) if n.terminating() => match n.launched() {
            Some(true) => Some(VanishClass::BootFailureTeardown),
            _ => Some(VanishClass::LaunchFailureTeardown),
        },
        (Some(_), _) => None,
        (None, None) => Some(VanishClass::GcVanish),
    }
}

/// How many leader ticks an unconfirmed delete tombstone survives.
/// Violable envelope (R17), derivation: confirmation normally lands at
/// the NEXT tick's LIST (a committed delete shows `deletionTimestamp`
/// immediately; the ~60–90s finalizer keeps it observable for many
/// 10s ticks), so 3 covers one stale-LIST tick plus one missed tick —
/// while bounding the window in which an INDEPENDENT same-name
/// teardown could be mis-attributed to this controller (that
/// suppression additionally requires the claim to flip into a
/// capacity-failure shape after our non-ICE classification — near-
/// contradictory, since Karpenter never transitions `Launched`
/// True→False; an `Ice`-reason tombstone's consequence is the mask
/// either way, so nothing is suppressed on that arm).
const TOMBSTONE_TTL_TICKS: u64 = 3;

/// One stored ambiguous delete attempt: the full consequence packet
/// the reap lane produced ([`AmbiguousDelete`]) plus the leader tick
/// that stamped it.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteAttempt {
    pub seed: AmbiguousDelete,
    pub tick: u64,
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
/// 4. [`prune_expired`](Self::prune_expired) — DISCONFIRMED entries
///    older than [`TOMBSTONE_TTL_TICKS`] drop as a typed, DISCLOSED
///    disposition (warn + expiry counter), never a silent prune.
/// 5. `clear()` on the ACQUISITION EDGE (suppress polarity, the
///    `inflight_created` row's rationale: a stale previous-tenure
///    tombstone could suppress a genuine vanish ICE of a same-named
///    successor claim).
#[derive(Debug, Default)]
pub struct DeleteTombstones {
    entries: HashMap<String, DeleteAttempt>,
}

impl DeleteTombstones {
    /// Record an ambiguous delete attempt at `tick` (re-stamping an
    /// existing name refreshes its TTL and consequence packet — a
    /// repeated `Err` is a fresh ambiguous attempt).
    pub fn stamp(&mut self, seed: AmbiguousDelete, tick: u64) {
        self.entries
            .insert(seed.name.clone(), DeleteAttempt { seed, tick });
    }

    /// The tombstoned reason for `name`, if any — the provenance axis
    /// [`classify_vanish`] consumes.
    pub fn reason(&self, name: &str) -> Option<ReapReason> {
        self.entries.get(name).map(|a| a.seed.reason)
    }

    /// Consume `name`'s tombstone (exit observed or retry completed).
    pub fn remove(&mut self, name: &str) {
        self.entries.remove(name);
    }

    /// Drop entries stamped more than [`TOMBSTONE_TTL_TICKS`] leader
    /// ticks ago. `wrapping_sub` matches the tick counter's wrap.
    ///
    /// Expiry is a TYPED, DISCLOSED disposition, never a silent prune
    /// (`ctrl.pool.delete-outcome`, bug_042): with the vanish fold
    /// consuming every in-flight exit and
    /// [`sweep_registered_tombstones`] consuming every confirmed
    /// registered exit each tick, an entry can only reach expiry
    /// DISCONFIRMED — its claim was observed alive (the delete
    /// provably had not committed) or was never re-observed at all.
    /// Each drop warns with the attempt's reason and increments
    /// `rio_controller_nodeclaim_tombstone_expired_total{reason}`.
    pub fn prune_expired(&mut self, now_tick: u64) {
        self.entries.retain(|name, a| {
            let keep = now_tick.wrapping_sub(a.tick) <= TOMBSTONE_TTL_TICKS;
            if !keep {
                warn!(
                    %name, reason = a.seed.reason.as_str(),
                    "delete tombstone expired unconfirmed (disconfirmed or never \
                     re-observed); dropping provenance — a later same-name \
                     teardown is foreign evidence again"
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
/// applies the original reason's FULL consequence through the SAME
/// code path as the prompt arms ([`record_reap`]: counter under the
/// original reason, mask iff `Ice`) plus the carried halves (the
/// wedge eviction via the packet's `node_name`, the censored idle
/// gap), and consumes the tombstone. A claim observed alive and not
/// terminating keeps its tombstone armed (DISCONFIRMED so far — the
/// reap lane retries while its conditions persist, and a completed
/// retry consumes via the callers' `reaped_claims` loop).
///
/// Mis-attribution bound: within [`TOMBSTONE_TTL_TICKS`] of OUR
/// delete attempt, an independent foreign teardown of the same claim
/// would be counted as ours — the same bounded suppression window the
/// TTL derivation prices for the vanish fold, and the consequence is
/// safe under it (a terminating node's evidence is evicted exactly
/// like fleet absence would evict it one window later).
// r[impl ctrl.pool.delete-outcome]
pub fn sweep_registered_tombstones(
    tombstones: &mut DeleteTombstones,
    inflight: &HashMap<String, Cell>,
    live: &[LiveNode],
) -> TombstoneSweep {
    let live_by_name: HashMap<&str, &LiveNode> =
        live.iter().map(|n| (n.name.as_str(), n)).collect();
    let mut out = TombstoneSweep::default();
    tombstones.entries.retain(|name, a| {
        if inflight.contains_key(name) {
            // The vanish fold's population — its exits consume these
            // (one consumer per tombstone, partitioned by population).
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

    /// `ReapReason::ALL` is total over the alphabet — rustc
    /// exhaustiveness pins the index map, so a new variant cannot
    /// compile without joining `ALL` (and thereby every reason-indexed
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
        let mut seen = [false; ReapReason::ALL.len()];
        for r in ReapReason::ALL {
            assert!(!seen[idx(r)], "duplicate {r:?} in ALL");
            seen[idx(r)] = true;
        }
        assert!(seen.iter().all(|s| *s), "ALL covers every variant");
    }

    /// The [`DeleteOutcome::classify`] partition, walked over the
    /// reachable result product (Ok + every distinct kube error
    /// shape): total — every input lands in exactly one arm — and 404
    /// is the ONLY error that counts as a completed reap. This is the
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
        let mut inflight: HashMap<String, Cell> = [
            ("nc-gone".into(), h.clone()),
            ("nc-inflight".into(), h.clone()),
            ("nc-reg".into(), h.clone()),
            ("nc-term".into(), h.clone()),
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

    // r[verify ctrl.nodeclaim.ice-mark-clear+4]
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
        let mut inflight: HashMap<String, Cell> = [("nc-doomed".to_string(), h.clone())].into();
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
        let mut inflight: HashMap<String, Cell> = [("nc-reg-term".to_string(), h.clone())].into();
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
        let mut inflight: HashMap<String, Cell> = [("nc-ttl".to_string(), h.clone())].into();
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
        let mut inflight: HashMap<String, Cell> = [("nc-bt".to_string(), h.clone())].into();
        let mut ts = DeleteTombstones::default();
        ts.stamp(ts_seed("nc-bt", ReapReason::BootTimeout), 7);
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
        let mut inflight: HashMap<String, Cell> = [("nc-ice".to_string(), h.clone())].into();
        let mut ts = DeleteTombstones::default();
        ts.stamp(ts_seed("nc-ice", ReapReason::Ice), 7);
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

    /// Tombstone lifecycle: a DISCONFIRMED attempt (claim observed
    /// alive, not terminating) keeps the entry tracked AND the
    /// tombstone armed (stale-LIST tolerance); `prune_expired` drops
    /// it after [`TOMBSTONE_TTL_TICKS`]; a fresh stamp survives its
    /// own tick's prune.
    #[test]
    fn tombstones_expire_and_disconfirmation_keeps_them_armed() {
        let h = Cell("h".into(), CapacityType::Spot);
        let mut inflight: HashMap<String, Cell> = [("nc-x".to_string(), h.clone())].into();
        let mut ts = DeleteTombstones::default();
        ts.stamp(ts_seed("nc-x", ReapReason::BootTimeout), 10);
        ts.prune_expired(10);
        assert!(ts.contains("nc-x"), "fresh stamp survives its own tick");

        // Disconfirmed: present ∧ !terminating → KEEP both.
        let mut alive = node("nc-x", "h", CapacityType::Spot, 8, 0, 0);
        alive.registered = false;
        assert!(detect_vanished(&mut inflight, &mut ts, &[alive]).is_empty());
        assert!(inflight.contains_key("nc-x") && ts.contains("nc-x"));

        // Expiry: TTL ticks later the tombstone is gone; a subsequent
        // absence is foreign evidence again (GcVanish → mask). The
        // expiry is DISCLOSED: warn + the per-reason expiry counter
        // (the typed disposition — never a silent prune).
        let rec = metrics_util::debugging::DebuggingRecorder::new();
        let _g = ::metrics::set_default_local_recorder(&rec);
        ts.prune_expired(10 + TOMBSTONE_TTL_TICKS + 1);
        assert!(!ts.contains("nc-x"), "expired past TTL");
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
        let ice = detect_vanished(&mut inflight, &mut ts, &[]);
        assert_eq!(ice, vec![h], "post-expiry absence is Karpenter evidence");
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
    ///   consumed + the reason's counter + the carried wedge eviction
    ///   + the censored gap iff the packet carries one + mask iff the
    ///   reason is `Ice`;
    /// - name OUTSIDE, observed alive non-terminating → DISCONFIRMED:
    ///   kept armed, zero consequence.
    ///
    /// The reason axis derives from `ReapReason::ALL` (the pinned
    /// alphabet) so a new reason letter joins this census or fails to
    /// compile out of `ALL`.
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
                for reason in ReapReason::ALL {
                    let row = (in_fold_population, obs, reason);
                    let mut ts = DeleteTombstones::default();
                    ts.stamp(
                        AmbiguousDelete {
                            name: "nc-x".into(),
                            reason,
                            cell: h.clone(),
                            node_name: Some("node-x".into()),
                            idle_gap_secs: (reason == ReapReason::Idle).then_some(7.5),
                        },
                        1,
                    );
                    let inflight: HashMap<String, Cell> = if in_fold_population {
                        [("nc-x".to_string(), h.clone())].into()
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
    /// (present × registered × terminating × launched × provenance) —
    /// 120 cells, every axis value FROM the alphabet (`Option<bool>`
    /// is `launched()`'s full range; the provenance axis derives from
    /// `ReapReason::ALL`, the pinned closed enum — bug_112 widened it
    /// with `Idle`). Each row
    /// asserts its class AND its mark/tracking/tombstone effect
    /// through the production `detect_vanished` fold. Generator: the
    /// loop product + rustc exhaustiveness at the law match.
    #[test]
    fn vanish_class_census_over_the_observation_product() {
        use super::super::ffd::tests::set_terminating;
        let h = Cell("h".into(), CapacityType::Spot);
        for present in [false, true] {
            for registered in [false, true] {
                for terminating in [false, true] {
                    for launched in [None, Some(false), Some(true)] {
                        for tombstoned in
                            std::iter::once(None).chain(ReapReason::ALL.into_iter().map(Some))
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
                            let row = (present, registered, terminating, launched, tombstoned);
                            // The law table, stated independently of
                            // the production match (two derivations of
                            // one law; rustc exhaustiveness on both).
                            let want = match row {
                                (false, _, _, _, Some(r)) => Some(VanishClass::SelfReap(r)),
                                (false, _, _, _, None) => Some(VanishClass::GcVanish),
                                (true, _, true, _, Some(r)) => Some(VanishClass::SelfReap(r)),
                                (true, true, true, _, None) => {
                                    Some(VanishClass::DeliberateTeardown)
                                }
                                (true, true, false, _, _) => Some(VanishClass::RegisteredHandoff),
                                (true, false, true, Some(true), None) => {
                                    Some(VanishClass::BootFailureTeardown)
                                }
                                (true, false, true, _, None) => {
                                    Some(VanishClass::LaunchFailureTeardown)
                                }
                                (true, false, false, _, _) => None,
                            };
                            let got = classify_vanish(present.then_some(&n), tombstoned);
                            assert_eq!(got, want, "class for {row:?}");
                            // Effect row through the production fold:
                            let mut inflight: HashMap<String, Cell> =
                                [("nc-x".to_string(), h.clone())].into();
                            let mut ts = DeleteTombstones::default();
                            if let Some(r) = tombstoned {
                                ts.stamp(ts_seed("nc-x", r), 1);
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
                            // Tombstone consumption: every EXIT eats
                            // the provenance; a KEEP retains it.
                            assert_eq!(
                                ts.contains("nc-x"),
                                tombstoned.is_some() && want.is_none(),
                                "tombstone effect for {row:?}"
                            );
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
            classify_vanish(Some(&mk(Some(true))), None),
            classify_vanish(Some(&mk(Some(false))), None),
            "a generator without the launched axis maps two letters to one row"
        );
        // — and the provenance axis decides ours vs Karpenter's:
        assert_ne!(
            classify_vanish(Some(&mk(None)), Some(ReapReason::BootTimeout)),
            classify_vanish(Some(&mk(None)), None),
            "a generator without the provenance axis maps two letters to one row"
        );
        // The absent projection splits on provenance too:
        assert_ne!(
            classify_vanish(None, Some(ReapReason::Dead)),
            classify_vanish(None, None),
            "absence is not always GcVanish — provenance decides"
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
    /// ALL reap lanes, machine-derived two ways and pinned
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
