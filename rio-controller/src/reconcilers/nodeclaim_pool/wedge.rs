//! OA2: controller-side per-node wedge clustering for pull-mode pools.
//!
//! Pull-mode executor pods do not register or heartbeat, so the
//! scheduler's heartbeat-fed hung-node detector
//! (`sched.admin.hung-node-detector`) goes blind for them: a node whose
//! kubelet/runtime wedges while the Node object stays `Ready` shows up
//! only as its builds running out their attempt deadlines one by one.
//! This module rebuilds that signal from ledger facts the controller
//! already reads each tick:
//!
//! - **Evidence** = an open pull-mode attempt whose age has exceeded its
//!   intent deadline by [`WEDGE_DEADLINE_GRACE_SECS`] (the pod was due
//!   to finish or report by then; an attempt still open past that point
//!   means nothing on the node reported anything — the establishment
//!   sweep will charge it after the report slack). Evidence is keyed on
//!   the attempt's derivation (`intent_id`) and attributed to the
//!   ledger's `source_node` ONLY (the kube-authoritative spawn-ack
//!   binding persisted by the pull transaction). Build-lane attempts
//!   only: a materialization attempt is a store-side fetch whose
//!   stamped node binding is the stale builder pod — never pod-on-node
//!   evidence. Unattributable evidence is dropped — the runbook's
//!   "a cluster of NULLs is not a node signal" rule; the in-memory
//!   newest-pod-wins intent→node fallback is gone (it attributed an
//!   old attempt's expiry to the replacement pod's healthy node).
//! - **Cluster** = a node accumulating evidence for at least
//!   [`WEDGE_CLUSTER_MIN_DISTINCT_DRVS`] *distinct derivations* inside
//!   the [`WEDGE_CLUSTER_WINDOW_SECS`] window, anchored at each
//!   derivation's FIRST observation (a stuck attempt re-observed every
//!   tick does not slide its window — expiries genuinely hours apart
//!   never cluster). One derivation expiring repeatedly is a build
//!   problem (its retries/establishments are handled by the retry
//!   fold), not a node problem — same discrimination the manual
//!   runbook query makes.
//! - **Trajectory guard** (§5-S Q2) = per-node Dead-reaps are gated on
//!   the FLEET trajectory, never a per-tick snapshot: when more than
//!   [`WEDGE_SYSTEMIC_FRACTION`] of the fleet-derived population
//!   (registered NodeClaims ∪ evidence nodes) is past the cluster
//!   threshold (ratio axis), OR bears any in-window expiry evidence
//!   (breadth axis — staggered shared-cause onset suppresses BEFORE
//!   the ratio trips instead of serially reaping each node as it
//!   crosses), OR a suppression watermark latched within
//!   [`WEDGE_VERDICT_DWELL_SECS`] (dwell axis — an episode's trailing
//!   edge is not fresh per-node wedges), the verdict is
//!   [`WedgeVerdict::Systemic`]: nothing is marked and the runbook's
//!   manual discrimination applies — the automation refuses to roll
//!   Dead-reaps across the fleet at `dead_reap_cap`.
//!
//! Clustered nodes are fed to [`super::health::reap_unhealthy`] as the
//! Dead-node input (the only such signal — the scheduler's stream-era
//! heartbeat detector and its `dead_nodes` field are gone), so the
//! existing `ReapReason::Dead` arm
//! — including its per-tick `dead_reap_cap` blast-radius bound — is the
//! single consumer. Evidence is event-shaped and in-memory only. A
//! restart loses accumulated evidence (under-detects until it
//! re-accumulates, at most one window) — but it ALSO loses the
//! suppression watermark and eviction tombstones, so a restart during
//! or just after a systemic episode can OVER-detect: the episode's
//! still-open attempts re-present and re-anchor as if fresh, and the
//! per-node verdicts they mint roll Dead-reaps the latch existed to
//! suppress (merged_bug_060 corrected this doc — the old text claimed
//! the under-detect direction for restarts unconditionally). The
//! `dead_reap_cap` blast-radius bound is the backstop for that
//! window. An open-attempt RPC failure merely skips one tick's
//! observation without dropping prior evidence.

use std::collections::{HashMap, HashSet};

use rio_proto::types::OpenAttempt;

/// Sliding window over which per-node deadline-expiry evidence is
/// clustered. Mirrors the interim `RioSchedulerAttemptEstablishmentCluster`
/// alert and the manual runbook query (30 minutes).
pub(super) const WEDGE_CLUSTER_WINDOW_SECS: f64 = 1800.0;

/// Distinct derivations whose attempts must have expired on one node
/// inside the window before the node is treated as Dead-equivalent.
/// Two distinct derivations is the runbook's hung-node signature; one
/// derivation expiring repeatedly is a derivation problem.
pub(super) const WEDGE_CLUSTER_MIN_DISTINCT_DRVS: usize = 2;

/// Grace added on top of the intent deadline before an open attempt
/// counts as expired. The pod is normally killed at
/// `activeDeadlineSeconds` (≈ the deadline) and its SIGTERM-abort
/// report closes the attempt within seconds; the grace absorbs that
/// propagation so an attempt mid-abort is not evidence. Kept well under
/// the establishment report slack (default 120 s) so expired attempts
/// are still observable in the open view for several ticks before the
/// sweep removes them.
pub(super) const WEDGE_DEADLINE_GRACE_SECS: u64 = 30;

/// Fraction of the REGISTERED NodeClaim fleet past a trajectory
/// threshold above which per-node verdicts are suppressed (shared
/// cause), not per-node wedges. Strictly-greater comparison; also
/// requires at least two nodes on the axis (a two-node fleet with one
/// wedge is 0.5, not systemic).
///
/// SIGNED 2026-06-08 (owner, bughunt-4 fix-wave §5-S Q2): the
/// systemic guard is trajectory-aware over the FLEET-DERIVED
/// denominator. The denominator is the registered NodeClaim fleet
/// united with the evidence-bearing nodes — never the per-tick
/// attributed survivor set (a traffic-lull denominator collapse
/// minted `Systemic{2, of: 2}` from a healthy 8-node fleet, which the
/// episode drain then made sticky). Two trajectory axes gate every
/// Dead-reap: BREADTH (nodes with >=1 in-window expiry — staggered
/// shared-cause onset suppresses per-node verdicts BEFORE the
/// affected-ratio trips, instead of serially Dead-reaping each node
/// as it crosses the threshold) and DWELL (after a suppression
/// watermark latches, per-node verdicts stay disabled for
/// [`WEDGE_VERDICT_DWELL_SECS`] — the trailing edge of an episode is
/// not a sequence of fresh per-node wedges). No retroactive repair
/// for nodes Dead-reaped under the retired instantaneous law —
/// disclosed at signing.
pub(super) const WEDGE_SYSTEMIC_FRACTION: f64 = 0.5;

/// Post-watermark dwell before per-node verdicts re-enable
/// (merged_bug_034 / §5-S Q2 dwell axis). Sized to outlast the
/// trailing edge of a healing episode (report redelivery, sweep
/// catch-up: tens of ticks) while staying well under the
/// [`WEDGE_CLUSTER_WINDOW_SECS`] evidence window — a genuinely
/// still-wedged node re-detects from fresh post-episode expiries
/// after the dwell, one window before its episode evidence would
/// have aged out anyway.
pub(super) const WEDGE_VERDICT_DWELL_SECS: f64 = 300.0;

// merged_bug_023 (round-6 banner rule 3): the "well under" prose above
// becomes a typed, violable envelope — the dwell MUST sit inside the
// evidence window (a dwell at/above the window would let an episode's
// own retained evidence age out before per-node verdicts re-enable,
// turning the re-detect law vacuous), and the Breadth→Dwell downgrade
// law leans on the same ordering (a downgrade-closed episode's dwell
// runs from the transition, one window before its evidence would have
// aged out anyway).
const _: () = assert!(
    WEDGE_VERDICT_DWELL_SECS < WEDGE_CLUSTER_WINDOW_SECS,
    "the post-episode dwell must stay strictly inside the evidence window"
);

// C2/077 gap 3, compile-time half: the wedge must be able to observe an
// expired attempt for its grace plus two full reconcile ticks before
// the establishment sweep may remove it from the open view. The
// scheduler validates the other half (`establishment_report_slack >=
// floor`) at config load — one shared constant, two enforced sides.
const _: () = assert!(
    WEDGE_DEADLINE_GRACE_SECS + 2 * super::TICK.as_secs()
        <= rio_common::limits::MIN_ESTABLISHMENT_REPORT_SLACK_SECS
);

pub(super) use seal::Sealed;

/// The closed suppression-axis alphabet (merged_bug_016/bug_061;
/// SIGNED S1-OQ2: the suppression counter is per-tick with an `axis`
/// label). Precedence ratio > breadth > dwell — the
/// highest-precedence ENGAGING source labels the tick. The
/// [`SuppressionAxis::label`] strings are THE alphabet the lib.rs
/// HELP containment test (`metric_help_alphabets_match_record_sites`)
/// consumes — shared consts, never restated literals. A new axis is
/// a non-exhaustive-match compile error at the per-arm effects in
/// `seal` and a failing containment test until its HELP clause
/// lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SuppressionAxis {
    /// More than half of the fleet-derived population (≥2 nodes)
    /// past the cluster threshold — drains + latches per engaged
    /// tick.
    Ratio,
    /// More than half of the population (≥2 nodes) bearing ≥1
    /// in-window expiry — suppresses while retaining evidence.
    Breadth,
    /// A suppression watermark latched within
    /// [`WEDGE_VERDICT_DWELL_SECS`] — the episode's trailing edge is
    /// not fresh per-node wedges.
    Dwell,
}

impl SuppressionAxis {
    /// Every axis — the containment test iterates this so a new
    /// variant fails the HELP assertion, not a reviewer's memory.
    /// Test-only consumer BY DESIGN (the lib.rs containment test);
    /// production code matches the closed enum exhaustively instead
    /// of iterating it.
    #[cfg_attr(not(test), expect(dead_code))]
    pub(crate) const ALL: [SuppressionAxis; 3] = [
        SuppressionAxis::Ratio,
        SuppressionAxis::Breadth,
        SuppressionAxis::Dwell,
    ];

    /// The Prometheus `axis` label value.
    pub(crate) fn label(self) -> &'static str {
        match self {
            SuppressionAxis::Ratio => "ratio",
            SuppressionAxis::Breadth => "breadth",
            SuppressionAxis::Dwell => "dwell",
        }
    }
}

/// One tick's wedge verdict: per-node Dead-equivalents, a systemic
/// pattern that marks nothing, or an unobserved tick.
#[derive(Debug)]
pub(super) enum WedgeVerdict {
    /// Nodes past the cluster threshold — the only permitted feed of
    /// `health::reap_unhealthy`'s Dead arm.
    NodeWedged(Vec<String>, Sealed),
    /// Per-node verdicts suppressed: shared cause (the affected
    /// ratio or the breadth trajectory axis exceeded
    /// [`WEDGE_SYSTEMIC_FRACTION`] of the fleet-derived population)
    /// or post-episode dwell. Nothing is marked; the trajectory
    /// state AND the engaging axis ride the verdict (§5-S Q2 +
    /// SIGNED S1-OQ2).
    Systemic {
        affected: usize,
        of: usize,
        breadth: usize,
        /// The highest-precedence engaging suppression source — the
        /// same value that labeled this tick's counter increment.
        axis: SuppressionAxis,
        _sealed: Sealed,
    },
    /// The open-attempt view was unobserved this tick (RPC failure):
    /// no verdict — retained evidence neither marks nor suppresses,
    /// and the marked set is NOT re-derived (bug_151: previously
    /// conflated with an empty `NodeWedged`, minted OUTSIDE the
    /// sealed exit).
    Unobserved(Sealed),
}

/// Commensurable verdict populations (merged_bug_009, re-based by
/// merged_bug_034/Q2): numerator, denominator and breadth are paired
/// by the ONLY constructor so `wedged ⊆ evidence-nodes ⊆ population`
/// holds BY CONSTRUCTION, where `population = registered fleet ∪
/// evidence-nodes`. The merged_bug_009 shape used the per-tick
/// ATTRIBUTED fleet (view rows) as the denominator base: a traffic
/// lull collapsed it to the expiring nodes themselves, minting a
/// false `Systemic{2, of: 2}` on a healthy 8-node fleet — which the
/// episode drain+latch then made sticky (permanent signal loss). The
/// registered NodeClaim fleet does not collapse with traffic.
struct WedgePopulations {
    /// Nodes past the cluster threshold (sorted, deduplicated).
    wedged: Vec<String>,
    /// `|registered fleet ∪ evidence-nodes|` (Q2 denominator).
    population: usize,
    /// Nodes with ≥1 in-window expiry — the staggered-onset axis.
    breadth: usize,
}

impl WedgePopulations {
    /// The only constructor: derive all three populations from the
    /// SAME window state + the tick's registered fleet.
    fn from_window(
        evidence: &HashMap<String, HashMap<String, Evidence>>,
        registered: &HashSet<String>,
        now_secs: f64,
    ) -> Self {
        let mut wedged: Vec<String> = evidence
            .iter()
            .filter(|(_, per_node)| {
                per_node
                    .values()
                    .filter(|e| now_secs - e.anchor <= WEDGE_CLUSTER_WINDOW_SECS)
                    .count()
                    >= WEDGE_CLUSTER_MIN_DISTINCT_DRVS
            })
            .map(|(node, _)| node.clone())
            .collect();
        wedged.sort();
        let breadth = evidence
            .iter()
            .filter(|(_, per_node)| {
                per_node
                    .values()
                    .any(|e| now_secs - e.anchor <= WEDGE_CLUSTER_WINDOW_SECS)
            })
            .count();
        let population = evidence
            .keys()
            .chain(registered.iter())
            .collect::<HashSet<_>>()
            .len();
        Self {
            wedged,
            population,
            breadth,
        }
    }
}

/// One (node, derivation) evidence entry.
#[derive(Clone, Copy, Debug)]
struct Evidence {
    /// Controller-frame first-observation instant — the window anchor.
    anchor: f64,
    /// The attempt's expiry instant in the ledger (PG) clock frame:
    /// `assigned_at + deadline + grace`, exact and identical every
    /// tick (merged_bug_018). Skew fallback (`assigned_at_epoch_secs
    /// == 0`, an older scheduler): the controller-frame reconstruction
    /// `now - over`, jitter-bounded to the rolling-skew window.
    expiry_pg: u64,
}

/// The suppression watermark (merged_bug_163), single-frame
/// (merged_bug_018): admission compares ledger-frame expiries against
/// the maximum ledger-frame expiry the drained episode contained.
/// `set_at` is controller-frame, used only for the TTL cadence —
/// never compared against an expiry.
#[derive(Clone, Copy, Debug)]
struct Suppression {
    set_at: f64,
    max_expiry_pg: u64,
}

/// merged_bug_024: the per-tick admission authority — the ONE
/// definition of node-conclusiveness evidence admission may consult.
/// Both conclusiveness legs are fixed at construction: eviction
/// tombstones (controller reaps, absence-sweep evictions, and prior
/// ghost refusals — window-TTL'd in `prune`) and fleet-absence (not in
/// the tick's registered NodeClaim fleet). The absence sweep in
/// `update` derives its evict-and-tombstone decision from the SAME
/// [`AdmissionAuthority::fleet_absent`] predicate the gate composes,
/// so the sweep and the admission gate cannot diverge on which
/// conclusiveness sources exist; a future source is added here and
/// nowhere else.
///
/// The retired shape derived the sweep from `evidence.keys()` only
/// and gated `observe` on tombstones only — a Karpenter-GC'd node
/// with NO prior evidence (never in `evidence`, never
/// controller-reaped) was admitted on its first post-expiry tick and
/// counted in wedged/breadth/population, violating the documented
/// absence law below and `ctrl.nodeclaim.wedge-cluster`'s
/// fleet-absence clause. Ghosts cannot be destructively reaped
/// (`health::classify` emits Dead only for registered claims), but
/// two wedged ghosts on a small fleet minted a false
/// `Systemic{2, of: 3}` whose drain+latch+dwell then blacked out
/// GENUINE per-node verdicts — undone by nothing.
struct AdmissionAuthority<'a> {
    /// Eviction tombstones at tick start (node → eviction instant).
    tombstoned: &'a HashMap<String, f64>,
    /// The tick's registered NodeClaim fleet.
    registered: &'a HashSet<String>,
}

/// One admission ruling. The ghost arm is distinct so the caller can
/// mint the spec's "tombstoned for one window" tombstone exactly once
/// per absence episode: a refused node that is ALREADY tombstoned
/// must not refresh its own tombstone (a self-perpetuating tombstone
/// would keep a re-registered same-named node undetectable forever —
/// the merged_bug_060 over-suppress inversion).
enum Admission {
    /// Both legs pass: the node may accumulate evidence.
    Admissible,
    /// Refused by a live tombstone (tombstone leg has precedence).
    RefusedTombstoned,
    /// Refused by fleet-absence with no live tombstone — a ghost on
    /// its first refusal. The caller tombstones it, so a same-named
    /// NodeClaim that re-registers inside the window cannot inherit
    /// the dead incarnation's still-open attempts as evidence
    /// (under-detect ≤ one window, the module's documented safe
    /// direction).
    RefusedGhost,
}

impl<'a> AdmissionAuthority<'a> {
    /// Constructed once per tick in [`WedgeTracker::update`] after
    /// reap feedback and the absence sweep folded into the
    /// tombstones, before any observation.
    fn for_tick(tombstoned: &'a HashMap<String, f64>, registered: &'a HashSet<String>) -> Self {
        Self {
            tombstoned,
            registered,
        }
    }

    /// The fleet-absence conclusiveness leg — the absence sweep's
    /// predicate. An associated fn (not a method) because the sweep
    /// runs BEFORE the authority can borrow the tombstone map it is
    /// about to mutate; both call sites name this one definition.
    fn fleet_absent(registered: &HashSet<String>, node: &str) -> bool {
        !registered.contains(node)
    }

    /// The admission ruling `observe` consumes — the only
    /// admissibility source (composition of both legs, tombstone leg
    /// first).
    fn admit(&self, node: &str) -> Admission {
        if self.tombstoned.contains_key(node) {
            Admission::RefusedTombstoned
        } else if Self::fleet_absent(self.registered, node) {
            Admission::RefusedGhost
        } else {
            Admission::Admissible
        }
    }
}

/// Per-node deadline-expiry evidence with window pruning. One instance
/// lives on the NodeClaim-pool reconciler; `update` is called once per
/// tick with that tick's open-attempt view (or `None` when the view
/// RPC failed) and the backing nodes the controller reaped since the
/// last call.
#[derive(Default)]
pub(super) struct WedgeTracker {
    /// node → (derivation/intent id → evidence entry). The window
    /// `anchor` is the controller-frame instant the expiry was FIRST
    /// observed (clustering is about OUR observation cadence); entries
    /// age out of the window even while the attempt stays open
    /// (re-anchoring fresh on the next observation), so two expiries
    /// genuinely far apart never cluster. `expiry_pg` is the attempt's
    /// expiry instant in the LEDGER clock frame (merged_bug_018) —
    /// the identity every admission gate compares against.
    evidence: HashMap<String, HashMap<String, Evidence>>,
    /// Nodes currently past the cluster threshold — tracked so the
    /// `rio_controller_node_wedge_marked_total` counter increments once
    /// per not-wedged → wedged transition, not once per tick.
    marked: HashSet<String>,
    /// merged_bug_163: the episode latch. Set on every Systemic
    /// verdict; `observe` admits an expired attempt as evidence ONLY
    /// when its expiry instant is strictly newer than every expiry the
    /// drained episode contained — the episode's still-open attempts
    /// re-present every tick, and without the gate they re-anchor
    /// fresh the tick after the drain (the trailing-edge reap) while
    /// sub-threshold participants' anchors pair with any later blip.
    /// merged_bug_018: the comparison is PG-frame on BOTH sides
    /// (`max_expiry_pg` vs the row's `expiry_pg`), never a
    /// reconstructed controller-frame instant.
    last_suppression: Option<Suppression>,
    /// merged_bug_017: eviction tombstones — node → controller-frame
    /// instant it was evicted (reaped by the controller, or absent
    /// from the registered NodeClaim fleet). Eviction is an ADMISSION
    /// source, not a state wipe: a reaped node's still-open attempts
    /// re-present in the very next view (the establishment sweep takes
    /// several ticks to close them), and the one-shot wipe let them
    /// re-anchor the same tick — re-feeding the Dead arm with a node
    /// that no longer exists. Membership blocks admission outright
    /// (nothing new can be assigned to a deleted node, so every
    /// still-open attempt predates the eviction by construction —
    /// no cross-frame instant comparison needed). Tombstones live one
    /// [`WEDGE_CLUSTER_WINDOW_SECS`] window — the same window law as
    /// the suppression watermark — so a same-named node recreated
    /// later can accumulate evidence again (under-detect ≤ one window,
    /// the module's documented safe direction).
    evicted: HashMap<String, f64>,
    /// merged_bug_023: the engaged suppression episode. `Some` while
    /// any axis engages (the axis recomputed per OBSERVED tick by
    /// precedence — an episode may escalate Breadth→Ratio or trail
    /// off Ratio→Dwell); an observed tick with NO engaging axis
    /// releases it through `seal::close_episode`, which executes the
    /// DISENGAGING variant's release effects. Unobserved ticks hold
    /// the episode (no release without an observed disengaged
    /// evaluation). The retired shape had no release edge at all:
    /// `last_suppression` was written only in the ratio arm, so a
    /// breadth episode that decayed below the fraction (staggered
    /// age-out, fleet growth) exited through a silent third door —
    /// retaining evidence the suppression itself attributed to a
    /// shared cause, which then Dead-reaped the late-onset node.
    episode: Option<EngagedEpisode>,
}

/// merged_bug_023: one engaged suppression episode (R14: the episode
/// object is replaced wholesale at the chokepoint, never field-poked).
#[derive(Clone, Copy, Debug)]
struct EngagedEpisode {
    /// Controller-frame instant the episode first engaged (kept
    /// across continue/escalate ticks; diagnostics only).
    engaged_at: f64,
    /// The engaging axis as of the LAST OBSERVED engaged tick — the
    /// variant whose release effects run at the close.
    axis: SuppressionAxis,
}

/// The verdict seal, CONFINED (bug_151): `Sealed` and the single
/// epilogue both live in this module, so a verdict token cannot be
/// minted anywhere else in the file — an early return in `update`
/// that skips the epilogue does not compile. (The previous shape
/// relied on field privacy, which is module-scoped: `update`'s
/// None arm freely minted `Sealed(())` 110 lines below the doc that
/// called that unrepresentable.)
mod seal {
    use super::{
        EngagedEpisode, Suppression, SuppressionAxis, WEDGE_CLUSTER_MIN_DISTINCT_DRVS,
        WEDGE_CLUSTER_WINDOW_SECS, WEDGE_SYSTEMIC_FRACTION, WEDGE_VERDICT_DWELL_SECS,
        WedgePopulations, WedgeTracker, WedgeVerdict,
    };

    /// Proof-of-origin token for [`WedgeVerdict`]: constructible only
    /// inside this module, whose only verdict producer is
    /// [`finalize`] — the single exit that runs the full epilogue.
    #[derive(Debug)]
    pub(in crate::reconcilers::nodeclaim_pool) struct Sealed(());

    /// What happens to the evidence window this tick.
    enum EvidenceDisposition {
        /// Drain the WHOLE window — every node in it, not just the
        /// wedged subset (merged_bug_163: a sub-threshold
        /// participant's episode anchor must not survive as half of
        /// a future pair).
        DrainWindow,
        /// Keep the window untouched.
        Retain,
    }

    /// What happens to the suppression watermark this tick.
    enum WatermarkDisposition {
        /// Latch at the max of any LIVE watermark and the drained
        /// evidence's maximum ledger-frame expiry — the gate never
        /// lowers — and refresh `set_at` so the dwell runs from this
        /// latch (merged_bug_018: both sides PG-frame).
        MergeLatch,
        /// Keep as-is.
        Keep,
    }

    /// What happens to the marked transition-memory this tick.
    enum MarkedDisposition {
        /// `marked` becomes exactly this set: transition-gated
        /// inserts (counter + warn inside the applier) plus removal
        /// of everything else — a node that fell under the threshold
        /// or whose episode drained leaves, so a later re-wedge
        /// counts as a NEW transition.
        ReplaceWith(Vec<String>),
        /// Keep as-is. A suppressed tick that retains evidence MUST
        /// retain the transition memory too (merged_bug_016: the
        /// retired shared `survivors` tail drained `marked` on every
        /// breadth/dwell-suppressed tick, so ONE continuous wedge
        /// re-counted `rio_controller_node_wedge_marked_total` and
        /// re-fired the Dead-equivalent warn after every suppressed
        /// phase, violating the once-per-transition contract).
        Retain,
    }

    /// merged_bug_016 (R14): the TOTAL per-arm epilogue effects
    /// record. Every populated verdict arm constructs one — all four
    /// fields mandatory — so an arm that retains evidence must STATE
    /// its marked-retention and its counter decision; drain-on-retain
    /// and silent counter gaps cease to typecheck as shared-tail
    /// defaults. Applied by [`apply`], the only mutation site for
    /// `evidence` / `last_suppression` / `marked` / both counters.
    struct ArmEffects {
        evidence: EvidenceDisposition,
        watermark: WatermarkDisposition,
        marked: MarkedDisposition,
        /// `Some(axis)` increments
        /// `rio_controller_wedge_systemic_suppressed_total{axis}` —
        /// the observability tick is PART of the typed closure set
        /// (bug_061: the breadth/dwell arms previously withheld both
        /// the Dead-arm input AND the counter, blinding the
        /// runbook's "non-zero = systemic triage" tripwire exactly
        /// while the automation was suppressing).
        suppressed_tick: Option<SuppressionAxis>,
    }

    impl ArmEffects {
        /// merged_bug_023: strip the suppression counter from an
        /// effects record. Used by [`transition_effects`]: the
        /// engaged tick stays counted ONCE, by the engaging axis's
        /// `engage_tick_effects` (SIGNED S1-OQ2
        /// per-tick-labeled-by-engaging-axis semantics) — running the
        /// outgoing axis's release effects verbatim at a transition
        /// would double-count the tick under the outgoing label.
        fn sans_suppressed_tick(self) -> ArmEffects {
            ArmEffects {
                suppressed_tick: None,
                ..self
            }
        }
    }

    /// merged_bug_023: the transition law — an axis CHANGE on an
    /// engaged episode is itself an edge, total over the axis product
    /// BY QUANTIFICATION: `from == to → None` (continue, no edge);
    /// `from != to → the outgoing axis's release obligations` (with
    /// the per-tick suppression counter stripped — see
    /// [`ArmEffects::sans_suppressed_tick`]). Compiler totality rides
    /// [`SuppressionAxis::release_effects`]'s exhaustive per-variant
    /// match — a new axis must declare its release before any
    /// transition out of it compiles; the 3×3 census walks every cell
    /// through the real `finalize`.
    ///
    /// Composition argument, per cell:
    ///   - Ratio→\* and Dwell→\*: their releases are identity records
    ///     (Retain/Keep/Retain) — the uniform law is a no-op there.
    ///   - Breadth→Ratio: double drain + double MergeLatch compose
    ///     idempotently (drain of drained is empty; MergeLatch is
    ///     monotone-max with a `set_at` refresh — same tick, same
    ///     value).
    ///   - Breadth→Dwell: THE behavioral cell — drain + merge-latch +
    ///     marked-clear at the downgrade, so the dwell phase starts
    ///     exactly like a post-close tail (which is what the Dwell
    ///     axis IS), `set_at` refreshes so the dwell runs from the
    ///     transition, and post-downgrade fresh expiries remain the
    ///     only path to a post-dwell per-node verdict (the existing
    ///     `post_episode_dwell_gates_per_node_verdicts` re-detect
    ///     law).
    fn transition_effects(from: SuppressionAxis, to: SuppressionAxis) -> Option<ArmEffects> {
        if from == to {
            None
        } else {
            Some(from.release_effects().sans_suppressed_tick())
        }
    }

    impl SuppressionAxis {
        /// Per-variant ENGAGED-TICK effects — exhaustive over the
        /// closed alphabet: a new axis fails to compile here until
        /// its effects are declared.
        fn engage_tick_effects(self) -> ArmEffects {
            match self {
                // Ratio: drain + latch are PER-ENGAGED-TICK effects
                // (spec-mandated re-derivation — a post-episode
                // re-wedge IS a new transition).
                SuppressionAxis::Ratio => ArmEffects {
                    evidence: EvidenceDisposition::DrainWindow,
                    watermark: WatermarkDisposition::MergeLatch,
                    marked: MarkedDisposition::ReplaceWith(Vec::new()),
                    suppressed_tick: Some(SuppressionAxis::Ratio),
                },
                // Breadth: retain everything while engaged — the
                // evidence keeps accumulating toward the ratio law.
                SuppressionAxis::Breadth => ArmEffects {
                    evidence: EvidenceDisposition::Retain,
                    watermark: WatermarkDisposition::Keep,
                    marked: MarkedDisposition::Retain,
                    suppressed_tick: Some(SuppressionAxis::Breadth),
                },
                // Dwell: retain — fresh post-watermark evidence is
                // building toward the post-dwell re-detect.
                SuppressionAxis::Dwell => ArmEffects {
                    evidence: EvidenceDisposition::Retain,
                    watermark: WatermarkDisposition::Keep,
                    marked: MarkedDisposition::Retain,
                    suppressed_tick: Some(SuppressionAxis::Dwell),
                },
            }
        }

        /// merged_bug_023: per-variant RELEASE effects — exhaustive
        /// over the closed alphabet, so a new axis must declare its
        /// close before it compiles. The third silent exit (a breadth
        /// episode decaying below the fraction with its retained
        /// evidence intact) becomes unrepresentable.
        fn release_effects(self) -> ArmEffects {
            match self {
                // Ratio: a no-op at release — its drain + latch are
                // PER-ENGAGED-TICK effects (already executed).
                SuppressionAxis::Ratio => ArmEffects {
                    evidence: EvidenceDisposition::Retain,
                    watermark: WatermarkDisposition::Keep,
                    marked: MarkedDisposition::Retain,
                    suppressed_tick: None,
                },
                // Breadth: the close — drain the whole window,
                // merge-latch the watermark (never lowering the
                // gate; `set_at` refreshes so the dwell runs from
                // the close), drain the marked transition-memory.
                // Evidence observed during the engaged episode
                // cannot mint a per-node verdict after release: the
                // late-onset node re-detects only from
                // WEDGE_CLUSTER_MIN_DISTINCT_DRVS fresh
                // post-watermark expiries after the dwell — the same
                // re-entry law as a ratio close. The close tick IS a
                // suppressed tick (it withholds a would-be per-node
                // verdict), so it counts, labeled by the closing
                // axis.
                SuppressionAxis::Breadth => ArmEffects {
                    evidence: EvidenceDisposition::DrainWindow,
                    watermark: WatermarkDisposition::MergeLatch,
                    marked: MarkedDisposition::ReplaceWith(Vec::new()),
                    suppressed_tick: Some(SuppressionAxis::Breadth),
                },
                // Dwell: a no-op — draining at dwell expiry would
                // destroy legitimate FRESH post-watermark evidence
                // (the post-dwell re-detect the existing
                // post_episode_dwell_gates_per_node_verdicts test
                // pins).
                SuppressionAxis::Dwell => ArmEffects {
                    evidence: EvidenceDisposition::Retain,
                    watermark: WatermarkDisposition::Keep,
                    marked: MarkedDisposition::Retain,
                    suppressed_tick: None,
                },
            }
        }
    }

    /// merged_bug_023: the release chokepoint. An engaged episode
    /// that disengages on an OBSERVED tick closes HERE — the
    /// disengaging variant's [`SuppressionAxis::release_effects`]
    /// run through the same single applier as every engaged tick.
    /// Returns the closing axis when the close itself suppressed
    /// this tick's would-be per-node verdict (the breadth close);
    /// `None` for the no-op releases (ratio, dwell), whose tick
    /// proceeds to the normal per-node evaluation.
    fn close_episode(
        t: &mut WedgeTracker,
        episode: EngagedEpisode,
        now_secs: f64,
    ) -> Option<SuppressionAxis> {
        let effects = episode.axis.release_effects();
        let closing = effects.suppressed_tick;
        if let Some(axis) = closing {
            tracing::warn!(
                axis = axis.label(),
                engaged_at = episode.engaged_at,
                "suppression episode closed at its release edge: draining the \
                 episode's window evidence, merge-latching the suppression \
                 watermark and starting the dwell — evidence observed during \
                 the engaged episode cannot mint a per-node verdict after \
                 release (a late-onset node re-detects from fresh \
                 post-watermark expiries after the dwell)"
            );
        }
        apply(t, effects, now_secs);
        closing
    }

    /// The single effects applier: the ONLY place `evidence`,
    /// `last_suppression`, `marked` and the two wedge counters
    /// mutate. Field order is load-bearing: the drain computes the
    /// episode's max ledger-frame expiry BEFORE clearing, and the
    /// merge-latch consumes it.
    fn apply(t: &mut WedgeTracker, effects: ArmEffects, now_secs: f64) {
        let drained_max: Option<u64> = match effects.evidence {
            EvidenceDisposition::DrainWindow => {
                let max = t
                    .evidence
                    .values()
                    .flat_map(|per| per.values())
                    .map(|e| e.expiry_pg)
                    .max();
                t.evidence.clear();
                max
            }
            EvidenceDisposition::Retain => None,
        };
        match effects.watermark {
            WatermarkDisposition::MergeLatch => {
                // Merge law: max of any live (in-TTL) watermark and
                // the drained episode's newest PG-frame expiry — the
                // admission gate never lowers; `set_at` refreshes so
                // the dwell runs from this latch.
                let live = t
                    .last_suppression
                    .filter(|w| now_secs - w.set_at <= WEDGE_CLUSTER_WINDOW_SECS)
                    .map(|w| w.max_expiry_pg)
                    .unwrap_or(0);
                t.last_suppression = Some(Suppression {
                    set_at: now_secs,
                    max_expiry_pg: live.max(drained_max.unwrap_or(0)),
                });
            }
            WatermarkDisposition::Keep => {}
        }
        match effects.marked {
            MarkedDisposition::ReplaceWith(nodes) => {
                for node in &nodes {
                    if t.marked.insert(node.clone()) {
                        metrics::counter!("rio_controller_node_wedge_marked_total").increment(1);
                        tracing::warn!(
                            node = %node,
                            "node marked Dead-equivalent: ≥{WEDGE_CLUSTER_MIN_DISTINCT_DRVS} distinct \
                             derivations' pull attempts expired on it inside the window (OA2 clustering)"
                        );
                    }
                }
                t.marked.retain(|n| nodes.contains(n));
            }
            MarkedDisposition::Retain => {}
        }
        if let Some(axis) = effects.suppressed_tick {
            // SIGNED S1-OQ2: per-tick, labeled by the engaging axis.
            metrics::counter!(
                "rio_controller_wedge_systemic_suppressed_total",
                "axis" => axis.label()
            )
            .increment(1);
        }
    }

    /// The single verdict exit: every arm constructs its TOTAL
    /// [`ArmEffects`] record, executed by [`apply`].
    ///
    /// - `None` (unobserved tick, bug_151): a distinct
    ///   [`WedgeVerdict::Unobserved`] — retained evidence neither
    ///   marks nor suppresses, and `marked` is NOT re-derived (an RPC
    ///   blip draining it would double-count the next transitions).
    /// - Engaged (Some axis, precedence ratio > breadth > dwell):
    ///   the axis's per-variant engage-tick effects — ratio drains
    ///   the WHOLE episode + merge-latches the watermark `observe`
    ///   gates admission on (a genuinely stuck node re-detects from
    ///   [`WEDGE_CLUSTER_MIN_DISTINCT_DRVS`] fresh POST-episode
    ///   expiries, the §5-Q21 direction); breadth/dwell retain. The
    ///   suppression counter ticks on EVERY engaged tick, labeled by
    ///   the axis.
    /// - NodeWedged: transition-gated counter + marked-set
    ///   re-derivation.
    pub(super) fn finalize(
        t: &mut WedgeTracker,
        populations: Option<WedgePopulations>,
        now_secs: f64,
    ) -> WedgeVerdict {
        let Some(WedgePopulations {
            wedged,
            population,
            breadth,
        }) = populations
        else {
            return WedgeVerdict::Unobserved(Sealed(()));
        };
        let affected = wedged.len();
        // §5-S Q2 trajectory law, three suppression sources in
        // precedence order:
        // 1. affected ratio (the classic systemic guard, now over the
        //    fleet-derived denominator) — drains + latches;
        // 2. breadth (staggered shared-cause onset: most of the fleet
        //    has SOME expiry evidence even though few crossed the
        //    per-node threshold yet) — suppresses WITHOUT draining,
        //    so the evidence keeps accumulating: either the ratio law
        //    fires next (drain+latch) or the window ages it out;
        // 3. dwell (a suppression watermark latched less than
        //    WEDGE_VERDICT_DWELL_SECS ago) — the trailing edge of an
        //    episode is not a sequence of fresh per-node wedges.
        let systemic =
            affected >= 2 && (affected as f64 / population as f64) > WEDGE_SYSTEMIC_FRACTION;
        let breadth_suppressed =
            breadth >= 2 && (breadth as f64 / population as f64) > WEDGE_SYSTEMIC_FRACTION;
        let dwell_active = t
            .last_suppression
            .is_some_and(|w| now_secs - w.set_at <= WEDGE_VERDICT_DWELL_SECS);
        let engaging: Option<SuppressionAxis> = if systemic {
            Some(SuppressionAxis::Ratio)
        } else if breadth_suppressed {
            Some(SuppressionAxis::Breadth)
        } else if dwell_active && !wedged.is_empty() {
            Some(SuppressionAxis::Dwell)
        } else {
            None
        };
        match engaging {
            Some(axis) => {
                match axis {
                    SuppressionAxis::Ratio => tracing::warn!(
                        affected,
                        of = population,
                        breadth,
                        "wedge clustering suppressed: >{WEDGE_SYSTEMIC_FRACTION} of the \
                         fleet-derived population is past the expiry threshold — systemic \
                         cause (report-path outage, store brownout), not per-node wedges; \
                         marking nothing, draining the WHOLE episode's evidence and \
                         latching the suppression watermark (see the hung-node runbook's \
                         systemic discrimination)"
                    ),
                    SuppressionAxis::Breadth | SuppressionAxis::Dwell => tracing::warn!(
                        affected,
                        of = population,
                        breadth,
                        axis = axis.label(),
                        "wedge per-node verdicts suppressed by the trajectory axes (§5-S \
                         Q2): breadth-fraction systemic-in-formation or post-episode \
                         dwell — marking nothing; evidence is retained while ENGAGED: \
                         the ratio law may still fire (drain+latch), and a breadth \
                         episode that ends without it closes through the same \
                         drain+merge-latch+dwell chokepoint at its release edge"
                    ),
                }
                // Engage or continue (merged_bug_023): the episode's
                // axis is recomputed per observed tick by precedence;
                // `engaged_at` survives continue/escalate/transition
                // (observability). An axis CHANGE is itself an edge:
                // the OUTGOING axis's release obligations run at the
                // transition, BEFORE the incoming axis's engaged-tick
                // effects — in particular a Breadth→Dwell downgrade
                // closes here (drain + merge-latch + dwell measured
                // from the transition), so breadth-phase evidence can
                // never fall through the dwell expiry into per-node
                // verdicts (the close law the release edge already
                // enforces, extended to the transition edge). The
                // suppressed tick stays counted once, by the engaging
                // axis (SIGNED S1-OQ2).
                let engaged_at = t.episode.map_or(now_secs, |e| e.engaged_at);
                if let Some(prior) = t.episode
                    && let Some(effects) = transition_effects(prior.axis, axis)
                {
                    tracing::warn!(
                        from = prior.axis.label(),
                        to = axis.label(),
                        engaged_at,
                        "engaged suppression episode changed axis: running the \
                         outgoing axis's release obligations at the transition \
                         (a breadth downgrade closes here — drain, merge-latch, \
                         dwell from the transition; evidence observed during \
                         the engaged episode cannot mint a per-node verdict \
                         after it)"
                    );
                    apply(t, effects, now_secs);
                }
                t.episode = Some(EngagedEpisode { engaged_at, axis });
                apply(t, axis.engage_tick_effects(), now_secs);
                // The verdict reports the pre-drain populations + the
                // engaging axis.
                WedgeVerdict::Systemic {
                    affected,
                    of: population,
                    breadth,
                    axis,
                    _sealed: Sealed(()),
                }
            }
            None => {
                // merged_bug_023: the release edge — an episode that
                // was engaged and now is not, on an OBSERVED tick,
                // transits the close_episode chokepoint. A breadth
                // close suppresses this tick's would-be per-node
                // verdict (the late-onset node's anchors were just
                // drained + watermark-gated); ratio/dwell releases
                // are no-ops and the tick evaluates normally.
                let closing_axis = t
                    .episode
                    .take()
                    .and_then(|episode| close_episode(t, episode, now_secs));
                if let Some(axis) = closing_axis {
                    return WedgeVerdict::Systemic {
                        affected,
                        of: population,
                        breadth,
                        axis,
                        _sealed: Sealed(()),
                    };
                }
                apply(
                    t,
                    ArmEffects {
                        evidence: EvidenceDisposition::Retain,
                        watermark: WatermarkDisposition::Keep,
                        marked: MarkedDisposition::ReplaceWith(wedged.clone()),
                        suppressed_tick: None,
                    },
                    now_secs,
                );
                WedgeVerdict::NodeWedged(wedged, Sealed(()))
            }
        }
    }
}

impl WedgeTracker {
    /// One tick: evict reaped nodes, fold the view (if observed),
    /// prune the window, and produce the verdict through the single
    /// sealed exit.
    ///
    /// - `view = None` (the open-attempt RPC failed): the tick skips
    ///   observation AND verdict — previously accumulated evidence
    ///   stays, and no node is marked from stale data. This makes the
    ///   call-site comment literally true (the retired path ran the
    ///   full verdict over an empty fleet, which mass-marked every
    ///   retained-evidence node right after a systemic episode).
    /// - `reaped_since_last` is a REQUIRED argument: reap feedback
    ///   cannot be forgotten by a call site (the backing nodes the
    ///   controller deleted since the previous tick — their evidence
    ///   and marked entries are dead, and must not re-feed the Dead
    ///   arm or inflate the systemic populations).
    // r[impl ctrl.nodeclaim.wedge-cluster+3]
    // r[impl ctrl.nodeclaim.wedge-two-axis+6]
    pub(super) fn update(
        &mut self,
        view: Option<&[OpenAttempt]>,
        reaped_since_last: &std::collections::BTreeSet<String>,
        registered: &HashSet<String>,
        now_secs: f64,
    ) -> WedgeVerdict {
        for node in reaped_since_last {
            self.evidence.remove(node);
            self.marked.remove(node);
            self.evicted.insert(node.clone(), now_secs);
        }
        // merged_bug_017: a node absent from the registered NodeClaim
        // fleet (deleted out-of-band, Karpenter GC) is evicted exactly
        // like a reaped one — its evidence cannot mark a node that is
        // gone, and it must not inflate the verdict populations.
        // merged_bug_024: the sweep's "absent" is the SAME
        // fleet-absence predicate the admission authority composes —
        // the sweep covers evidence-bearing absentees, the gate
        // refuses (and tombstones) ghosts that never had evidence.
        let absent: Vec<String> = self
            .evidence
            .keys()
            .filter(|n| AdmissionAuthority::fleet_absent(registered, n))
            .cloned()
            .collect();
        for node in absent {
            self.evidence.remove(&node);
            self.marked.remove(&node);
            self.evicted.insert(node, now_secs);
        }
        let Some(open_attempts) = view else {
            // bug_151: the unobserved tick routes through the SAME
            // sealed exit as every verdict — `seal::finalize` is the
            // only place a token can be minted, so this arm cannot
            // skip the epilogue accounting; the epilogue itself
            // branches on the unobserved case (an RPC blip must not
            // drain `marked` and double-count later transitions).
            return seal::finalize(self, None, now_secs);
        };
        self.observe(open_attempts, registered, now_secs);
        self.prune(now_secs);
        let populations = WedgePopulations::from_window(&self.evidence, registered, now_secs);
        seal::finalize(self, Some(populations), now_secs)
    }

    /// Fold one tick's open-attempt view into the evidence map. Only
    /// BUILD attempts past `deadline + grace` with a known deadline and
    /// a ledger node attribution contribute; each (node, derivation)
    /// pair anchors at its first observation. (The systemic-guard
    /// denominator is the REGISTERED fleet — §5-S Q2 — not the view's
    /// attributed nodes, so nothing is returned here.)
    ///
    /// merged_bug_024: node-conclusiveness is consumed ONLY through
    /// the per-tick [`AdmissionAuthority`] (tombstones ∪
    /// fleet-absence). A ledger-attributed attempt on a never-seen
    /// fleet-absent node (Karpenter-GC'd before its first
    /// observation) is refused AND tombstoned on first refusal — the
    /// spec's "tombstoned for one window" — instead of admitted into
    /// the verdict populations.
    fn observe(
        &mut self,
        open_attempts: &[OpenAttempt],
        registered: &HashSet<String>,
        now_secs: f64,
    ) {
        let authority = AdmissionAuthority::for_tick(&self.evicted, registered);
        let mut refused_ghosts: Vec<String> = Vec::new();
        for a in open_attempts {
            if a.attempt_kind != rio_proto::types::AttemptKind::Build as i32 {
                // Materialization (store fetch): the stamped node is the
                // stale builder binding — never pod-on-node evidence.
                // UNSPECIFIED is skipped too: a fail-closed posture for
                // a destructive Dead-reap — deliberately DIFFERENT from
                // pool/job.rs's MintedPullIdentity, which reads
                // UNSPECIFIED as Build per the pinned proto posture (a
                // report is charge-accounting, a reap is destruction;
                // RULED S2-OQ4, neither site "fixes" the other).
                continue;
            }
            if a.source_node.is_empty() {
                // Not ledger-attributable: never evidence against any
                // node (and not fleet either — an unattributed attempt
                // says nothing about node breadth).
                continue;
            }
            match authority.admit(&a.source_node) {
                // merged_bug_017: an evicted node's still-open attempts
                // are inadmissible. Eviction is an admission source
                // consumed here, not a state wipe the next observation
                // undoes.
                Admission::RefusedTombstoned => continue,
                // merged_bug_024: first refusal of a fleet-absent
                // ghost — queue its tombstone (applied after the loop;
                // the authority borrows the tombstone map).
                Admission::RefusedGhost => {
                    refused_ghosts.push(a.source_node.clone());
                    continue;
                }
                Admission::Admissible => {}
            }
            if a.deadline_secs == 0 {
                // Deadline unknown to the scheduler — can't call it expired.
                continue;
            }
            if a.assigned_at_age_secs <= a.deadline_secs.saturating_add(WEDGE_DEADLINE_GRACE_SECS) {
                // Healthy (or still inside the abort-report grace).
                continue;
            }
            // merged_bug_163: evidence admission must beat the
            // suppression watermark — an expiry at or before the
            // drained episode's newest expiry belongs to that episode
            // (the same still-open attempts must not re-anchor at the
            // trailing edge, and a sub-threshold participant's episode
            // expiry must not pair with a later blip); a genuinely NEW
            // expiry admits normally.
            // merged_bug_018: the expiry identity is single-frame —
            // `assigned_at + deadline + grace`, all PG-clock — so one
            // physical expiry compares identically every tick. The
            // retired reconstruction (`now − over`) mixed the PG-clock
            // age (mid-tick RPC, u64-floored) with the controller's
            // tick-START instant: ±(preamble + flooring) of jitter,
            // flipping admission at the watermark boundary. Skew
            // fallback (older scheduler, epoch field absent → 0): the
            // reconstruction, jitter-bounded to the rollout window
            // (scheduler deploys first, the attempt_kind posture).
            let expiry_pg = if a.assigned_at_epoch_secs > 0 {
                a.assigned_at_epoch_secs
                    .saturating_add(a.deadline_secs)
                    .saturating_add(WEDGE_DEADLINE_GRACE_SECS)
            } else {
                let over = a
                    .assigned_at_age_secs
                    .saturating_sub(a.deadline_secs.saturating_add(WEDGE_DEADLINE_GRACE_SECS));
                (now_secs - over as f64).max(0.0) as u64
            };
            // merged_bug_060: the gate itself expires after one
            // window — a protective latch cannot outlive the episode
            // it suppresses. Without the TTL an episode attempt that
            // never closes (a genuinely wedged node is exactly the
            // node whose attempts never close) was blocked FOREVER:
            // the node became permanently undetectable, a NodeClaim
            // leak in the over-suppress direction.
            if self.last_suppression.is_some_and(|w| {
                expiry_pg <= w.max_expiry_pg && now_secs - w.set_at <= WEDGE_CLUSTER_WINDOW_SECS
            }) {
                continue;
            }
            self.evidence
                .entry(a.source_node.clone())
                .or_default()
                .entry(a.intent_id.clone())
                // First-observation anchor: a stuck-open attempt
                // re-observed every tick does not slide its window. (An
                // entry that ages out while the attempt stays open
                // re-anchors fresh on the next observation — a
                // derivation stuck for a full window AND a second
                // expiry is the runbook's genuine signature.)
                .or_insert(Evidence {
                    anchor: now_secs,
                    expiry_pg,
                });
        }
        // merged_bug_024: mint the ghosts' tombstones (one per absence
        // episode — `or_insert` dedups within the tick; across ticks
        // the `RefusedGhost` arm only fires while NO tombstone lives,
        // so an expired tombstone re-mints but a live one never
        // self-refreshes). The window TTL in `prune` is the same
        // one-window law as every other tombstone.
        for node in refused_ghosts {
            self.evicted.entry(node).or_insert(now_secs);
        }
    }

    /// Drop evidence older than the window, nodes left without any,
    /// and eviction tombstones past the window (one window law shared
    /// with the suppression watermark: no admission gate outlives the
    /// episode it suppresses).
    fn prune(&mut self, now_secs: f64) {
        for per_node in self.evidence.values_mut() {
            per_node.retain(|_, e| now_secs - e.anchor <= WEDGE_CLUSTER_WINDOW_SECS);
        }
        self.evidence.retain(|_, per_node| !per_node.is_empty());
        self.evicted
            .retain(|_, t| now_secs - *t <= WEDGE_CLUSTER_WINDOW_SECS);
        if self
            .last_suppression
            .is_some_and(|w| now_secs - w.set_at > WEDGE_CLUSTER_WINDOW_SECS)
        {
            self.last_suppression = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::NodeClaimPoolConfig;
    fn no_reaps() -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::new()
    }

    /// The registered NodeClaim fleet for a tick (backing node names).
    fn fleet(nodes: &[&str]) -> HashSet<String> {
        nodes.iter().map(|s| s.to_string()).collect()
    }

    use super::super::ffd::tests::with_conds;
    use super::super::health::{ReapReason, classify};
    use super::super::sketch::{CapacityType, CellSketches};
    use super::*;

    /// One expired open attempt for `intent` on `node`, `over_secs`
    /// past its deadline+grace.
    fn expired(intent: &str, node: &str, over_secs: u64) -> OpenAttempt {
        OpenAttempt {
            intent_id: intent.into(),
            derivation: format!("/nix/store/{intent}.drv"),
            exec_id: format!("exec-{intent}"),
            executor_id: intent.into(),
            source_node: node.into(),
            generation: 1,
            assigned_at_age_secs: 100 + WEDGE_DEADLINE_GRACE_SECS + over_secs,
            // Skew-fallback fixtures (epoch 0): the unit tests below
            // pin the reconstruction path; the PG-frame path is pinned
            // by `pg_frame_admission_is_jitter_stable` and the jitter
            // proptest.
            assigned_at_epoch_secs: 0,
            deadline_secs: 100,
            // The wedge consumes BUILD evidence only (C2/222);
            // UNSPECIFIED (skew) and MATERIALIZATION are skipped.
            attempt_kind: rio_proto::types::AttemptKind::Build as i32,
        }
    }

    /// Unwrap the per-node verdict (panics on a systemic verdict —
    /// tests that expect suppression match on it directly).
    fn nodes(v: WedgeVerdict) -> Vec<String> {
        match v {
            WedgeVerdict::NodeWedged(n, _) => n,
            WedgeVerdict::Systemic { affected, of, .. } => {
                panic!("unexpected systemic verdict ({affected}/{of})")
            }
            WedgeVerdict::Unobserved(_) => panic!("unexpected unobserved verdict"),
        }
    }

    /// A healthy open attempt (well inside its deadline).
    fn healthy(intent: &str, node: &str) -> OpenAttempt {
        OpenAttempt {
            assigned_at_age_secs: 10,
            ..expired(intent, node, 0)
        }
    }

    /// Two attempts for distinct derivations expired on the same node
    /// inside the window → that node (and only it) is Dead-equivalent,
    /// and `classify` consumes it as the SOLE Dead input (the removed
    /// `dead_nodes` field — reserved in the proto since the 1d sweep —
    /// fed the same arm in its day; see the module header).
    // r[verify ctrl.nodeclaim.wedge-cluster+3]
    #[test]
    fn two_expired_drvs_on_one_node_mark_it_dead_equivalent() {
        let mut tracker = WedgeTracker::default();
        let wedged = nodes(tracker.update(
            Some(&[
                expired("drv-a", "node-1", 5),
                expired("drv-b", "node-1", 5),
                expired("drv-c", "node-2", 5),
                healthy("drv-d", "node-3"),
            ]),
            &no_reaps(),
            // Six registered nodes: breadth (2 evidence-bearing of 6)
            // stays under the trajectory fraction, so the basic
            // detection case still marks (Q2).
            &fleet(&["node-1", "node-2", "node-3", "node-4", "node-5", "node-6"]),
            10_000.0,
        ));
        assert_eq!(wedged, vec!["node-1".to_string()]);

        // The wedge list flows into `classify` as the Dead input: the
        // registered NodeClaim backing node-1 classifies Dead; node-2
        // (one expiry) and node-3 (healthy pulls) do not.
        let dead_set: HashSet<&str> = wedged.iter().map(String::as_str).collect();
        let cfg = NodeClaimPoolConfig::default();
        let sk = CellSketches::default();
        let live: Vec<_> = ["node-1", "node-2", "node-3"]
            .iter()
            .map(|n| {
                // ffd::tests::node names the NodeClaim `nc` and its backing
                // Node `node-{nc}`; strip the prefix to line them up.
                with_conds(
                    super::super::ffd::tests::node(
                        n.trim_start_matches("node-"),
                        "h",
                        CapacityType::Spot,
                        8,
                        0,
                        0,
                    ),
                    &[("Registered", "True", 1042.0)],
                )
            })
            .collect();
        let reaped = classify(&live, &dead_set, &sk, &cfg, 1100.0);
        assert_eq!(
            reaped.len(),
            1,
            "exactly one claim classifies Dead: {reaped:?}"
        );
        assert_eq!(reaped[0].1, ReapReason::Dead);
        assert_eq!(live[reaped[0].0].node_name.as_deref(), Some("node-1"));
    }

    /// One derivation expiring (even repeatedly observed) never marks a
    /// node, and healthy pulls contribute nothing.
    // r[verify ctrl.nodeclaim.wedge-cluster+3]
    #[test]
    fn single_expired_drv_or_healthy_pulls_do_not_mark() {
        let mut tracker = WedgeTracker::default();
        // Same single derivation observed expired on three consecutive ticks.
        for tick in 0u64..3 {
            let wedged = nodes(tracker.update(
                Some(&[
                    expired("drv-a", "node-1", 5 + tick),
                    healthy("drv-b", "node-1"),
                ]),
                &no_reaps(),
                &fleet(&["node-1"]),
                10_000.0 + (tick as f64) * 10.0,
            ));
            assert!(wedged.is_empty(), "tick {tick}: {wedged:?}");
        }
    }

    /// merged_bug_034 / §5-S Q2 red 1: staggered shared-cause onset.
    /// Five of six nodes accumulate expiries one tick apart; under the
    /// retired instantaneous guard each node was Dead-reaped SERIALLY
    /// as it crossed the per-node threshold (affected=1 per tick — the
    /// ratio never trips), rolling reaps across a fleet suffering a
    /// shared cause. The breadth axis suppresses per-node verdicts
    /// while most of the fleet bears evidence.
    // r[verify ctrl.nodeclaim.wedge-two-axis+6]
    #[test]
    fn staggered_onset_does_not_serially_reap() {
        let mut t = WedgeTracker::default();
        let reg = fleet(&["node-1", "node-2", "node-3", "node-4", "node-5", "node-6"]);
        // Tick 1: node-1 crosses the threshold (2 drvs); nodes 2-5
        // each show their FIRST expiry (shared cause ramping up).
        let view1 = vec![
            expired("a1", "node-1", 5),
            expired("a2", "node-1", 5),
            expired("b1", "node-2", 5),
            expired("c1", "node-3", 5),
            expired("d1", "node-4", 5),
            expired("e1", "node-5", 5),
        ];
        let v = t.update(Some(&view1), &no_reaps(), &reg, 10_000.0);
        match v {
            WedgeVerdict::NodeWedged(nodes, _) => assert!(
                nodes.is_empty(),
                "staggered onset Dead-reaped {nodes:?} before the ratio could trip"
            ),
            WedgeVerdict::Systemic { breadth, of, .. } => {
                assert!(breadth >= 5 && of == 6, "breadth {breadth} of {of}");
            }
            WedgeVerdict::Unobserved(_) => panic!("observed tick yielded Unobserved"),
        }
    }

    /// merged_bug_034 / §5-S Q2 red 2: traffic-lull denominator
    /// collapse. Two genuinely wedged nodes in an otherwise-idle
    /// EIGHT-node registered fleet: the retired guard's denominator
    /// was the per-tick attributed view (the two expiring nodes
    /// themselves), minting a false `Systemic{2, of: 2}` — which the
    /// episode drain+latch then made sticky, permanently suppressing
    /// the real per-node signal. The fleet-derived denominator keeps
    /// the verdict per-node.
    // r[verify ctrl.nodeclaim.wedge-two-axis+6]
    #[test]
    fn traffic_lull_does_not_mint_false_systemic() {
        let mut t = WedgeTracker::default();
        let reg = fleet(&[
            "node-1", "node-2", "node-3", "node-4", "node-5", "node-6", "node-7", "node-8",
        ]);
        let view = vec![
            expired("a1", "node-1", 5),
            expired("a2", "node-1", 5),
            expired("b1", "node-2", 5),
            expired("b2", "node-2", 5),
        ];
        let v = t.update(Some(&view), &no_reaps(), &reg, 10_000.0);
        match v {
            WedgeVerdict::NodeWedged(nodes, _) => assert_eq!(
                nodes,
                vec!["node-1".to_string(), "node-2".to_string()],
                "two wedged nodes in an idle 8-node fleet are per-node verdicts"
            ),
            WedgeVerdict::Systemic { affected, of, .. } => {
                panic!("traffic lull minted a false systemic verdict ({affected}/{of})")
            }
            WedgeVerdict::Unobserved(_) => panic!("observed tick yielded Unobserved"),
        }
    }

    /// §5-S Q2 dwell axis: after a ratio-systemic episode latches the
    /// watermark, per-node verdicts stay disabled for
    /// WEDGE_VERDICT_DWELL_SECS (the trailing edge of an episode is
    /// not a sequence of fresh per-node wedges), then re-enable.
    // r[verify ctrl.nodeclaim.wedge-two-axis+6]
    #[test]
    fn post_episode_dwell_gates_per_node_verdicts() {
        let mut t = WedgeTracker::default();
        let reg = fleet(&["node-1", "node-2", "node-3", "node-4", "node-5", "node-6"]);
        // Ratio episode: 4 of 6 wedged.
        let storm: Vec<OpenAttempt> = (1..=4)
            .flat_map(|n| {
                vec![
                    expired(&format!("s{n}x"), &format!("node-{n}"), 5),
                    expired(&format!("s{n}y"), &format!("node-{n}"), 5),
                ]
            })
            .collect();
        assert!(matches!(
            t.update(Some(&storm), &no_reaps(), &reg, 10_000.0),
            WedgeVerdict::Systemic { .. }
        ));
        // 60s later (inside the dwell): node-6 shows two FRESH
        // expiries (assigned after the watermark — they admit), but
        // per-node verdicts are still dwell-gated.
        fn fresh(intent: &str, node: &str, assigned_pg: u64, now: f64) -> OpenAttempt {
            OpenAttempt {
                assigned_at_epoch_secs: assigned_pg,
                assigned_at_age_secs: (now as u64) - assigned_pg,
                ..expired(intent, node, 0)
            }
        }
        let v = t.update(
            Some(&[
                fresh("f1", "node-6", 9_931, 10_065.0),
                fresh("f2", "node-6", 9_931, 10_065.0),
            ]),
            &no_reaps(),
            &reg,
            10_065.0,
        );
        assert!(
            matches!(v, WedgeVerdict::Systemic { .. }),
            "per-node verdicts must stay dwell-gated 60s post-episode: {v:?}"
        );
        // Past the dwell: the same still-open fresh pair re-detects.
        let late = 10_000.0 + WEDGE_VERDICT_DWELL_SECS + 10.0;
        let v = t.update(
            Some(&[
                fresh("f1", "node-6", 9_931, late),
                fresh("f2", "node-6", 9_931, late),
            ]),
            &no_reaps(),
            &reg,
            late,
        );
        match v {
            WedgeVerdict::NodeWedged(nodes, _) => {
                assert_eq!(nodes, vec!["node-6".to_string()])
            }
            other => panic!("post-dwell re-detect failed: {other:?}"),
        }
    }

    /// merged_bug_018: one physical expiry must compare identically
    /// against the suppression watermark every tick. The retired law
    /// reconstructed the expiry as `now − over` (PG-clock age vs the
    /// controller's tick-START clock): ±(RPC preamble + flooring) of
    /// jitter, so a row expiring within jitter of the watermark
    /// FLIPPED to "newer than the suppression" on a later tick and
    /// re-anchored — the trailing-edge reap the watermark exists to
    /// prevent. PG-frame admission (`assigned_at + deadline + grace`
    /// vs the episode's max PG-frame expiry) is jitter-free.
    // r[verify ctrl.nodeclaim.wedge-two-axis+6]
    #[test]
    fn pg_frame_admission_is_jitter_stable() {
        fn row(intent: &str, node: &str, assigned_pg: u64, age: u64) -> OpenAttempt {
            OpenAttempt {
                assigned_at_epoch_secs: assigned_pg,
                assigned_at_age_secs: age,
                ..expired(intent, node, 0)
            }
        }
        let mut t = WedgeTracker::default();
        // Four rows, two nodes, all assigned at PG 9_869 with a 100s
        // deadline → every expiry_pg = 9_869 + 100 + 30 = 9_999, one
        // second before the systemic verdict at now = 10_000
        // (true age there: 131 → over = 1).
        let storm: Vec<OpenAttempt> = [
            ("a1", "node-1"),
            ("a2", "node-1"),
            ("b1", "node-2"),
            ("b2", "node-2"),
        ]
        .iter()
        .map(|(i, n)| row(i, n, 9_869, 131))
        .collect();
        assert!(matches!(
            t.update(
                Some(&storm),
                &no_reaps(),
                &fleet(&["node-1", "node-2"]),
                10_000.0
            ),
            WedgeVerdict::Systemic { .. }
        ));
        // Trailing tick at 10_005: the SAME still-open rows re-present.
        // True age is 136, but the ledger computed it 2 s earlier in
        // the RPC (age 134) while the controller stamps tick-start
        // time — the retired reconstruction yields 10_005 − 3 =
        // 10_002 > 10_000 and ADMITS the suppressed episode's rows.
        let laggard: Vec<OpenAttempt> = [("a1", "node-1"), ("a2", "node-1")]
            .iter()
            .map(|(i, n)| row(i, n, 9_869, 134))
            .collect();
        let v = t.update(
            Some(&laggard),
            &no_reaps(),
            &fleet(&["node-1", "node-2"]),
            10_005.0,
        );
        match v {
            WedgeVerdict::NodeWedged(nodes, _) => assert!(
                nodes.is_empty(),
                "one physical expiry flipped admission under reconstruction jitter: {nodes:?}"
            ),
            other => panic!("unexpected verdict {other:?}"),
        }
    }

    /// merged_bug_017: reap eviction is an ADMISSION source, not a
    /// state wipe. A reaped node's still-open attempts re-present in
    /// the very next view (the establishment sweep takes several ticks
    /// to close them); the one-shot `mem::take`-era wipe let `observe`
    /// re-anchor them the same tick, re-feeding the Dead arm with a
    /// node that no longer exists.
    // r[verify ctrl.nodeclaim.wedge-cluster+3]
    #[test]
    fn reaped_node_still_open_attempts_are_inadmissible() {
        let mut tracker = WedgeTracker::default();
        let view = [expired("drv-a", "node-1", 5), expired("drv-b", "node-1", 5)];
        let wedged = nodes(tracker.update(Some(&view), &no_reaps(), &fleet(&["node-1"]), 10_000.0));
        assert_eq!(wedged, vec!["node-1".to_string()]);
        // The controller reaps node-1; the SAME still-open attempts
        // re-present next tick.
        let reaps: std::collections::BTreeSet<String> = ["node-1".to_string()].into();
        let verdict = tracker.update(Some(&view), &reaps, &fleet(&[]), 10_010.0);
        assert!(
            nodes(verdict).is_empty(),
            "a reaped node's still-open attempts must be inadmissible, not re-anchored"
        );
        // ... and they stay inadmissible on later ticks while the
        // tombstone lives (the reap feed only fires once).
        let verdict = tracker.update(Some(&view), &no_reaps(), &fleet(&[]), 10_020.0);
        assert!(
            nodes(verdict).is_empty(),
            "tombstone must outlive the one-shot reap feedback"
        );
    }

    /// merged_bug_017: a node absent from the registered NodeClaim
    /// fleet (deleted out-of-band, Karpenter GC) is evicted exactly
    /// like a reaped one — its evidence cannot mark and does not
    /// inflate the populations.
    // r[verify ctrl.nodeclaim.wedge-cluster+3]
    #[test]
    fn fleet_absent_node_is_evicted_and_inadmissible() {
        let mut tracker = WedgeTracker::default();
        let view = [expired("drv-a", "node-1", 5), expired("drv-b", "node-1", 5)];
        // node-1 was registered when its evidence accumulated...
        let wedged = nodes(tracker.update(Some(&view), &no_reaps(), &fleet(&["node-1"]), 10_000.0));
        assert_eq!(wedged, vec!["node-1".to_string()]);
        // ...then vanished from the NodeClaim list between ticks.
        let verdict = tracker.update(Some(&view), &no_reaps(), &fleet(&[]), 10_010.0);
        assert!(
            nodes(verdict).is_empty(),
            "a fleet-absent node's evidence must be evicted and inadmissible"
        );
    }

    /// merged_bug_024: a Karpenter-GC'd node with NO prior evidence
    /// (never controller-reaped, never in `evidence`) must be refused
    /// by the admission authority's fleet-absence leg and tombstoned
    /// on first refusal — never admitted into wedged/breadth/
    /// population. Recorded red on the pre-authority code: tick 1
    /// returned `Systemic{2, of: 3}` (two wedged ghosts + one
    /// registered node), whose whole-episode drain + watermark latch
    /// then blocked n1's genuine first expiry from ever pairing — a
    /// verdict blackout undone by nothing.
    // r[verify ctrl.nodeclaim.wedge-cluster+3]
    #[test]
    fn ghost_nodes_cannot_inflate_verdict_populations() {
        /// PG-frame production posture (Q1): `assigned_at_epoch_secs`
        /// set, age consistent with `now`.
        fn pg_expired(intent: &str, node: &str, assigned_pg: u64, now: f64) -> OpenAttempt {
            OpenAttempt {
                assigned_at_epoch_secs: assigned_pg,
                assigned_at_age_secs: (now as u64) - assigned_pg,
                ..expired(intent, node, 0)
            }
        }
        let mut t = WedgeTracker::default();
        let reg = fleet(&["node-1"]);
        // Tick 1 at 10_000: two ghosts (deleted out-of-band before any
        // observation) each carry 2 distinct expired drvs; node-1's
        // genuine pair is still building (first expiry only). All rows
        // are well past deadline(100) + grace(30).
        let view1 = vec![
            pg_expired("ga1", "ghost-1", 9_800, 10_000.0),
            pg_expired("ga2", "ghost-1", 9_800, 10_000.0),
            pg_expired("gb1", "ghost-2", 9_800, 10_000.0),
            pg_expired("gb2", "ghost-2", 9_800, 10_000.0),
            pg_expired("n1a", "node-1", 9_820, 10_000.0),
        ];
        match t.update(Some(&view1), &no_reaps(), &reg, 10_000.0) {
            WedgeVerdict::Systemic { affected, of, .. } => {
                panic!("ghosts minted a false systemic verdict ({affected}/{of})")
            }
            WedgeVerdict::NodeWedged(nodes, _) => assert!(
                nodes.is_empty(),
                "one genuine expiry must not mark; ghosts must not mark: {nodes:?}"
            ),
            WedgeVerdict::Unobserved(_) => panic!("observed tick yielded Unobserved"),
        }
        // First refusal minted the ghosts' tombstones (the literal
        // "tombstoned for one window" conformance), and no episode
        // was drained: no suppression watermark latched.
        assert!(
            t.evicted.contains_key("ghost-1") && t.evicted.contains_key("ghost-2"),
            "ghost refusal must mint eviction tombstones: {:?}",
            t.evicted
        );
        assert!(
            t.last_suppression.is_none(),
            "no episode existed; nothing may latch"
        );
        // Tick 2: node-1's second derivation expires (fresh attempt,
        // PG-frame). The genuine pair must mark — the recorded red's
        // counterfactual was a drained episode + latch blocking
        // exactly this admission.
        let view2 = vec![
            pg_expired("n1a", "node-1", 9_820, 10_010.0),
            pg_expired("n1b", "node-1", 9_879, 10_010.0),
            // The ghosts' still-open attempts re-present and stay
            // refused on the tombstone leg.
            pg_expired("ga1", "ghost-1", 9_800, 10_010.0),
        ];
        match t.update(Some(&view2), &no_reaps(), &reg, 10_010.0) {
            WedgeVerdict::NodeWedged(nodes, _) => {
                assert_eq!(nodes, vec!["node-1".to_string()])
            }
            other => panic!("genuine pair must mark on tick 2, got {other:?}"),
        }
    }

    /// Evidence ages out of the 30-minute window: two expiries observed
    /// far apart never coexist inside one window, so the node is not
    /// marked; once both are inside the window it is.
    // r[verify ctrl.nodeclaim.wedge-cluster+3]
    #[test]
    fn evidence_outside_the_window_does_not_count() {
        let mut tracker = WedgeTracker::default();
        let t0 = 10_000.0;
        assert!(
            nodes(tracker.update(
                Some(&[expired("drv-a", "node-1", 5)]),
                &no_reaps(),
                &fleet(&["node-1"]),
                t0
            ))
            .is_empty()
        );
        // Second distinct expiry observed after the first aged out.
        let late = t0 + WEDGE_CLUSTER_WINDOW_SECS + 1.0;
        assert!(
            nodes(tracker.update(
                Some(&[expired("drv-b", "node-1", 5)]),
                &no_reaps(),
                &fleet(&["node-1"]),
                late
            ))
            .is_empty(),
            "the drv-a evidence aged out; one in-window expiry must not mark"
        );
        // Both inside one window → marked.
        let wedged = nodes(tracker.update(
            Some(&[expired("drv-a", "node-1", 5), expired("drv-b", "node-1", 5)]),
            &no_reaps(),
            &fleet(&["node-1"]),
            late + 5.0,
        ));
        assert_eq!(wedged, vec!["node-1".to_string()]);
    }

    /// Attempts with an unknown deadline (0) are never evidence.
    // r[verify ctrl.nodeclaim.wedge-cluster+3]
    #[test]
    fn unknown_deadline_is_not_evidence() {
        let mut tracker = WedgeTracker::default();
        let mut a = expired("drv-a", "node-1", 5);
        a.deadline_secs = 0;
        let mut b = expired("drv-b", "node-1", 5);
        b.deadline_secs = 0;
        assert!(
            nodes(tracker.update(Some(&[a, b]), &no_reaps(), &fleet(&["node-1"]), 10_000.0))
                .is_empty()
        );
    }

    /// C2/222 leg 1: materialization attempts are store-side fetches —
    /// their deadline expiry says nothing about a *node* (the stamped
    /// source_node is the stale builder binding). They are never wedge
    /// evidence.
    // r[verify ctrl.nodeclaim.wedge-two-axis+6]
    #[test]
    fn materialization_attempts_are_never_wedge_evidence() {
        let mut tracker = WedgeTracker::default();
        let mut a = expired("drv-a", "node-1", 5);
        a.attempt_kind = rio_proto::types::AttemptKind::Materialization as i32;
        let mut b = expired("drv-b", "node-1", 5);
        b.attempt_kind = rio_proto::types::AttemptKind::Materialization as i32;
        let wedged =
            nodes(tracker.update(Some(&[a, b]), &no_reaps(), &fleet(&["node-1"]), 10_000.0));
        assert!(
            wedged.is_empty(),
            "two expired MATERIALIZATION attempts on one node must not mark it: {wedged:?}"
        );
    }

    /// C2/222 leg 2: an attempt the ledger cannot attribute to a node
    /// (empty source_node) is never evidence — the newest-pod-wins
    /// in-memory binding attributes an old attempt's expiry to the
    /// *replacement* pod's healthy node.
    // r[verify ctrl.nodeclaim.wedge-two-axis+6]
    #[test]
    fn empty_source_node_never_attributes() {
        let mut tracker = WedgeTracker::default();
        let mut a = expired("drv-a", "", 5);
        a.source_node = String::new();
        let mut b = expired("drv-b", "", 5);
        b.source_node = String::new();
        let wedged = nodes(tracker.update(Some(&[a, b]), &no_reaps(), &fleet(&[]), 10_000.0));
        assert!(
            wedged.is_empty(),
            "ledger-unattributable expiries must never mark any node: {wedged:?}"
        );
    }

    /// C2/077 gap 2: the cluster window anchors at FIRST observation.
    /// One stuck derivation re-observed every tick for over a window,
    /// plus a second derivation expiring at the very end, must not
    /// mark: the two expiries are hours apart even though both were
    /// "recently observed".
    // r[verify ctrl.nodeclaim.wedge-two-axis+6]
    #[test]
    fn evidence_window_anchors_at_first_observation() {
        let mut tracker = WedgeTracker::default();
        let t0 = 10_000.0;
        // drv-a stays expired-and-open, re-observed every 600s well past
        // the 1800s window.
        let mut t = t0;
        while t < t0 + WEDGE_CLUSTER_WINDOW_SECS + 600.0 {
            let wedged = nodes(tracker.update(
                Some(&[expired("drv-a", "node-1", 5)]),
                &no_reaps(),
                &fleet(&["node-1"]),
                t,
            ));
            assert!(wedged.is_empty(), "single drv must never mark: {wedged:?}");
            t += 600.0;
        }
        // drv-b expires now — drv-a's FIRST observation is > window ago.
        let wedged = tracker.update(
            Some(&[expired("drv-a", "node-1", 5), expired("drv-b", "node-1", 5)]),
            &no_reaps(),
            &fleet(&["node-1"]),
            t,
        );
        let wedged = match wedged {
            WedgeVerdict::NodeWedged(n, _) => n,
            // A 1-node attributed fleet with 1 clustered node is not
            // systemic by the >=2 affected guard; reaching here means
            // the anchor regressed.
            WedgeVerdict::Systemic { affected, of, .. } => {
                panic!("unexpected systemic verdict ({affected}/{of})")
            }
            WedgeVerdict::Unobserved(_) => panic!("unexpected unobserved verdict"),
        };
        assert!(
            wedged.is_empty(),
            "expiries first observed > window apart must not cluster: {wedged:?}"
        );
    }

    /// C2/077 gap 1: when MOST attributed nodes are accumulating
    /// expiries the cause is systemic (scheduler/report-path outage,
    /// store brownout), not per-node wedges — marking nothing beats
    /// rolling Dead-reaps across the fleet at dead_reap_cap.
    // r[verify ctrl.nodeclaim.wedge-two-axis+6]
    #[test]
    fn fleet_wide_expiry_is_systemic_and_marks_nothing() {
        let mut tracker = WedgeTracker::default();
        let view: Vec<OpenAttempt> = (0..4)
            .flat_map(|n| {
                vec![
                    expired(&format!("drv-{n}-x"), &format!("node-{n}"), 5),
                    expired(&format!("drv-{n}-y"), &format!("node-{n}"), 5),
                ]
            })
            .collect();
        let verdict = tracker.update(
            Some(&view),
            &no_reaps(),
            &fleet(&["node-0", "node-1", "node-2", "node-3"]),
            10_000.0,
        );
        assert!(
            matches!(
                verdict,
                WedgeVerdict::Systemic {
                    affected: 4,
                    of: 4,
                    ..
                }
            ),
            "all-nodes-expiring is systemic; the Dead input must be empty: {verdict:?}"
        );
    }
}

/// C2 formal-delta proptest plane (bughunt-2 slot 5): the wedge
/// verdict laws over 2-5-tick TRAJECTORIES, checked against an
/// independent set-algebra oracle — NOT a mirror of the
/// implementation (the retired single-tick mirror restated the fold
/// and would have been wrong together with it). The oracle computes,
/// from the raw trajectory alone: which (node, drv) anchors are live
/// in each tick's window (first-observation anchored, eviction- and
/// drain-aware), and from that the expected verdict populations.
// r[verify ctrl.nodeclaim.wedge-two-axis+6]
#[cfg(test)]
mod proptests {
    use proptest::prelude::*;
    use rio_proto::types::OpenAttempt;
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    const DRVS: [&str; 3] = ["d0", "d1", "d2"];
    /// Attribution universe: "" (unattributable), three REGISTERED
    /// nodes, and one ghost (merged_bug_024: ledger-attributed but
    /// outside the registered fleet — refused + tombstoned by the
    /// admission authority, never evidence).
    const NODES: [&str; 5] = ["", "n0", "n1", "n2", "g0"];
    /// The registered NodeClaim fleet (constant across a trajectory);
    /// `g0` is deliberately outside it.
    const REGISTERED: [&str; 3] = ["n0", "n1", "n2"];

    /// One trajectory tick: the observed view (None = RPC failure),
    /// the backing nodes reaped since the last tick, and the gap to
    /// the PREVIOUS tick (merged_bug_023: window-crossing and
    /// dwell-crossing gaps make breadth-decay and release edges
    /// reachable inside a 5-tick trajectory — anchors age out,
    /// watermarks and tombstones expire).
    #[derive(Debug, Clone)]
    struct Tick {
        view: Option<Vec<OpenAttempt>>,
        reaped: BTreeSet<String>,
        gap_secs: f64,
    }

    fn arb_attempt() -> impl Strategy<Value = OpenAttempt> {
        (
            proptest::sample::select(&DRVS[..]),
            proptest::sample::select(&NODES[..]),
            prop_oneof![
                3 => Just(rio_proto::types::AttemptKind::Build as i32),
                1 => Just(rio_proto::types::AttemptKind::Materialization as i32),
                1 => Just(0i32),
            ],
            prop_oneof![Just(0u64), Just(10u64)],
            prop_oneof![Just(0u64), Just(5u64), Just(100u64)],
        )
            .prop_map(|(intent, node, kind, deadline, age)| OpenAttempt {
                intent_id: intent.to_owned(),
                source_node: node.to_owned(),
                attempt_kind: kind,
                deadline_secs: deadline,
                assigned_at_age_secs: age,
                ..Default::default()
            })
    }

    fn arb_tick() -> impl Strategy<Value = Tick> {
        (
            prop_oneof![
                4 => proptest::collection::vec(arb_attempt(), 0..=8).prop_map(Some),
                1 => Just(None),
            ],
            proptest::collection::btree_set(
                // Reap feedback covers registered claims only (the
                // controller cannot reap a ghost's NodeClaim — it has
                // none); ghost conclusiveness flows through the
                // admission authority's fleet-absence leg instead.
                proptest::sample::select(&REGISTERED[..]).prop_map(str::to_owned),
                0..=2,
            ),
            prop_oneof![
                4 => Just(10.0f64),
                1 => Just(WEDGE_VERDICT_DWELL_SECS + 1.0),
                1 => Just(WEDGE_CLUSTER_WINDOW_SECS + 1.0),
            ],
        )
            .prop_map(|(view, reaped, gap_secs)| Tick {
                view,
                reaped,
                gap_secs,
            })
    }

    /// Independent oracle state: live anchors per (node, drv) and the
    /// expected marked set, evolved by SET ALGEBRA over the trajectory
    /// (window arithmetic, first-anchor keep, eviction removal,
    /// drain-on-systemic), never by calling the implementation.
    #[derive(Default)]
    struct Oracle {
        anchors: BTreeMap<(String, String), f64>,
        marked: BTreeSet<String>,
        /// merged_bug_163 mirror: the suppression watermark. The
        /// established CONSERVATIVE approximation: set to the drain
        /// tick's `now` (every drained expiry precedes it), for the
        /// ratio drain and (merged_bug_023) the breadth close alike.
        watermark: Option<f64>,
        /// merged_bug_017 mirror: eviction tombstones (node → instant).
        evicted: BTreeMap<String, f64>,
        /// merged_bug_023 mirror: the engaged episode's axis as of
        /// the last observed engaged tick (true = the retaining
        /// breadth phase; false = ratio/dwell, whose release is a
        /// no-op). None = disengaged.
        engaged_retaining: Option<bool>,
    }

    enum Expect {
        Skipped,
        PerNode(Vec<String>),
        Systemic {
            affected: usize,
            of: usize,
            breadth: usize,
        },
    }

    impl Oracle {
        fn step(&mut self, tick: &Tick, now: f64) -> Expect {
            for n in &tick.reaped {
                self.anchors.retain(|(node, _), _| node != n);
                self.marked.remove(n);
                self.evicted.insert(n.clone(), now);
            }
            let Some(view) = &tick.view else {
                return Expect::Skipped;
            };
            let mut fleet: BTreeSet<String> = BTreeSet::new();
            for a in view {
                if a.attempt_kind != rio_proto::types::AttemptKind::Build as i32
                    || a.source_node.is_empty()
                    || self.evicted.contains_key(&a.source_node)
                {
                    continue;
                }
                // merged_bug_024 admission law (independent set
                // algebra): evidence requires registered ∧
                // not-tombstoned; a fleet-absent refusal with no live
                // tombstone mints one (once per absence episode).
                if !REGISTERED.contains(&a.source_node.as_str()) {
                    self.evicted.entry(a.source_node.clone()).or_insert(now);
                    continue;
                }
                fleet.insert(a.source_node.clone());
                if a.deadline_secs == 0
                    || a.assigned_at_age_secs <= a.deadline_secs + WEDGE_DEADLINE_GRACE_SECS
                {
                    continue;
                }
                // Watermark admission (merged_bug_163): set algebra
                // over the raw trajectory, independent of the impl.
                let over = a.assigned_at_age_secs - (a.deadline_secs + WEDGE_DEADLINE_GRACE_SECS);
                let expiry = now - over as f64;
                if self
                    .watermark
                    .is_some_and(|w| expiry <= w && now - w <= WEDGE_CLUSTER_WINDOW_SECS)
                {
                    continue;
                }
                self.anchors
                    .entry((a.source_node.clone(), a.intent_id.clone()))
                    .or_insert(now);
            }
            self.anchors
                .retain(|_, t| now - *t <= WEDGE_CLUSTER_WINDOW_SECS);
            // Tombstones prune AFTER the view fold — the impl's order
            // (a tombstone at its TTL boundary still refuses within
            // the tick that retires it).
            self.evicted
                .retain(|_, t| now - *t <= WEDGE_CLUSTER_WINDOW_SECS);
            let _ = fleet;
            let mut per_node: BTreeMap<&str, usize> = BTreeMap::new();
            for (node, _) in self.anchors.keys() {
                *per_node.entry(node.as_str()).or_default() += 1;
            }
            let wedged: Vec<String> = per_node
                .iter()
                .filter(|(_, c)| **c >= WEDGE_CLUSTER_MIN_DISTINCT_DRVS)
                .map(|(n, _)| (*n).to_owned())
                .collect();
            let evidence_nodes: BTreeSet<&str> =
                self.anchors.keys().map(|(n, _)| n.as_str()).collect();
            // Q2: registered-fleet denominator (constant across the
            // trajectory; the ghost is outside it by construction).
            let registered: BTreeSet<&str> = REGISTERED.iter().copied().collect();
            let population = evidence_nodes
                .iter()
                .copied()
                .chain(registered.iter().copied())
                .collect::<BTreeSet<_>>()
                .len();
            let breadth = evidence_nodes.len();
            let systemic = wedged.len() >= 2
                && (wedged.len() as f64 / population as f64) > WEDGE_SYSTEMIC_FRACTION;
            let breadth_suppressed =
                breadth >= 2 && (breadth as f64 / population as f64) > WEDGE_SYSTEMIC_FRACTION;
            let dwell_active = self
                .watermark
                .is_some_and(|w| now - w <= WEDGE_VERDICT_DWELL_SECS);
            if systemic {
                let affected = wedged.len();
                // Whole-episode drain + latch (merged_bug_163);
                // ratio engages the episode in the NON-retaining
                // phase (its release is a no-op).
                self.anchors.clear();
                self.watermark = Some(now);
                self.marked.clear();
                self.engaged_retaining = Some(false);
                Expect::Systemic {
                    affected,
                    of: population,
                    breadth,
                }
            } else if breadth_suppressed || (dwell_active && !wedged.is_empty()) {
                // Trajectory suppression: NO drain, NO latch — and
                // the marked transition-memory is RETAINED
                // (merged_bug_016: draining it made the next per-node
                // tick re-count a continuous wedge). Breadth engages
                // the RETAINING phase; dwell the non-retaining one
                // (its release must not drain fresh post-watermark
                // evidence).
                self.engaged_retaining = Some(breadth_suppressed);
                Expect::Systemic {
                    affected: wedged.len(),
                    of: population,
                    breadth,
                }
            } else if self.engaged_retaining.take() == Some(true) {
                // merged_bug_023: the breadth release edge — the
                // close drains the whole window, latches (the
                // conservative now-approximation) and drains marked;
                // the tick reports a suppressed verdict with the
                // pre-close populations.
                self.anchors.clear();
                self.watermark = Some(now);
                self.marked.clear();
                Expect::Systemic {
                    affected: wedged.len(),
                    of: population,
                    breadth,
                }
            } else {
                // (A no-op ratio/dwell release was consumed by the
                // `take()` above when present.)
                self.marked = wedged.iter().cloned().collect();
                Expect::PerNode(wedged)
            }
        }
    }

    proptest! {
        /// merged_bug_018 ground-truth law: a row whose PG-frame
        /// expiry is at or before the suppression watermark is NEVER
        /// admitted, for any per-tick age jitter and any observation
        /// instant — including window-crossing trailing ticks. (The
        /// retired reconstruction admitted it whenever the jittered
        /// `now − over` landed past the controller-frame watermark.)
        #[test]
        fn suppressed_pg_expiries_never_readmit_under_jitter(
            jitters in proptest::collection::vec(0u64..=3, 1..=4),
            gap in 1u64..=1900,
        ) {
            let assigned_pg = 9_869u64;
            let deadline = 100u64;
            let expiry_pg = assigned_pg + deadline + WEDGE_DEADLINE_GRACE_SECS;
            let mk = |intent: &str, node: &str, age: u64| OpenAttempt {
                intent_id: intent.to_owned(),
                source_node: node.to_owned(),
                attempt_kind: rio_proto::types::AttemptKind::Build as i32,
                deadline_secs: deadline,
                assigned_at_epoch_secs: assigned_pg,
                assigned_at_age_secs: age,
                ..Default::default()
            };
            let registered: HashSet<String> = ["n1", "n2"].iter().map(|s| s.to_string()).collect();
            let mut t = WedgeTracker::default();
            let t0 = (expiry_pg + 1) as f64;
            let storm: Vec<OpenAttempt> = [("a1", "n1"), ("a2", "n1"), ("b1", "n2"), ("b2", "n2")]
                .iter().map(|(i, n)| mk(i, n, (t0 as u64) - assigned_pg)).collect();
            match t.update(Some(&storm), &std::collections::BTreeSet::new(), &registered, t0) {
                WedgeVerdict::Systemic { .. } => {}
                v => return Err(TestCaseError::fail(format!("expected systemic, got {v:?}"))),
            }
            // Trailing ticks: same physical rows, jittered ages, at
            // arbitrary later instants (including past the window).
            for (k, j) in jitters.iter().enumerate() {
                let now = t0 + (gap as f64) * ((k + 1) as f64);
                let true_age = (now as u64) - assigned_pg;
                let rows: Vec<OpenAttempt> = [("a1", "n1"), ("a2", "n1")]
                    .iter().map(|(i, n)| mk(i, n, true_age.saturating_sub(*j))).collect();
                let in_window = now - t0 <= WEDGE_CLUSTER_WINDOW_SECS;
                match t.update(Some(&rows), &std::collections::BTreeSet::new(), &registered, now) {
                    WedgeVerdict::NodeWedged(nodes, _) => prop_assert!(
                        // Within the watermark window: never admitted,
                        // for any jitter. Past it: merged_bug_060's
                        // TTL law applies — a still-wedged participant
                        // MAY re-detect (pinned by the unit test).
                        !in_window || nodes.is_empty(),
                        "suppressed expiry re-admitted at tick {} (jitter {}): {:?}", k, j, nodes
                    ),
                    WedgeVerdict::Systemic { .. } | WedgeVerdict::Unobserved(_) => {}
                }
            }
        }

        /// Trajectory law: over 2-5 ticks of arbitrary views, RPC
        /// failures and reaps, the tracker's verdicts match the
        /// set-algebra oracle tick for tick — populations
        /// commensurable, drain-on-systemic, eviction honored,
        /// skip-on-None.
        #[test]
        fn trajectory_matches_set_algebra_oracle(
            ticks in proptest::collection::vec(arb_tick(), 2..=5),
        ) {
            let mut tracker = WedgeTracker::default();
            let mut oracle = Oracle::default();
            let mut now = 1_000_000.0;
            for (i, tick) in ticks.iter().enumerate() {
                now += tick.gap_secs;
                let registered: HashSet<String> =
                    REGISTERED.iter().map(|s| s.to_string()).collect();
                let verdict = tracker.update(tick.view.as_deref(), &tick.reaped, &registered, now);
                match (oracle.step(tick, now), verdict) {
                    (Expect::Skipped, WedgeVerdict::Unobserved(_)) => {}
                    (Expect::Skipped, v) => {
                        return Err(TestCaseError::fail(format!("tick {i}: skip produced {v:?}")));
                    }
                    (Expect::PerNode(exp), WedgeVerdict::NodeWedged(nodes, _)) => {
                        prop_assert_eq!(nodes, exp, "tick {}", i);
                    }
                    (Expect::Systemic { affected, of, breadth }, WedgeVerdict::Systemic { affected: a, of: o, breadth: b, .. }) => {
                        prop_assert_eq!(a, affected, "tick {}", i);
                        prop_assert_eq!(o, of, "tick {}", i);
                        prop_assert_eq!(b, breadth, "tick {}", i);
                        prop_assert!(a <= o, "tick {i}: incommensurable {a}/{o}");
                    }
                    (Expect::PerNode(exp), v) => {
                        return Err(TestCaseError::fail(format!(
                            "tick {i}: expected per-node {exp:?}, got {v:?}"
                        )));
                    }
                    (Expect::Systemic { affected, of, .. }, v) => {
                        return Err(TestCaseError::fail(format!(
                            "tick {i}: expected systemic {affected}/{of}, got {v:?}"
                        )));
                    }
                }
                // merged_bug_016 retention law: the tracker's marked
                // transition-memory matches the oracle's set algebra
                // on EVERY tick — suppressed ticks retain, ratio
                // ticks drain, per-node ticks re-derive.
                let tracker_marked: BTreeSet<String> = tracker.marked.iter().cloned().collect();
                prop_assert_eq!(
                    tracker_marked,
                    oracle.marked.clone(),
                    "tick {}: marked set diverged from the oracle",
                    i
                );
            }
        }
    }

    /// Kani substitute (rio-controller is [[bin]]-ineligible for the
    /// kani driver — recorded none-sensible-for-kani per the bug_363
    /// precedent): EXHAUSTIVE single-tick verdict check over the full
    /// 8-node universe at the real constants — all 4^8 (node ∈
    /// {absent, healthy, qualifying, ghost-qualifying}) combinations
    /// (merged_bug_024 extended the 3^8 universe with the ghost
    /// state: a node OUTSIDE the registered fleet whose view rows
    /// carry 2 expired drvs). Closed-form expectation: ghosts are
    /// excluded from wedged, breadth AND population (the registered
    /// universe shrinks by the ghost count); affected ≤ of on every
    /// systemic verdict; every refused ghost is tombstoned.
    #[test]
    fn wedge_populations_exhaustive_bounded() {
        let nodes: Vec<String> = (0..8).map(|i| format!("n{i}")).collect();
        let mut checked = 0u32;
        // 4^8 assignments: 0 = absent (registered, no view rows),
        // 1 = healthy-only, 2 = qualifying, 3 = ghost-qualifying
        // (NOT registered, 2 expired drvs in the view).
        for mask in 0..4u32.pow(8) {
            let mut view: Vec<OpenAttempt> = Vec::new();
            let mut expect_wedged: Vec<String> = Vec::new();
            let mut ghosts: Vec<String> = Vec::new();
            let mut registered: HashSet<String> = HashSet::new();
            let mut m = mask;
            for n in &nodes {
                let state = m % 4;
                m /= 4;
                match state {
                    0 => {
                        registered.insert(n.clone());
                    }
                    1 => {
                        view.push(OpenAttempt {
                            intent_id: "h".into(),
                            source_node: n.clone(),
                            attempt_kind: rio_proto::types::AttemptKind::Build as i32,
                            deadline_secs: 100,
                            assigned_at_age_secs: 10,
                            ..Default::default()
                        });
                        registered.insert(n.clone());
                    }
                    s => {
                        for d in ["da", "db"] {
                            view.push(OpenAttempt {
                                intent_id: d.into(),
                                source_node: n.clone(),
                                attempt_kind: rio_proto::types::AttemptKind::Build as i32,
                                deadline_secs: 100,
                                assigned_at_age_secs: 100 + WEDGE_DEADLINE_GRACE_SECS + 5,
                                ..Default::default()
                            });
                        }
                        if s == 2 {
                            expect_wedged.push(n.clone());
                            registered.insert(n.clone());
                        } else {
                            ghosts.push(n.clone());
                        }
                    }
                }
            }
            expect_wedged.sort();
            // Q2 + merged_bug_024 closed form: population = registered
            // fleet ∪ evidence nodes = the registered fleet (ghosts
            // never enter evidence); breadth = the qualifying count.
            let population = registered.len();
            let systemic = expect_wedged.len() >= 2
                && (expect_wedged.len() as f64 / population as f64) > WEDGE_SYSTEMIC_FRACTION;
            let mut tracker = WedgeTracker::default();
            let verdict = tracker.update(
                Some(&view),
                &std::collections::BTreeSet::new(),
                &registered,
                1_000_000.0,
            );
            match verdict {
                WedgeVerdict::Systemic {
                    affected,
                    of,
                    breadth,
                    ..
                } => {
                    assert!(systemic, "mask {mask}: unexpected systemic");
                    assert!(affected <= of, "mask {mask}: {affected}/{of}");
                    assert_eq!(affected, expect_wedged.len(), "mask {mask}");
                    assert_eq!(of, population, "mask {mask}");
                    assert_eq!(breadth, expect_wedged.len(), "mask {mask}");
                }
                WedgeVerdict::NodeWedged(found, _) => {
                    assert!(!systemic, "mask {mask}: expected systemic");
                    assert_eq!(found, expect_wedged, "mask {mask}");
                }
                WedgeVerdict::Unobserved(_) => {
                    panic!("mask {mask}: observed view yielded Unobserved")
                }
            }
            for g in &ghosts {
                assert!(
                    tracker.evicted.contains_key(g),
                    "mask {mask}: refused ghost {g} must be tombstoned"
                );
                assert!(
                    !tracker.evidence.contains_key(g),
                    "mask {mask}: ghost {g} must hold no evidence"
                );
            }
            checked += 1;
        }
        assert_eq!(checked, 4u32.pow(8), "full universe covered");
    }
}

/// merged_bug_009 + merged_bug_176 regression battery (the wave's
/// recorded reds, now pinned green).
// r[verify ctrl.nodeclaim.wedge-two-axis+6]
#[cfg(test)]
mod population_and_epilogue_tests {
    use super::*;
    fn no_reaps() -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::new()
    }

    /// The registered NodeClaim fleet for a tick (backing node names).
    fn fleet(nodes: &[&str]) -> HashSet<String> {
        nodes.iter().map(|s| s.to_string()).collect()
    }

    fn expired(intent: &str, node: &str) -> OpenAttempt {
        OpenAttempt {
            intent_id: intent.into(),
            source_node: node.into(),
            assigned_at_age_secs: 100 + WEDGE_DEADLINE_GRACE_SECS + 5,
            deadline_secs: 100,
            attempt_kind: rio_proto::types::AttemptKind::Build as i32,
            ..Default::default()
        }
    }
    fn healthy(intent: &str, node: &str) -> OpenAttempt {
        OpenAttempt {
            assigned_at_age_secs: 10,
            ..expired(intent, node)
        }
    }

    /// This module's full node universe — registered every tick.
    fn all_nodes() -> HashSet<String> {
        fleet(&["n0", "n1", "n2", "n3", "n4", "n5"])
    }

    /// A systemic episode followed by an observation failure must not
    /// mass-mark from retained evidence (red: tick 2 returned
    /// NodeWedged(["n0","n1","n2","n3"]) — the mass-Dead-reap polarity).
    #[test]
    fn observation_failure_after_systemic_must_not_mass_mark() {
        let mut t = WedgeTracker::default();
        let view: Vec<OpenAttempt> = (0..4)
            .flat_map(|n| {
                vec![
                    expired(&format!("d{n}x"), &format!("n{n}")),
                    expired(&format!("d{n}y"), &format!("n{n}")),
                ]
            })
            .collect();
        assert!(matches!(
            t.update(Some(&view), &no_reaps(), &all_nodes(), 1000.0),
            WedgeVerdict::Systemic { .. }
        ));
        // tick 2: ListOpenAttempts failed -> mod.rs now passes None
        // (observation AND verdict skipped).
        match t.update(None, &no_reaps(), &all_nodes(), 1010.0) {
            WedgeVerdict::Unobserved(_) => {}
            v => panic!("observation-skipped tick must yield Unobserved, got {v:?}"),
        }
    }

    /// The verdict populations are commensurable (red:
    /// `Systemic{affected: 2, of: 1}` — retained evidence vs this-tick
    /// fleet).
    #[test]
    fn affected_never_exceeds_of() {
        let mut t = WedgeTracker::default();
        let tick1: Vec<OpenAttempt> = vec![
            expired("a1", "n1"),
            expired("a2", "n1"),
            expired("b1", "n2"),
            expired("b2", "n2"),
            healthy("c", "n3"),
            healthy("d", "n4"),
            healthy("e", "n5"),
        ];
        let v1 = t.update(Some(&tick1), &no_reaps(), &all_nodes(), 1000.0);
        assert!(
            matches!(v1, WedgeVerdict::NodeWedged(..)),
            "2/5 is not systemic: {v1:?}"
        );
        // tick 2: only n3 reports; n1/n2 evidence retained in-window.
        match t.update(
            Some(&[healthy("c", "n3")]),
            &no_reaps(),
            &all_nodes(),
            1010.0,
        ) {
            WedgeVerdict::Systemic { affected, of, .. } => assert!(
                affected <= of,
                "incommensurable systemic verdict: affected={affected} of={of}"
            ),
            WedgeVerdict::NodeWedged(..) => {}
            WedgeVerdict::Unobserved(_) => panic!("observed view yielded Unobserved"),
        }
    }

    /// Evidence for a node the controller already reaped is evicted
    /// through the REQUIRED argument (red: compile-red — no eviction
    /// input existed; behavior red: the dead node re-fed the Dead arm).
    #[test]
    fn reaped_node_evidence_is_evicted() {
        let mut t = WedgeTracker::default();
        let v = t.update(
            Some(&[expired("a1", "n1"), expired("a2", "n1"), healthy("c", "n3")]),
            &no_reaps(),
            &all_nodes(),
            1000.0,
        );
        assert!(matches!(v, WedgeVerdict::NodeWedged(ref n, _) if n == &vec!["n1".to_string()]));
        // The controller reaped n1's claim after that verdict; the
        // reaped set is now a REQUIRED argument (pre-fix the tracker
        // had no eviction input at all — compile-red — and retained
        // evidence re-marked the dead node every tick until the
        // window expired).
        let reaped: std::collections::BTreeSet<String> =
            std::iter::once("n1".to_string()).collect();
        match t.update(Some(&[healthy("c", "n3")]), &reaped, &all_nodes(), 1010.0) {
            WedgeVerdict::NodeWedged(nodes, _) => assert!(
                nodes.is_empty(),
                "reaped node still fed to the Dead arm from stale evidence: {nodes:?}"
            ),
            WedgeVerdict::Systemic { .. } => {}
            WedgeVerdict::Unobserved(_) => panic!("observed view yielded Unobserved"),
        }
    }

    /// A node wedged before a systemic episode counts a fresh
    /// marked-transition after it (red: marked stayed `{"n1"}` across
    /// the suppression — the early return froze the set).
    #[test]
    fn suppression_epilogue_drains_marked() {
        let mut t = WedgeTracker::default();
        let v = t.update(
            Some(&[
                expired("a1", "n1"),
                expired("a2", "n1"),
                healthy("c", "n2"),
                healthy("d", "n3"),
                healthy("e", "n4"),
            ]),
            &no_reaps(),
            &all_nodes(),
            1000.0,
        );
        assert!(matches!(v, WedgeVerdict::NodeWedged(ref n, _) if n == &vec!["n1".to_string()]));
        // Systemic episode.
        let view: Vec<OpenAttempt> = (0..4)
            .flat_map(|n| {
                vec![
                    expired(&format!("s{n}x"), &format!("n{n}")),
                    expired(&format!("s{n}y"), &format!("n{n}")),
                ]
            })
            .collect();
        assert!(matches!(
            t.update(Some(&view), &no_reaps(), &all_nodes(), 1010.0),
            WedgeVerdict::Systemic { .. }
        ));
        // After the episode the marked set must have been re-derived -
        // today the early return freezes it with n1 inside, so a later
        // genuine re-wedge of n1 never counts a transition. Observable
        // here: the suppressed episode left stale marked entries.
        assert!(
            t.marked.is_empty(),
            "suppression epilogue must drain the marked set: {:?}",
            t.marked
        );
    }
}

/// merged_bug_016 + bug_061 regression battery: the typed per-arm
/// epilogue effects record — suppressed ticks RETAIN the marked
/// transition-memory, and the suppression counter ticks on EVERY
/// suppressed tick labeled by the engaging axis. Witnesses per Q1:
/// DebuggingRecorder over the production counter names, sequences
/// driven only through `update()` with PG-frame production-shaped
/// `OpenAttempt`s.
// r[verify ctrl.nodeclaim.wedge-two-axis+6]
#[cfg(test)]
mod epilogue_effects_tests {
    use super::*;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    fn no_reaps() -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::new()
    }
    fn fleet(nodes: &[&str]) -> HashSet<String> {
        nodes.iter().map(|s| s.to_string()).collect()
    }
    /// PG-frame production posture (Q1): epoch set, age consistent.
    fn pg_expired(intent: &str, node: &str, assigned_pg: u64, now: f64) -> OpenAttempt {
        OpenAttempt {
            intent_id: intent.into(),
            source_node: node.into(),
            assigned_at_epoch_secs: assigned_pg,
            assigned_at_age_secs: (now as u64) - assigned_pg,
            deadline_secs: 100,
            attempt_kind: rio_proto::types::AttemptKind::Build as i32,
            ..Default::default()
        }
    }
    /// Sum a counter's value across label sets, optionally requiring
    /// an `axis` label value. Takes a materialized snapshot — the
    /// DebuggingRecorder's `snapshot()` DRAINS counters (swap-to-0),
    /// so a test must snapshot exactly once.
    type SnapshotEntries = Vec<(
        metrics_util::CompositeKey,
        Option<metrics::Unit>,
        Option<metrics::SharedString>,
        DebugValue,
    )>;
    fn counter_value(entries: &SnapshotEntries, name: &str, axis: Option<&str>) -> u64 {
        entries
            .iter()
            .filter(|(k, _, _, _)| k.key().name() == name)
            .filter(|(k, _, _, _)| match axis {
                Some(want) => k
                    .key()
                    .labels()
                    .any(|l| l.key() == "axis" && l.value() == want),
                None => true,
            })
            .map(|(_, _, _, v)| match v {
                DebugValue::Counter(c) => *c,
                _ => 0,
            })
            .sum()
    }

    /// merged_bug_016 red 1: ONE continuous wedge (node-1's two
    /// attempts never close) must count exactly one not-wedged→wedged
    /// transition even when a breadth-suppressed phase interleaves.
    /// The retired epilogue fed empty `survivors` into the shared
    /// `marked.retain` on every suppressed tick, so node-1 re-counted
    /// (and re-warned) when the phase ended. Recorded red on that
    /// shape: counter read 2 (`left: 2, right: 1`). Deliberately
    /// arm-agnostic about the post-suppression tick's verdict so the
    /// episode-close chokepoint (merged_bug_023) does not invalidate
    /// this pin — only the COUNTER is asserted there.
    #[test]
    fn continuous_wedge_does_not_recount_across_suppressed_phase() {
        let recorder = DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            let mut t = WedgeTracker::default();
            let reg = fleet(&["node-1", "node-2", "node-3", "node-4", "node-5", "node-6"]);
            // t0: node-1's pair marks (assigned 9_865, deadline+grace
            // 130 → expired since 9_995). Counter: 1.
            let pair = |now: f64| {
                vec![
                    pg_expired("n1a", "node-1", 9_865, now),
                    pg_expired("n1b", "node-1", 9_865, now),
                ]
            };
            let v = t.update(Some(&pair(10_000.0)), &no_reaps(), &reg, 10_000.0);
            assert!(
                matches!(v, WedgeVerdict::NodeWedged(ref n, _) if n == &vec!["node-1".to_string()])
            );
            // t1: singleton expiries on nodes 2-6 engage breadth
            // (6 of 6 evidence-bearing); node-1 still over threshold.
            let mut view1 = pair(10_010.0);
            for n in 2..=6 {
                view1.push(pg_expired(
                    &format!("s{n}"),
                    &format!("node-{n}"),
                    9_875,
                    10_010.0,
                ));
            }
            assert!(matches!(
                t.update(Some(&view1), &no_reaps(), &reg, 10_010.0),
                WedgeVerdict::Systemic { .. }
            ));
            // t2 = 11_810: node-1's t0 anchors age out (1810 > 1800)
            // while the singletons' t1 anchors hold the window edge
            // (1800 ≤ 1800) — still breadth-suppressed; the singleton
            // ATTEMPTS closed (not in view), node-1's pair is still
            // open.
            assert!(matches!(
                t.update(Some(&pair(11_810.0)), &no_reaps(), &reg, 11_810.0),
                WedgeVerdict::Systemic { .. }
            ));
            // t3 = 11_815: the singletons' anchors age out; node-1's
            // still-open pair re-anchors fresh. Whatever the verdict
            // arm does here (per-node re-detect today; an episode
            // close under the breadth-release law), the transition
            // counter must NOT move — node-1 never stopped being the
            // same continuous wedge.
            let _ = t.update(Some(&pair(11_815.0)), &no_reaps(), &reg, 11_815.0);
        });
        let entries = snap.snapshot().into_vec();
        assert_eq!(
            counter_value(&entries, "rio_controller_node_wedge_marked_total", None),
            1,
            "one continuous wedge across a suppressed phase must count exactly one transition"
        );
    }

    /// merged_bug_023 red: the breadth release edge. A breadth-axis
    /// episode that decays below the fraction (staggered age-out of
    /// the early participants) must CLOSE through the
    /// drain+merge-latch+dwell chokepoint — never mint a per-node
    /// verdict from evidence the suppression itself attributed to a
    /// shared cause. Recorded red on the pre-chokepoint code: t2
    /// returned `NodeWedged(["node-6"])` — the late-onset node
    /// Dead-reaped (destructive NodeClaim delete) from retained
    /// episode evidence, while possibly mid-recovery. The same test
    /// pins the over-suppress guard (the m060 direction): node-6
    /// re-detects from 2 fresh post-watermark expiries after the
    /// dwell. (The fleet-growth release variant — population
    /// inflation dropping the fraction with anchors intact — is the
    /// alternate trigger of the same edge and closes through the
    /// same chokepoint.)
    #[test]
    fn breadth_release_does_not_dead_reap_late_onset_node() {
        let mut t = WedgeTracker::default();
        let reg = fleet(&["node-1", "node-2", "node-3", "node-4", "node-5", "node-6"]);
        // t0: singleton expiries on nodes 1-5 engage breadth (5 of 6
        // evidence-bearing; nobody past the cluster threshold).
        let t0 = 10_000.0;
        let singles: Vec<OpenAttempt> = (1..=5)
            .map(|n| pg_expired(&format!("s{n}"), &format!("node-{n}"), 9_865, t0))
            .collect();
        assert!(matches!(
            t.update(Some(&singles), &no_reaps(), &reg, t0),
            WedgeVerdict::Systemic {
                axis: SuppressionAxis::Breadth,
                ..
            }
        ));
        // t1 = t0+600: node-6 (late onset) earns 2 anchors — still
        // suppressed (6 of 6 bearing evidence).
        let t1 = t0 + 600.0;
        let pair = |now: f64| {
            vec![
                pg_expired("l6x", "node-6", 10_465, now),
                pg_expired("l6y", "node-6", 10_465, now),
            ]
        };
        assert!(matches!(
            t.update(Some(&pair(t1)), &no_reaps(), &reg, t1),
            WedgeVerdict::Systemic {
                axis: SuppressionAxis::Breadth,
                ..
            }
        ));
        // t2 = t0+1811: nodes 1-5's anchors aged out and their
        // attempts closed; node-6's pair (anchored t1) is still
        // in-window. The breadth fraction dropped — the release edge.
        let t2 = t0 + WEDGE_CLUSTER_WINDOW_SECS + 11.0;
        match t.update(Some(&pair(t2)), &no_reaps(), &reg, t2) {
            WedgeVerdict::NodeWedged(nodes, _) if !nodes.is_empty() => {
                panic!("breadth release Dead-reaped {nodes:?} from retained shared-cause evidence")
            }
            WedgeVerdict::Systemic {
                axis: SuppressionAxis::Breadth,
                ..
            } => {}
            other => panic!("the release edge must close as a breadth-suppressed tick: {other:?}"),
        }
        // The close latched the watermark (dwell running) and drained
        // the window.
        assert!(
            t.last_suppression.is_some_and(|w| w.set_at == t2),
            "the close must merge-latch the watermark at the release instant"
        );
        assert!(t.evidence.is_empty(), "the close must drain the window");
        // Over-suppress guard: after the dwell, node-6 re-detects
        // from 2 FRESH post-watermark expiries (its drained episode
        // attempts re-present and stay inadmissible).
        let t3 = t2 + WEDGE_VERDICT_DWELL_SECS + 1.0;
        let mut view3 = pair(t3); // the old pair: expiry <= watermark — blocked
        view3.push(pg_expired("f6a", "node-6", 11_975, t3));
        view3.push(pg_expired("f6b", "node-6", 11_975, t3));
        match t.update(Some(&view3), &no_reaps(), &reg, t3) {
            WedgeVerdict::NodeWedged(nodes, _) => assert_eq!(
                nodes,
                vec!["node-6".to_string()],
                "post-dwell re-detect from fresh expiries (the m060 direction)"
            ),
            other => panic!("post-dwell re-detect failed: {other:?}"),
        }
    }

    /// bug_061 / merged_bug_016 secondary red: EVERY suppressed tick
    /// increments `rio_controller_wedge_systemic_suppressed_total`
    /// labeled by the engaging axis. Recorded red on the retired
    /// code: breadth and dwell ticks moved nothing (the counter
    /// lived only in the ratio arm, unlabeled) — the runbook's
    /// "non-zero = run the systemic triage" tripwire was blind
    /// exactly while the automation was suppressing.
    #[test]
    fn suppressed_ticks_tick_the_suppression_counter_by_axis() {
        let recorder = DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            let reg = fleet(&["node-1", "node-2", "node-3", "node-4", "node-5", "node-6"]);
            // axis=breadth: singletons on 4 of 6 nodes (no node past
            // the cluster threshold).
            let mut t = WedgeTracker::default();
            let view: Vec<OpenAttempt> = (1..=4)
                .map(|n| pg_expired(&format!("b{n}"), &format!("node-{n}"), 9_865, 10_000.0))
                .collect();
            assert!(matches!(
                t.update(Some(&view), &no_reaps(), &reg, 10_000.0),
                WedgeVerdict::Systemic {
                    axis: SuppressionAxis::Breadth,
                    ..
                }
            ));
            // axis=ratio: a fresh tracker; 4 of 6 nodes past the
            // threshold.
            let mut t = WedgeTracker::default();
            let storm: Vec<OpenAttempt> = (1..=4)
                .flat_map(|n| {
                    vec![
                        pg_expired(&format!("r{n}x"), &format!("node-{n}"), 9_865, 10_000.0),
                        pg_expired(&format!("r{n}y"), &format!("node-{n}"), 9_865, 10_000.0),
                    ]
                })
                .collect();
            assert!(matches!(
                t.update(Some(&storm), &no_reaps(), &reg, 10_000.0),
                WedgeVerdict::Systemic {
                    axis: SuppressionAxis::Ratio,
                    ..
                }
            ));
            // axis=dwell: 60s after the ratio latch, node-5 presents
            // a FRESH post-watermark pair (admitted) — per-node
            // verdicts stay dwell-gated.
            let fresh: Vec<OpenAttempt> = vec![
                pg_expired("d5x", "node-5", 9_931, 10_065.0),
                pg_expired("d5y", "node-5", 9_931, 10_065.0),
            ];
            assert!(matches!(
                t.update(Some(&fresh), &no_reaps(), &reg, 10_065.0),
                WedgeVerdict::Systemic {
                    axis: SuppressionAxis::Dwell,
                    ..
                }
            ));
        });
        let suppressed = "rio_controller_wedge_systemic_suppressed_total";
        let entries = snap.snapshot().into_vec();
        for axis in SuppressionAxis::ALL {
            assert_eq!(
                counter_value(&entries, suppressed, Some(axis.label())),
                1,
                "exactly one {} suppressed tick must be counted",
                axis.label()
            );
        }
    }

    // r[verify ctrl.nodeclaim.wedge-two-axis+6]
    /// merged_bug_023 red (recorded verbatim in the close commit): a
    /// Breadth->Dwell downgrade MUST run Breadth's close. Trajectory
    /// (all through the public `update` API): episode A (ratio storm,
    /// 4-node fleet) closes-by-engage and latches the watermark ->
    /// within the dwell, fresh post-watermark expiries engage Breadth
    /// with wedged={node-1} (n1 two distinct drvs; n2/n3 one each:
    /// breadth 3/4, affected 1) -> the fleet GROWS to 8 (breadth 3/8
    /// decays below the fraction) while the dwell is active and
    /// `wedged` non-empty: the stored axis silently became Dwell ->
    /// past WEDGE_VERDICT_DWELL_SECS the dwell expires and the
    /// episode releases through Dwell's no-op. Pre-fix the retained
    /// breadth-phase evidence fell through to
    /// `WedgeVerdict::NodeWedged(["node-1"])` --- exactly what the
    /// close law forbids (`assertion failed ... left:
    /// ["node-1"], right: []`). Post-fix the downgrade drains +
    /// merge-latches + measures the dwell from the transition, and
    /// node-1 re-detects ONLY from fresh post-downgrade expiries
    /// after the dwell (the green half, asserted below --- the
    /// re-detect lane must survive).
    ///
    /// Witness-strength: certifies that NO per-node verdict is minted
    /// from episode-explained evidence via the DOWNGRADE path
    /// specifically (the release-edge path has its own pre-existing
    /// battery); the green half certifies the re-detect lane.
    #[test]
    fn breadth_downgrade_to_dwell_runs_breadth_close() {
        let mut t = WedgeTracker::default();
        let fleet4 = fleet(&["node-1", "node-2", "node-3", "node-4"]);
        let fleet8 = fleet(&[
            "node-1", "node-2", "node-3", "node-4", "node-5", "node-6", "node-7", "node-8",
        ]);

        // t=10_000: episode A --- ratio storm (4/4 nodes, 2 drvs each,
        // assigned 9_000, expiry 9_130). Drains + latches (wm 9_130,
        // set_at 10_000).
        let storm: Vec<OpenAttempt> = (1..=4)
            .flat_map(|n| {
                vec![
                    pg_expired(&format!("a{n}x"), &format!("node-{n}"), 9_000, 10_000.0),
                    pg_expired(&format!("a{n}y"), &format!("node-{n}"), 9_000, 10_000.0),
                ]
            })
            .collect();
        assert!(matches!(
            t.update(Some(&storm), &no_reaps(), &fleet4, 10_000.0),
            WedgeVerdict::Systemic {
                axis: SuppressionAxis::Ratio,
                ..
            }
        ));

        // t=10_010: episode B engages BREADTH --- post-watermark
        // expiries (assigned 9_870/9_875, expiry 10_000/10_005 >
        // 9_130): node-1 two distinct drvs (wedged), node-2/3 one
        // each. breadth 3/4 > 1/2; affected 1 (ratio off).
        let breadth_view = vec![
            pg_expired("b1x", "node-1", 9_870, 10_010.0),
            pg_expired("b1y", "node-1", 9_875, 10_010.0),
            pg_expired("b2x", "node-2", 9_870, 10_010.0),
            pg_expired("b3x", "node-3", 9_870, 10_010.0),
        ];
        assert!(matches!(
            t.update(Some(&breadth_view), &no_reaps(), &fleet4, 10_010.0),
            WedgeVerdict::Systemic {
                axis: SuppressionAxis::Breadth,
                ..
            }
        ));

        // t=10_020: the fleet grows to 8 --- breadth 3/8 decays below
        // the fraction while the dwell (set_at 10_000) is active and
        // wedged={node-1}: the DOWNGRADE tick (stored Breadth ->
        // recomputed Dwell).
        assert!(matches!(
            t.update(Some(&breadth_view), &no_reaps(), &fleet8, 10_020.0),
            WedgeVerdict::Systemic {
                axis: SuppressionAxis::Dwell,
                ..
            }
        ));

        // Past the dwell (post-fix: measured from the 10_020
        // transition; pre-fix: from the 10_000 latch --- 10_340 is
        // past either). The same still-open episode rows re-present.
        // Pre-fix red: the un-closed breadth-phase evidence mints
        // NodeWedged(["node-1"]) at dwell expiry.
        let v = t.update(Some(&breadth_view), &no_reaps(), &fleet8, 10_340.0);
        let wedged_nodes: Vec<String> = match v {
            WedgeVerdict::NodeWedged(ref nodes, _) => nodes.clone(),
            _ => Vec::new(),
        };
        assert_eq!(
            wedged_nodes,
            Vec::<String>::new(),
            "no per-node verdict may be minted from episode-explained \
             evidence after a breadth downgrade (the downgrade must have \
             closed the episode)"
        );

        // Green half: node-1 re-detects ONLY from
        // WEDGE_CLUSTER_MIN_DISTINCT_DRVS fresh POST-DOWNGRADE
        // expiries after the dwell (assigned 10_200, expiry 10_330 >
        // the downgrade watermark; breadth 1/8, dwell over).
        let fresh_pair = vec![
            pg_expired("f1", "node-1", 10_200, 10_350.0),
            pg_expired("f2", "node-1", 10_205, 10_350.0),
        ];
        let v = t.update(Some(&fresh_pair), &no_reaps(), &fleet8, 10_350.0);
        match v {
            WedgeVerdict::NodeWedged(nodes, _) => {
                assert_eq!(nodes, vec!["node-1".to_string()], "re-detect lane")
            }
            other => panic!("post-downgrade re-detect failed: {other:?}"),
        }
    }

    // r[verify ctrl.nodeclaim.wedge-two-axis+6]
    /// merged_bug_023 census (R15): the transition law is total over
    /// the 3x3 axis product --- `SuppressionAxis::ALL` x `ALL` walked
    /// through the REAL `finalize` (via the public `update`), never a
    /// parallel table. The generator is the `ALL` const pinned
    /// exhaustive by the same-file `label`/effects matches: a new
    /// axis variant fails compilation at those pins AND panics here
    /// until its cells are stated. Per-cell composed dispositions:
    ///   - every edge INTO Ratio drains the window + latches (the
    ///     engage-tick law; a Breadth->Ratio transition composes
    ///     idempotently with it);
    ///   - Breadth->Dwell drains + latches + measures the dwell from
    ///     the transition (THE behavioral cell --- pre-fix red on
    ///     exactly this 1 of 9: evidence retained);
    ///   - every other cell's composed effect equals engage-alone
    ///     (evidence retained);
    ///   - the suppression counter increments EXACTLY ONCE per
    ///     engaged tick, labeled by the ENGAGING (incoming) axis ---
    ///     the transition's release effects are counter-stripped
    ///     (SIGNED S1-OQ2).
    ///
    /// Witness-strength: certifies the production transition machine
    /// cell-by-cell (axis sequencing driven through real views and
    /// fleet changes); one DebuggingRecorder per cell, snapshot
    /// exactly once each (hazard ppppp).
    #[test]
    fn axis_transition_square_discharges_outgoing_close() {
        let fleet4: Vec<&str> = vec!["node-1", "node-2", "node-3", "node-4"];
        let fleet8: Vec<&str> = vec![
            "node-1", "node-2", "node-3", "node-4", "node-5", "node-6", "node-7", "node-8",
        ];
        // Ratio storm: every fleet4 node 2 distinct drvs (tag keeps
        // drv ids distinct across ticks so re-anchoring never aliases).
        fn ratio_storm(tag: &str, assigned: u64, now: f64) -> Vec<OpenAttempt> {
            (1..=4)
                .flat_map(|n| {
                    vec![
                        pg_expired(&format!("{tag}{n}x"), &format!("node-{n}"), assigned, now),
                        pg_expired(&format!("{tag}{n}y"), &format!("node-{n}"), assigned, now),
                    ]
                })
                .collect()
        }
        // Breadth view: node-1 wedged (2 drvs), node-2/3 one each.
        fn breadth_view(tag: &str, assigned: u64, now: f64) -> Vec<OpenAttempt> {
            vec![
                pg_expired(&format!("{tag}1x"), "node-1", assigned, now),
                pg_expired(&format!("{tag}1y"), "node-1", assigned + 5, now),
                pg_expired(&format!("{tag}2x"), "node-2", assigned, now),
                pg_expired(&format!("{tag}3x"), "node-3", assigned, now),
            ]
        }
        // Dwell view: node-1 wedged only (breadth 1/fleet).
        fn n1_pair(tag: &str, assigned: u64, now: f64) -> Vec<OpenAttempt> {
            vec![
                pg_expired(&format!("{tag}1x"), "node-1", assigned, now),
                pg_expired(&format!("{tag}1y"), "node-1", assigned + 5, now),
            ]
        }

        for from in SuppressionAxis::ALL {
            for to in SuppressionAxis::ALL {
                let recorder = DebuggingRecorder::new();
                let snap = recorder.snapshotter();
                let (evidence_empty, wm_set_at) = metrics::with_local_recorder(&recorder, || {
                    let mut t = WedgeTracker::default();
                    // ---- PREFIX: establish stored = `from` (and a
                    // live watermark where the Dwell arm needs one).
                    match from {
                        SuppressionAxis::Ratio => {
                            // t=10_000: ratio storm latches (wm
                            // 9_130, set_at 10_000), stored=Ratio.
                            assert!(matches!(
                                t.update(
                                    Some(&ratio_storm("p", 9_000, 10_000.0)),
                                    &no_reaps(),
                                    &fleet(&fleet4),
                                    10_000.0
                                ),
                                WedgeVerdict::Systemic {
                                    axis: SuppressionAxis::Ratio,
                                    ..
                                }
                            ));
                        }
                        SuppressionAxis::Breadth => {
                            // t=9_990 ratio latch, then t=10_000
                            // post-watermark breadth (stored
                            // Ratio->Breadth --- an identity
                            // transition by the composition law).
                            assert!(matches!(
                                t.update(
                                    Some(&ratio_storm("p", 9_000, 9_990.0)),
                                    &no_reaps(),
                                    &fleet(&fleet4),
                                    9_990.0
                                ),
                                WedgeVerdict::Systemic {
                                    axis: SuppressionAxis::Ratio,
                                    ..
                                }
                            ));
                            assert!(matches!(
                                t.update(
                                    Some(&breadth_view("q", 9_860, 10_000.0)),
                                    &no_reaps(),
                                    &fleet(&fleet4),
                                    10_000.0
                                ),
                                WedgeVerdict::Systemic {
                                    axis: SuppressionAxis::Breadth,
                                    ..
                                }
                            ));
                        }
                        SuppressionAxis::Dwell => {
                            // t=9_990 ratio latch, then t=10_000
                            // node-1 pair under the dwell (breadth
                            // 1/4; stored Ratio->Dwell --- identity
                            // transition).
                            assert!(matches!(
                                t.update(
                                    Some(&ratio_storm("p", 9_000, 9_990.0)),
                                    &no_reaps(),
                                    &fleet(&fleet4),
                                    9_990.0
                                ),
                                WedgeVerdict::Systemic {
                                    axis: SuppressionAxis::Ratio,
                                    ..
                                }
                            ));
                            assert!(matches!(
                                t.update(
                                    Some(&n1_pair("q", 9_860, 10_000.0)),
                                    &no_reaps(),
                                    &fleet(&fleet4),
                                    10_000.0
                                ),
                                WedgeVerdict::Systemic {
                                    axis: SuppressionAxis::Dwell,
                                    ..
                                }
                            ));
                        }
                    }

                    // ---- TICK B (t=10_010): drive the `to` axis.
                    let (view, reg) = match to {
                        // Fresh-er storm beats any prefix watermark
                        // (assigned 9_900, expiry 10_030).
                        SuppressionAxis::Ratio => {
                            (ratio_storm("b", 9_870, 10_010.0), fleet(&fleet4))
                        }
                        // Post-watermark breadth rows. The
                        // Breadth->Breadth continue cell
                        // re-presents the SAME still-open prefix
                        // rows (re-observation of an anchored
                        // (node, drv) pair is a no-op --- fresh
                        // sibling drvs on node-2/3 would make
                        // them wedged and escalate to Ratio);
                        // other entries use fresh post-watermark
                        // rows.
                        SuppressionAxis::Breadth => {
                            if from == SuppressionAxis::Breadth {
                                (breadth_view("q", 9_860, 10_010.0), fleet(&fleet4))
                            } else {
                                (breadth_view("c", 9_870, 10_010.0), fleet(&fleet4))
                            }
                        }
                        // node-1 pair on a GROWN fleet: breadth
                        // <= 3/8, dwell active (prefix latch
                        // set_at 9_990/10_000 is < 300 s old),
                        // wedged non-empty.
                        SuppressionAxis::Dwell => (n1_pair("c", 9_870, 10_010.0), fleet(&fleet8)),
                    };
                    let v = t.update(Some(&view), &no_reaps(), &reg, 10_010.0);
                    match v {
                        WedgeVerdict::Systemic { axis, .. } => assert_eq!(
                            axis, to,
                            "cell ({from:?}->{to:?}): tick B must engage the `to` axis"
                        ),
                        other => {
                            panic!("cell ({from:?}->{to:?}): tick B yielded {other:?}")
                        }
                    }
                    (t.evidence.is_empty(), t.last_suppression.map(|w| w.set_at))
                });

                // Counter law: exactly one suppressed tick at tick B
                // beyond the prefix, labeled by the ENGAGING axis.
                // Prefix contributions are deterministic per `from`:
                // Ratio prefix = 1 ratio tick; Breadth = 1 ratio + 1
                // breadth; Dwell = 1 ratio + 1 dwell.
                let entries = snap.snapshot().into_vec();
                let name = "rio_controller_wedge_systemic_suppressed_total";
                let prefix_counts: [(SuppressionAxis, u64); 3] = match from {
                    SuppressionAxis::Ratio => [
                        (SuppressionAxis::Ratio, 1),
                        (SuppressionAxis::Breadth, 0),
                        (SuppressionAxis::Dwell, 0),
                    ],
                    SuppressionAxis::Breadth => [
                        (SuppressionAxis::Ratio, 1),
                        (SuppressionAxis::Breadth, 1),
                        (SuppressionAxis::Dwell, 0),
                    ],
                    SuppressionAxis::Dwell => [
                        (SuppressionAxis::Ratio, 1),
                        (SuppressionAxis::Breadth, 0),
                        (SuppressionAxis::Dwell, 1),
                    ],
                };
                for (axis, prefix) in prefix_counts {
                    let expect = prefix + u64::from(axis == to);
                    assert_eq!(
                        counter_value(&entries, name, Some(axis.label())),
                        expect,
                        "cell ({from:?}->{to:?}): suppressed-tick count for {} \
                         (the tick is counted ONCE, by the engaging axis; the \
                         transition is counter-stripped)",
                        axis.label()
                    );
                }

                // Evidence disposition law (the composed effect).
                let expect_drained = to == SuppressionAxis::Ratio
                    || (from == SuppressionAxis::Breadth && to == SuppressionAxis::Dwell);
                assert_eq!(
                    evidence_empty, expect_drained,
                    "cell ({from:?}->{to:?}): evidence disposition (drained={expect_drained})"
                );
                // Watermark law: edges that drain at tick B refresh
                // `set_at` to tick B (the dwell runs from the
                // transition/engage); all other cells keep the prefix
                // latch.
                if expect_drained {
                    assert_eq!(
                        wm_set_at,
                        Some(10_010.0),
                        "cell ({from:?}->{to:?}): the dwell must run from tick B"
                    );
                } else {
                    assert!(
                        wm_set_at.is_some() && wm_set_at != Some(10_010.0),
                        "cell ({from:?}->{to:?}): a non-draining edge must keep \
                         the prefix watermark (got {wm_set_at:?})"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod verdict_confinement_tests {
    use super::*;

    /// The registered NodeClaim fleet for a tick (backing node names).
    fn fleet(nodes: &[&str]) -> HashSet<String> {
        nodes.iter().map(|s| s.to_string()).collect()
    }

    /// bug_151 (recorded red: pre-fix `update(None)` returned
    /// `NodeWedged([])` minted directly in the None arm — a verdict
    /// produced OUTSIDE the sealed exit, conflating "unobserved" with
    /// "zero wedged" while the Sealed doc called exactly that
    /// unrepresentable). The token now lives in the `seal` module
    /// whose only producer is `finalize`; the unobserved tick is its
    /// own variant.
    #[test]
    fn unobserved_tick_yields_a_distinct_verdict() {
        let mut t = WedgeTracker::default();
        let v = t.update(
            None,
            &std::collections::BTreeSet::new(),
            &fleet(&[]),
            1000.0,
        );
        assert!(
            matches!(v, WedgeVerdict::Unobserved(..)),
            "an RPC-failed tick must produce the Unobserved verdict, got {v:?}"
        );
    }

    /// The unobserved epilogue branch: an RPC blip between two
    /// per-node verdicts must NOT drain `marked` (a drained set would
    /// double-count the next transition as a fresh edge).
    #[test]
    fn unobserved_tick_does_not_drain_marked() {
        let mut t = WedgeTracker::default();
        let view = vec![
            OpenAttempt {
                intent_id: "a1".into(),
                source_node: "n1".into(),
                assigned_at_age_secs: 100 + WEDGE_DEADLINE_GRACE_SECS + 5,
                deadline_secs: 100,
                attempt_kind: rio_proto::types::AttemptKind::Build as i32,
                ..Default::default()
            },
            OpenAttempt {
                intent_id: "a2".into(),
                source_node: "n1".into(),
                assigned_at_age_secs: 100 + WEDGE_DEADLINE_GRACE_SECS + 5,
                deadline_secs: 100,
                attempt_kind: rio_proto::types::AttemptKind::Build as i32,
                ..Default::default()
            },
        ];
        let v = t.update(
            Some(&view),
            &std::collections::BTreeSet::new(),
            &fleet(&["n1"]),
            1000.0,
        );
        assert!(matches!(v, WedgeVerdict::NodeWedged(ref n, _) if n == &vec!["n1".to_string()]));
        assert!(t.marked.contains("n1"));
        let v = t.update(
            None,
            &std::collections::BTreeSet::new(),
            &fleet(&["n1"]),
            1010.0,
        );
        assert!(matches!(v, WedgeVerdict::Unobserved(..)));
        assert!(
            t.marked.contains("n1"),
            "an RPC blip must not drain the marked set"
        );
    }
}

#[cfg(test)]
mod episode_latch_tests {
    use super::*;
    fn no_reaps() -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::new()
    }

    /// The registered NodeClaim fleet for a tick (backing node names).
    fn fleet(nodes: &[&str]) -> HashSet<String> {
        nodes.iter().map(|s| s.to_string()).collect()
    }
    /// This module's full node universe — registered every tick.
    fn all_nodes() -> HashSet<String> {
        fleet(&[
            "node-a", "node-b", "node-c", "node-0", "node-1", "node-2", "node-3",
        ])
    }

    fn expired_at(intent: &str, node: &str, over_secs: u64) -> OpenAttempt {
        OpenAttempt {
            intent_id: intent.into(),
            source_node: node.into(),
            assigned_at_age_secs: 100 + WEDGE_DEADLINE_GRACE_SECS + over_secs,
            deadline_secs: 100,
            attempt_kind: rio_proto::types::AttemptKind::Build as i32,
            ..Default::default()
        }
    }

    /// merged_bug_163 hole 1: a sub-threshold participant of a
    /// suppressed episode (one in-window anchor — the single-pod-node
    /// case) must not keep that anchor as half of a future pair. One
    /// genuinely new post-episode expiry is "a build problem, not a
    /// node problem" (module doc) — yet pre-fix it completed the pair
    /// and Dead-reaped the node.
    #[test]
    fn suppressed_episode_subthreshold_anchor_cannot_complete_a_pair() {
        let mut t = WedgeTracker::default();
        // A and B wedge (2 distinct drvs); C participates with ONE
        // anchor. wedged={A,B}, population={A,B,C}: 2/3 > 0.5 and
        // affected >= 2 -> Systemic.
        let view = vec![
            expired_at("a1", "node-a", 5),
            expired_at("a2", "node-a", 5),
            expired_at("b1", "node-b", 5),
            expired_at("b2", "node-b", 5),
            expired_at("c1", "node-c", 5),
        ];
        assert!(matches!(
            t.update(
                Some(&view),
                &no_reaps(),
                &fleet(&["node-a", "node-b", "node-c"]),
                10_000.0
            ),
            WedgeVerdict::Systemic {
                affected: 2,
                of: 3,
                ..
            }
        ));
        // Post-episode: C's old attempt closed; ONE genuinely new
        // expiry on C (fresh attempt, expired just now).
        let v = t.update(
            Some(&[expired_at("c-new", "node-c", 2)]),
            &no_reaps(),
            &all_nodes(),
            10_010.0,
        );
        match v {
            WedgeVerdict::NodeWedged(nodes, _) => assert!(
                nodes.is_empty(),
                "a single fresh expiry completed a pair with suppressed-episode \
                 evidence: {nodes:?}"
            ),
            other => panic!("unexpected verdict {other:?}"),
        }
    }

    /// merged_bug_060: the suppression watermark is a PROTECTIVE
    /// latch — it cannot outlive the episode it suppresses. An
    /// episode attempt that never closes (a genuinely wedged node is
    /// exactly the node whose attempts never close) was blocked
    /// forever by the un-TTL'd watermark: the node became permanently
    /// undetectable — a NodeClaim leak in the over-suppress direction,
    /// inverting the module's documented safe-failure direction.
    // r[verify ctrl.nodeclaim.wedge-two-axis+6]
    #[test]
    fn post_window_participant_re_detects() {
        let mut t = WedgeTracker::default();
        let storm: Vec<OpenAttempt> = (0..4)
            .flat_map(|n| {
                vec![
                    expired_at(&format!("d{n}x"), &format!("node-{n}"), 5),
                    expired_at(&format!("d{n}y"), &format!("node-{n}"), 5),
                ]
            })
            .collect();
        assert!(matches!(
            t.update(Some(&storm), &no_reaps(), &all_nodes(), 10_000.0),
            WedgeVerdict::Systemic { .. }
        ));
        // One full window later the episode is over, every other node
        // healed — but node-1's SAME two attempts are still open and
        // expired (the wedge signature: nothing on that node reports).
        let late = 10_000.0 + WEDGE_CLUSTER_WINDOW_SECS + 10.0;
        let still_open = vec![
            expired_at("d1x", "node-1", 5 + (WEDGE_CLUSTER_WINDOW_SECS as u64) + 10),
            expired_at("d1y", "node-1", 5 + (WEDGE_CLUSTER_WINDOW_SECS as u64) + 10),
        ];
        let v = t.update(Some(&still_open), &no_reaps(), &all_nodes(), late);
        match v {
            WedgeVerdict::NodeWedged(nodes, _) => assert_eq!(
                nodes,
                vec!["node-1".to_string()],
                "a post-window still-wedged participant must re-detect"
            ),
            other => panic!("unexpected verdict {other:?}"),
        }
    }

    /// merged_bug_163 hole 2: at the trailing edge of a healing
    /// episode, the last node whose reports land (same attempts,
    /// still open+expired) re-anchors from the SAME suppressed
    /// expiries and gets Dead-reaped — a healthy node, killed during
    /// recovery. Evidence admission must beat the suppression
    /// watermark: the same attempts' expiries predate it.
    #[test]
    fn trailing_edge_laggard_is_not_reaped() {
        let mut t = WedgeTracker::default();
        let storm: Vec<OpenAttempt> = (0..4)
            .flat_map(|n| {
                vec![
                    expired_at(&format!("d{n}x"), &format!("node-{n}"), 5),
                    expired_at(&format!("d{n}y"), &format!("node-{n}"), 5),
                ]
            })
            .collect();
        assert!(matches!(
            t.update(Some(&storm), &no_reaps(), &all_nodes(), 10_000.0),
            WedgeVerdict::Systemic { .. }
        ));
        // Heal in arbitrary order: only node-1's SAME two attempts are
        // still open+expired next tick (its reports are last in the
        // redelivery queue). Their expiries predate the suppression.
        let laggard = vec![
            expired_at("d1x", "node-1", 15),
            expired_at("d1y", "node-1", 15),
        ];
        let v = t.update(Some(&laggard), &no_reaps(), &all_nodes(), 10_010.0);
        match v {
            WedgeVerdict::NodeWedged(nodes, _) => assert!(
                nodes.is_empty(),
                "the trailing-edge laggard was re-marked from suppressed-episode \
                 expiries: {nodes:?}"
            ),
            other => panic!("unexpected verdict {other:?}"),
        }
    }
}
