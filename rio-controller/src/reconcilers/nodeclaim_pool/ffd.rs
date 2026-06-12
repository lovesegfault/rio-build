//! First-Fit-Decreasing bin-packing simulation.
//!
//! Per `r[ctrl.nodeclaim.ffd-sim]`: sort intents `(ready, c*)` descending
//! (Ready before forecast, large before small), bin-select MostAllocated
//! on the `allocatable` divisor — the same scoring `kube-scheduler-packed`
//! (B2) uses, so the simulation's `placeable` set predicts what the real
//! scheduler will do once B12 routes pods to it. The `unplaced` residual
//! is `cover_deficit`'s (B8) per-cell input.

use std::collections::{BTreeMap, HashMap, HashSet};

use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use rio_crds::karpenter::{NodeClaim, NodeClaimStatus};
use rio_proto::types::SpawnIntent;

use super::sketch::{CapacityType, Cell, CellSketches};

/// Karpenter's well-known capacity-type label key. Values: `"spot"` /
/// `"on-demand"` (NOT the PG/helm `"od"` form — `cap_from_label`
/// maps).
pub const CAPACITY_TYPE_LABEL: &str = "karpenter.sh/capacity-type";

/// hw-class label key. The scheduler emits this on each
/// `node_affinity` term (`r[sched.sla.hw-class]`); B8's
/// `create_nodeclaim` stamps it on `metadata.labels` so
/// [`LiveNode::from`] can recover the cell without re-reading the
/// scheduler's `[sla.hw_classes]` map.
pub const HW_CLASS_LABEL: &str = "rio.build/hw-class";

/// `node.kubernetes.io/instance-type` — Karpenter writes this to
/// `NodeClaim.metadata.labels` post-Launch (not just to the Node). See
/// [`LiveNode::instance_type`].
pub const INSTANCE_TYPE_LABEL: &str = "node.kubernetes.io/instance-type";

/// `kubernetes.io/arch` — `amd64`/`arm64`. Each `[sla.hw_classes.$h]`
/// conjunction carries this (12-class prod config); [`system_to_arch`]
/// maps `intent.system` to the same vocabulary so hw-agnostic intents
/// (cold-start `fit=None` → `hw_class_names=[]`) can FFD-place on any
/// matching-arch node and `cover_deficit` can target the reference
/// cell.
pub const ARCH_LABEL: &str = "kubernetes.io/arch";

/// Re-export of [`rio_common::k8s::system_to_k8s_arch`] under the
/// pre-existing local name. Shared with the scheduler's bypass-path
/// `--capacity` arch-match so both sides use the same table.
pub use rio_common::k8s::system_to_k8s_arch as system_to_arch;

/// View of one owned NodeClaim for FFD + consolidation. Built from the
/// typed `NodeClaim` (B4) so condition/allocatable/label parsing lives
/// in one `From` impl.
#[derive(Debug, Clone)]
pub struct LiveNode {
    /// `metadata.name` — the delete key.
    pub name: String,
    /// Backing `Node` name once `Registered=True`; `None` in-flight.
    pub node_name: Option<String>,
    /// `status.conditions[type=Registered].status == "True"`. FFD treats
    /// in-flight (`!registered`) claims as projected capacity (their
    /// `status.capacity`, populated at Launch); Registered claims use
    /// `status.allocatable` minus [`Self::requested`].
    pub registered: bool,
    /// `metadata.deletionTimestamp` epoch seconds. `Some(_)` ⇔ Karpenter's
    /// termination finalizer is running (drain → cordon → delete Node →
    /// remove finalizer; ~60-90s). The NodeClaim object is still in the
    /// API and still consuming EC2 cores so the `max_fleet_cores` budget
    /// keeps counting it (otherwise `cover_deficit` double-provisions
    /// while the old node bills), but it is NOT a placement candidate:
    /// the kube-scheduler refuses to bind onto a cordoned/terminating
    /// node and any pod already there is being evicted. Without this gate
    /// the FFD's simulated placement set ⊋ the K8s-acceptable set — it
    /// "places" intents on the dying node, reports `deficit=0`, and the
    /// replacement isn't minted until the finalizer clears (~80s late).
    /// Carries the timestamp (not just the bool) so `emit_live_gauges`
    /// can compute `terminating_age_max_seconds` (r38 merged_001 —
    /// count-based StuckTerminating false-fires under sustained
    /// scale-down churn, the same anti-pattern
    /// `inflight_age_max_seconds` was added to fix for scale-up).
    pub terminating_since: Option<f64>,
    /// `(hw_class, capacity_type)` recovered from `metadata.labels`.
    /// `None` ⇔ labels absent/malformed (a freshly-`create()`d claim
    /// before Karpenter resolves capacity-type, or a non-B8 claim that
    /// leaked the [`OWNER_LABEL`](super::OWNER_LABEL)). FFD skips
    /// cell-less nodes — no intent's `A_open` can match `None`.
    pub cell: Option<Cell>,
    /// `metadata.labels[node.kubernetes.io/instance-type]`. Karpenter
    /// writes this post-Launch (when it resolves the bid to a concrete
    /// type), so `None` pre-Launch — same timing as `cell`.
    /// `observe_registered` ships `(cell, instance_type, allocatable)`
    /// to the scheduler so `CostTable` learns which types each cell
    /// actually resolves to (R24B7 option-i autodiscovery).
    pub instance_type: Option<String>,
    /// `(cores, mem_bytes, disk_bytes)` from `status.allocatable`
    /// (preferred) or `status.capacity` (in-flight fallback). Whole
    /// cores: `7910m` → 7, matching `SpawnIntent.cores`' unit.
    pub allocatable: (u32, u64, u64),
    /// `(cores, mem_bytes, disk_bytes)` already requested by pods on
    /// the backing Node. `From<NodeClaim>` sets this to `(0,0,0)`;
    /// `list_live_nodeclaims` post-fills it from the per-tick
    /// `PodSnapshot` (see `super::pods`) Pod LIST.
    pub requested: (u32, u64, u64),
    /// `metadata.creationTimestamp` as unix-epoch seconds. `None` only
    /// on a just-`create()`d object before the apiserver round-trip.
    pub created_secs: Option<f64>,
    /// `metadata.annotations`. B10's hold-open ε reads
    /// `rio.build/hold-open`.
    pub annotations: BTreeMap<String, String>,
    /// Full `status` for B10/B11's condition reads (idle-since,
    /// `Launched=False` ICE detection).
    pub status: NodeClaimStatus,
}

/// `metav1.Time` → unix-epoch seconds. kube 3.0 wraps `jiff::Timestamp`;
/// integer seconds suffice for boot-time arithmetic (typical boot ≈
/// 30–120s).
fn time_secs(t: &Time) -> f64 {
    t.0.as_second() as f64
}

impl LiveNode {
    /// `true` ⇔ `metadata.deletionTimestamp.is_some()`. Backward-compat
    /// shim for the original `bool` field.
    #[inline]
    pub fn terminating(&self) -> bool {
        self.terminating_since.is_some()
    }

    /// Remaining placeable `(cores, mem, disk)`. Registered →
    /// `allocatable − requested` (saturating: a mis-accounted node
    /// reads as 0-free, not underflow). In-flight → `allocatable`
    /// (nothing scheduled yet by construction).
    pub fn free(&self) -> (u32, u64, u64) {
        if self.registered {
            (
                self.allocatable.0.saturating_sub(self.requested.0),
                self.allocatable.1.saturating_sub(self.requested.1),
                self.allocatable.2.saturating_sub(self.requested.2),
            )
        } else {
            self.allocatable
        }
    }

    /// `(status, last_transition_secs)` for condition `type_`. `None` ⇔
    /// the condition isn't present (Karpenter writes `Launched`/
    /// `Registered` lazily — absence ≠ `False`).
    pub fn cond(&self, type_: &str) -> Option<(&str, f64)> {
        self.status
            .conditions
            .iter()
            .find(|c| c.type_ == type_)
            .map(|c| (c.status.as_str(), time_secs(&c.last_transition_time)))
    }

    /// `reason` for condition `type_`. `None` ⇔ condition absent.
    /// `health::classify` reads `Launched`'s reason to fire ICE
    /// immediately on a terminal launch failure (Karpenter GCs the
    /// claim ~1s after posting `LaunchFailed`, so the timeout-based
    /// path never observes it).
    pub fn cond_reason(&self, type_: &str) -> Option<&str> {
        self.status
            .conditions
            .iter()
            .find(|c| c.type_ == type_)
            .map(|c| c.reason.as_str())
    }

    /// The `Launched` condition projected to the boot/capacity axis:
    /// `Some(true)` ⇔ `Launched=True` (capacity provably materialized
    /// — an instance came up), `Some(false)` ⇔ `Launched=False`,
    /// `None` ⇔ condition absent or `Unknown` (Karpenter has not
    /// adjudicated the launch — the same capacity-unproven posture as
    /// `False`; `health::classify`'s timeout arms make the identical
    /// call). `health::classify_vanish` keys its boot-vs-capacity
    /// teardown split on this bit (bug_094's Launched axis).
    /// (Plain-code references: `health` is module-private — a pub-doc
    /// link would break the doc build without
    /// `--document-private-items`.)
    pub fn launched(&self) -> Option<bool> {
        match self.cond("Launched")? {
            ("True", _) => Some(true),
            ("False", _) => Some(false),
            _ => None,
        }
    }

    /// Seconds since `metadata.creationTimestamp`. `None` if creation
    /// time is absent (apiserver hasn't round-tripped yet).
    pub fn age_secs(&self, now_secs: f64) -> Option<f64> {
        self.created_secs.map(|c| now_secs - c)
    }

    /// `Registered.lastTransitionTime − creationTimestamp`: the
    /// Karpenter+kubelet boot overhead. `Some` only when
    /// `Registered=True`. The value B9's [`super::sketch::CellState::
    /// record`] feeds into the `boot_active` quantile sketch.
    pub fn boot_secs(&self) -> Option<f64> {
        let created = self.created_secs?;
        match self.cond("Registered")? {
            ("True", t) => Some(t - created),
            _ => None,
        }
    }

    /// `Registered.lastTransitionTime` epoch-secs. `Some` only when
    /// `Registered=True`. NOT [`Self::boot_secs`] (which is the
    /// `Registered − created` DURATION) — `observe_registered`'s
    /// recency-gate needs `now − registered_at`, so a 5-day-old node
    /// with 18s boot must NOT pass the "recent edge" check.
    pub fn registered_at_secs(&self) -> Option<f64> {
        match self.cond("Registered")? {
            ("True", t) => Some(t),
            _ => None,
        }
    }

    // r42 bug_020: `idle_secs()` deleted. It read a Karpenter `Empty`
    // condition Karpenter v1 does not write — emptiness was folded into
    // `Consolidatable.reason` in v1.0; the shim NodePool's
    // `consolidateAfter: Never` suppresses `Consolidatable` too. The
    // fallback path always fired → "idle for hours" the moment a busy
    // node's last pod left → the `r[ctrl.nodeclaim.consolidate-na]`
    // warm-keep model was dead code. The controller's own `requested.0`
    // transition tracked in `prev_idle` is the source of truth. **No
    // replacement method**: the never-bound-node case is handled by
    // `prev_idle.or_insert(now_secs)` on first observation in
    // `observe_idle_to_busy`.

    /// Read `metadata.annotations[key]`.
    pub fn annotation(&self, key: &str) -> Option<&str> {
        self.annotations.get(key).map(String::as_str)
    }
}

impl From<NodeClaim> for LiveNode {
    fn from(nc: NodeClaim) -> Self {
        let status = nc.status.unwrap_or_default();
        let registered = status
            .conditions
            .iter()
            .any(|c| c.type_ == "Registered" && c.status == "True");
        let terminating_since = nc.metadata.deletion_timestamp.as_ref().map(time_secs);
        let cell = nc.metadata.labels.as_ref().and_then(|l| {
            let h = l.get(HW_CLASS_LABEL)?;
            let cap = cap_from_label(l.get(CAPACITY_TYPE_LABEL)?)?;
            Some(Cell(h.clone(), cap))
        });
        let instance_type = nc
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get(INSTANCE_TYPE_LABEL).cloned());
        // Prefer allocatable (kubelet-reported, post-reserved); fall
        // back to capacity (Karpenter's launch-time projection); fall
        // back to spec.resources.requests (what cover_deficit asked
        // for) so a pre-Launch claim contributes to the
        // `max_fleet_cores` budget instead of reading as 0 cores —
        // otherwise the same deficit is re-minted each tick under
        // Karpenter queue lag.
        let allocatable = status
            .allocatable
            .as_ref()
            .or(status.capacity.as_ref())
            .or(nc.spec.resources.as_ref().and_then(|r| r.requests.as_ref()))
            .map_or((0, 0, 0), parse_resources);
        Self {
            name: nc.metadata.name.unwrap_or_default(),
            node_name: status.node_name.clone(),
            registered,
            terminating_since,
            cell,
            instance_type,
            allocatable,
            requested: (0, 0, 0),
            created_secs: nc.metadata.creation_timestamp.as_ref().map(time_secs),
            annotations: nc.metadata.annotations.unwrap_or_default(),
            status,
        }
    }
}

/// `(intent, target_nodeclaim_name, in_flight)` for placeable intents.
/// `in_flight = !registered` so the consolidator can distinguish
/// "reserved on a live node" from "reserved on a node that hasn't
/// landed yet".
pub type Placement = (SpawnIntent, String, bool);

/// Cores-window slack multiplier for [`sim_window_cores`]. The window
/// is denominated on the cores axis only; the ×2 absorbs (a) multi-axis
/// packing inefficiency (an intent can be unplaceable by mem/disk while
/// small on cores) and (b) placed-on-live variance, so the windowed
/// unplaced residual still saturates the budget brake whenever the full
/// set would (W9-AV). VIOLABLE (R17, cost axis): the value is a
/// derivation-note constant, not a measured bound — revisit if the
/// deficit-equivalence witness ever needs more headroom.
pub const SIM_WINDOW_SLACK: u64 = 2;

/// merged_bug_053: the window's MINTABILITY view — window admission is
/// denominated in the quantity downstream minting evaluates (mask ×
/// per-class budget × live free capacity), not in cores alone, so
/// [`admit_window`] can classify PROVABLY-unmintable bucket heads out
/// of window accounting exactly as the cores>window skip does (they
/// defer typed, re-seen when capacity grows, and the bucket
/// CONTINUES). Pre-fix a head with `class budget < cores ≤ window`
/// admitted, blocked every sibling bucket on its rotation-first
/// ticks, and minted zero — net-zero ticks at 1/K frequency,
/// persistent under the cores-desc sort.
///
/// Constructed by `cover::window_mintability` (the
/// `cover::class_budget` seam — the SAME budget law the mint
/// consumes, evaluated at tick start with zero created cores; the
/// mask snapshot is the pre-sim subset, conservative by
/// construction). The skip is sound-conservative: it never classifies
/// out a head that could place OR mint this tick.
pub struct WindowMintability {
    /// Classes whose EVERY configured cell is ICE-masked
    /// (mint-impossible this tick).
    pub fully_masked: HashSet<String>,
    /// Per-known-class mint budget (`cover::class_budget` at tick
    /// start, `class_created = 0`).
    pub class_budget: HashMap<String, u32>,
    /// The global remaining fleet budget — the bound for hw-agnostic
    /// heads and for declared classes without a per-class cap row.
    pub global_budget: u32,
    /// Σ free cores over live placeable (cell-bearing,
    /// non-terminating) nodes — the placement bound: a head above it
    /// cannot place anywhere regardless of class.
    pub live_free_cores: u64,
    /// Every known cell masked — the hw-agnostic all-masked arm.
    pub all_known_masked: bool,
}

impl WindowMintability {
    /// The PERMISSIVE view: skips nothing. Test-only — unit harnesses
    /// exercising window mechanics other than mintability, and the
    /// W11-AE negative control (the permissive view IS the pre-fix
    /// window). Production always constructs through
    /// `cover::window_mintability`.
    #[cfg(test)]
    #[must_use]
    pub fn permissive() -> Self {
        Self {
            fully_masked: HashSet::new(),
            class_budget: HashMap::new(),
            global_budget: u32::MAX,
            live_free_cores: u64::MAX,
            all_known_masked: false,
        }
    }

    /// PROVABLY unmintable this tick: the head can neither PLACE
    /// (`cores > Σ live free` — placement onto live capacity ignores
    /// ICE masks, so the placement conjunct rides BOTH arms) nor MINT
    /// (every declared class fully masked, or `cores` above every
    /// declared class's budget; hw-agnostic: all known cells masked,
    /// or `cores > global budget`). Unknown declared classes default
    /// to the global budget (permissive-leaning — the unknown-class
    /// lane has its own typed outcome downstream and is not this
    /// skip's population).
    #[must_use]
    pub fn head_unmintable(&self, i: &SpawnIntent) -> bool {
        let cores = u64::from(i.cores);
        if cores <= self.live_free_cores {
            return false; // placeable on live capacity — never skip.
        }
        if i.hw_class_names.is_empty() {
            self.all_known_masked || cores > u64::from(self.global_budget)
        } else {
            let budget_of = |h: &String| {
                u64::from(
                    self.class_budget
                        .get(h.as_str())
                        .copied()
                        .unwrap_or(self.global_budget),
                )
            };
            i.hw_class_names
                .iter()
                .all(|h| self.fully_masked.contains(h.as_str()))
                || i.hw_class_names.iter().all(|h| cores > budget_of(h))
        }
    }
}

/// Yield quantum for [`simulate_windowed`]: the walk yields to the
/// runtime every `FFD_YIELD_QUANTUM` simulated intents so a large tick
/// cannot starve the reconciler's executor (Banner A-1 on the
/// controller). VIOLABLE (R17, time axis): the quantum is deliberately
/// UNSIZED-BY-MEASUREMENT this round — the B4 freeze primitive is
/// unidentified (17-18s whole-runtime freezes at 0.13-0.47 cores,
/// blocked-not-compute), so the quantum is sized at the first
/// sentinel-attributed freeze (the D5 skew sentinel landed this wave;
/// see `rio_controller_runtime_skew_seconds`). 256 bounds the
/// between-yield chunk to ~sub-millisecond at the benched per-intent
/// score cost while keeping yield overhead negligible.
pub const FFD_YIELD_QUANTUM: usize = 256;

/// Typed per-tick window remainder — demand beyond the
/// fleet-capacity-derived admission window. NOT dropped: the scheduler
/// still holds these Ready/forecast intents and the next tick re-polls
/// them; the type makes the truncation observable (remainder gauges +
/// the deficit law) instead of silent.
// r[impl ctrl.nodeclaim.sim-window]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SimRemainder {
    pub intents: usize,
    pub cores: u64,
}

/// One windowed-simulation tick's outcome: the FFD result over the
/// admitted window plus the window-deferred intents — the THIRD tick
/// disposition as a per-intent typed letter (round-10 merged_bug_012,
/// R25: the aggregate [`SimRemainder`] is a metric, not the law's
/// carrier; every consumer hears the per-intent letter through
/// [`super::PlacedTick`]).
pub struct SimOutcome {
    pub placeable: Vec<Placement>,
    pub unplaced: Vec<SpawnIntent>,
    /// Demand beyond this tick's admission window — still wanted,
    /// re-seen next tick; published per-intent so the demand-visible
    /// fold (pool jobs) and the consolidator count it instead of
    /// reading absence as demand-gone.
    pub deferred: Vec<SpawnIntent>,
    pub remainder: SimRemainder,
}

/// The per-tick simulated-intent window, denominated in cores and
/// derived from FLEET CAPACITY in the budget-brake's own terms:
/// `(live free + budget remaining) × slack`. Window ≥ the mint law's
/// per-tick consumption (`ctrl.nodeclaim.mint-deficit-proportional`'s
/// budget term is ≤ `budget_remaining_cores`), so every intent the
/// brake could mint for this tick is inside the window.
///
/// merged_bug_053 re-derivation of the old "supply is never
/// window-starved" claim: the SIZE of the window is necessary but not
/// sufficient — window SHARE is also consumed, and a head the mint
/// law will provably refuse (mask × per-class budget × live free,
/// the [`WindowMintability`] view) used to consume a whole rotation
/// while minting nothing, starving mintable siblings at 1/K
/// frequency. The window-starvation guarantee therefore holds at the
/// PAIR: this capacity-derived size PLUS [`admit_window`]'s
/// mintability skip — the windowed unplaced residual saturates the
/// budget whenever the full set would, because everything occupying
/// the window is either placeable or mintable
/// (`window_admits_mintable_siblings_every_tick` is the
/// multi-class mint-comparison witness).
// r[impl ctrl.nodeclaim.sim-window]
pub fn sim_window_cores(live_free_cores: u64, budget_remaining_cores: u64) -> u64 {
    live_free_cores
        .saturating_add(budget_remaining_cores)
        .saturating_mul(SIM_WINDOW_SLACK)
}

/// Split `sorted` (FFD priority order) into the admitted window and the
/// typed remainder.
///
/// Admission is round-robin-fair across hw-class buckets (keyed on the
/// intent's first `hw_class_names` entry; `""` = the hw-agnostic
/// bucket), with the bucket start rotated by `tick` — mirroring
/// `cells_round_robin`, so a single class's pathological demand cannot
/// permanently starve a sibling class's window share across ticks.
/// Priority order is preserved WITHIN a bucket and restored globally
/// (admitted intents are re-emitted in their original sort positions).
/// Already-bound intents bypass the window entirely: their simulation
/// cost is a map lookup and excluding them would mis-read bound work as
/// remainder.
///
/// Round-10 merged_bug_012 amendments:
/// - **Job-held exemption** (mirroring the bound exemption): an intent
///   in `job_held` (any live pod — bound OR Pending) bypasses the
///   window. Its Job already exists, so deferring it cannot pace
///   anything — it only mis-reads existing demand as remainder and
///   hands the still-wanted Pending Job to the orphan reap. Sim cost
///   is bounded by the live-Job population (≤ ceiling), so the
///   work-bound the window exists for survives.
/// - **Can-never-fit head skip**: a bucket head whose own cores exceed
///   the WHOLE window (`cores > window_cores`) can never be admitted
///   by any rotation of this window — it defers (typed, re-seen when
///   capacity grows) and the bucket CONTINUES, so one oversized head
///   no longer starves every sibling in its class across rotations
///   (the cores-desc sort made that starvation persistent). A head
///   that would fit a fresh window but not the remaining budget still
///   blocks its bucket (priority order within a bucket is never
///   skipped past).
///
/// Bughunt-11 merged_bug_053 amendment — **provably-unmintable head
/// skip** (the same defer-without-blocking arm): a head that fits the
/// window by cores but that downstream minting will PROVABLY refuse
/// this tick ([`WindowMintability::head_unmintable`] — cannot place on
/// live capacity AND cannot mint under mask × per-class budget)
/// classifies out of window accounting exactly like the cores>window
/// skip. Pre-fix such a head admitted, consumed the rotation-first
/// window share, blocked every sibling bucket, and minted zero —
/// net-zero ticks at 1/K frequency. The skip engages only under
/// window contention (the fast path admits everything when total
/// demand fits — no share to starve); skipped heads defer typed into
/// the remainder, re-seen when budget/mask/live capacity changes.
// r[impl ctrl.nodeclaim.sim-window]
pub fn admit_window(
    sorted: Vec<SpawnIntent>,
    bound: &HashMap<String, String>,
    job_held: &HashSet<String>,
    window_cores: u64,
    tick: u64,
    mint: &WindowMintability,
) -> (Vec<SpawnIntent>, Vec<SpawnIntent>) {
    let exempt =
        |i: &SpawnIntent| bound.contains_key(&i.intent_id) || job_held.contains(&i.intent_id);
    // Fast path: total non-exempt cores within the window — admit all.
    let unbound_cores: u64 = sorted
        .iter()
        .filter(|i| !exempt(i))
        .map(|i| u64::from(i.cores))
        .sum();
    if unbound_cores <= window_cores {
        return (sorted, Vec::new());
    }
    // Bucket the sort positions by hw-class (stable: bucket order ==
    // priority order within each class).
    let mut buckets: BTreeMap<&str, std::collections::VecDeque<usize>> = BTreeMap::new();
    let mut admitted_idx: Vec<usize> = Vec::with_capacity(sorted.len());
    for (idx, i) in sorted.iter().enumerate() {
        if exempt(i) {
            admitted_idx.push(idx); // window-exempt (bound or job-held)
        } else {
            buckets
                .entry(i.hw_class_names.first().map_or("", String::as_str))
                .or_default()
                .push_back(idx);
        }
    }
    let mut order: Vec<&str> = buckets.keys().copied().collect();
    if !order.is_empty() {
        let off = (tick % order.len() as u64) as usize;
        order.rotate_left(off);
    }
    let mut cum: u64 = 0;
    let mut open = order.len();
    let mut blocked: Vec<bool> = vec![false; order.len()];
    while open > 0 {
        let mut progressed = false;
        for (bi, key) in order.iter().enumerate() {
            if blocked[bi] {
                continue;
            }
            let q = buckets.get_mut(key).expect("bucket exists");
            // Can-never-fit heads defer without blocking the bucket
            // (they go to the remainder via non-admission) — both the
            // cores>window form and the merged_bug_053
            // provably-unmintable form (mask × budget × live free).
            while q.front().is_some_and(|&idx| {
                u64::from(sorted[idx].cores) > window_cores || mint.head_unmintable(&sorted[idx])
            }) {
                q.pop_front();
                progressed = true;
            }
            let Some(&idx) = q.front() else {
                blocked[bi] = true;
                open -= 1;
                continue;
            };
            let cores = u64::from(sorted[idx].cores);
            if cum.saturating_add(cores) > window_cores {
                // Head doesn't fit the REMAINING budget (but could fit
                // a fresh window): the bucket blocks — priority order
                // within a bucket is never skipped past.
                blocked[bi] = true;
                open -= 1;
                continue;
            }
            q.pop_front();
            cum += cores;
            admitted_idx.push(idx);
            progressed = true;
        }
        if !progressed {
            break;
        }
    }
    // Restore global priority order, then split.
    admitted_idx.sort_unstable();
    let admitted_set: Vec<bool> = {
        let mut v = vec![false; sorted.len()];
        for &i in &admitted_idx {
            v[i] = true;
        }
        v
    };
    let mut admitted = Vec::with_capacity(admitted_idx.len());
    let mut remainder = Vec::with_capacity(sorted.len() - admitted_idx.len());
    for (idx, i) in sorted.into_iter().enumerate() {
        if admitted_set[idx] {
            admitted.push(i);
        } else {
            remainder.push(i);
        }
    }
    (admitted, remainder)
}

/// FFD-simulate placing `intents` onto `live`. Returns
/// `(placeable, unplaced)`.
///
/// **Sort**: `(ready, cores, mem_bytes)` descending, `intent_id`
/// ascending tiebreak (stable across ticks). `ready` is the explicit
/// proto field 13 discriminator — NOT `eta_seconds == 0.0`, which a
/// forecast intent with overdue deps can hit (bug_030).
///
/// **A_open**: a Ready intent's admissible cells are its full
/// `cells_of` set. A forecast intent's are filtered to
/// `eta_seconds < lead_time[cell]` — only place on cells that will be
/// up before the intent's deps complete. Empty `A_open` from
/// `hw_class_names=[]` (cold-start `fit=None`) is the **hw-agnostic**
/// case: eligible on ANY node whose hw-class arch matches
/// `system_to_arch(intent.system)` — so the placeable-gate works for
/// unfitted drvs once `cover_deficit` has provisioned a reference-cell
/// node. Empty `A_open` from lead-time gating (all cells too slow) →
/// unplaced.
///
/// **Already-bound short-circuit**: an intent in `bound` (the tick's
/// Pod LIST saw its pod with `spec.nodeName` set) goes
/// directly into `placeable` keyed to its actual node — no fit-check.
/// Its own pod's `(c,m,d)` is already in `free()`'s `requested` term;
/// fit-checking would double-count and evict it (then orphan-reap the
/// progressing-ContainerCreating pod). When the bound node isn't in
/// `live` (NodeClaim deleted; race), the intent falls through to the
/// regular fit-check.
///
/// **Bin-select**: among `live` nodes whose `cell ∈ A_open` (or whose
/// arch matches, for hw-agnostic intents) AND whose running `free`
/// covers [`crate::reconcilers::pool::jobs::intent_pod_footprint`]'s
/// `(cores, mem, ephemeral)` triple, pick MostAllocated:
/// max `(allocatable − free + cores) / allocatable` on the cpu axis.
/// `free` is the running tally (decremented per placement by the same
/// footprint triple) so the score sees prior placements within this
/// tick — matching kube-scheduler-packed's per-pod scoring on the
/// live node state. The shared footprint fn is the
/// §Simulator-shares-accounting guarantee — FFD compares against the
/// SAME `(c,m,d)` the pod will request, not raw `disk_bytes`.
// r[impl ctrl.nodeclaim.ffd-sim]
/// `hw_admits(h, arch, required_features)` — agnostic-fallback gate.
/// `arch` is the resolved `kubernetes.io/arch` value derived from
/// `intent.system` (NOT re-derived inside the closure — `simulate`
/// already resolves it once per intent), `None` for arch-unmappable
/// systems (`builtin` FODs — r35 B1; the closure treats `None` as
/// pass-through). §13d STRIKE-7 (mb_012): `required_features` is
/// included so the closure can check `features_compatible` — a
/// `hw_class_names=[]` intent carrying `required_features=["kvm"]` must
/// NOT FFD-place onto a non-metal node; the inverse (featureless intent
/// onto kvm-tainted metal node) must NOT either.
pub fn simulate(
    intents: &[SpawnIntent],
    live: &[LiveNode],
    sketches: &CellSketches,
    bound: &HashMap<String, String>,
    fuse_cache_bytes: u64,
    hw_admits: impl Fn(&str, Option<&str>, &[String]) -> bool,
) -> (Vec<Placement>, Vec<SpawnIntent>) {
    let mut sorted: Vec<SpawnIntent> = intents.to_vec();
    sort_ffd(&mut sorted);
    let mut st = SimState::new(live, sorted.len());
    for i in sorted {
        sim_one(
            &mut st,
            live,
            i,
            sketches,
            bound,
            fuse_cache_bytes,
            &hw_admits,
        );
    }
    (st.placeable, st.unplaced)
}

/// The windowed, runtime-cooperative form of [`simulate`] — same
/// placement semantics over the admitted window ([`admit_window`]),
/// yielding to the executor every [`FFD_YIELD_QUANTUM`] intents so a
/// large tick cannot starve the reconciler's runtime; demand beyond
/// the window is the typed [`SimRemainder`].
// r[impl ctrl.nodeclaim.sim-window]
#[allow(clippy::too_many_arguments)]
pub async fn simulate_windowed(
    intents: &[SpawnIntent],
    live: &[LiveNode],
    sketches: &CellSketches,
    bound: &HashMap<String, String>,
    job_held: &HashSet<String>,
    fuse_cache_bytes: u64,
    window_cores: u64,
    tick: u64,
    mint: &WindowMintability,
    hw_admits: impl Fn(&str, Option<&str>, &[String]) -> bool,
) -> SimOutcome {
    let mut sorted: Vec<SpawnIntent> = intents.to_vec();
    sort_ffd(&mut sorted);
    let (admitted, deferred) = admit_window(sorted, bound, job_held, window_cores, tick, mint);
    let rem = SimRemainder {
        intents: deferred.len(),
        cores: deferred.iter().map(|i| u64::from(i.cores)).sum(),
    };
    let mut st = SimState::new(live, admitted.len());
    for (n, i) in admitted.into_iter().enumerate() {
        if n > 0 && n % FFD_YIELD_QUANTUM == 0 {
            tokio::task::yield_now().await;
        }
        sim_one(
            &mut st,
            live,
            i,
            sketches,
            bound,
            fuse_cache_bytes,
            &hw_admits,
        );
    }
    SimOutcome {
        placeable: st.placeable,
        unplaced: st.unplaced,
        deferred,
        remainder: rem,
    }
}

/// The FFD priority order: `(ready, cores, mem_bytes)` descending,
/// `intent_id` ascending tiebreak (stable across ticks).
fn sort_ffd(sorted: &mut [SpawnIntent]) {
    sorted.sort_by(|a, b| {
        let k = |i: &SpawnIntent| (i.ready.unwrap_or(true), i.cores, i.mem_bytes);
        k(b).cmp(&k(a)).then_with(|| a.intent_id.cmp(&b.intent_id))
    });
}

/// Carry-state for one simulation pass. [`simulate`] drives it in one
/// sync pass; [`simulate_windowed`] threads it across yield points so
/// the running `free` tally survives chunking.
struct SimState<'a> {
    free: HashMap<&'a str, (u32, u64, u64)>,
    by_node_name: HashMap<&'a str, (bool, &'a str)>,
    placeable: Vec<Placement>,
    unplaced: Vec<SpawnIntent>,
}

impl<'a> SimState<'a> {
    fn new(live: &'a [LiveNode], cap: usize) -> Self {
        // Running free per node. Cell-less nodes excluded up front: no
        // intent can match them, and excluding here keeps the score loop's
        // `cell.unwrap` infallible. Terminating nodes excluded: §13d
        // "placement ⊇ provisioning" — kube-scheduler won't bind onto a
        // draining node, so a simulated placement there overcounts capacity
        // and `cover_deficit` under-mints the replacement (~80s of Pending
        // before Karpenter's finalizer clears the object). The dying node's
        // cores STILL count toward `max_fleet_cores`/`class_budget` (it's
        // billing), just not toward FFD's free-bin set.
        // r[impl ctrl.nodeclaim.ffd-exclude-terminating]
        let free: HashMap<&str, (u32, u64, u64)> = live
            .iter()
            .filter(|n| n.cell.is_some() && !n.terminating())
            .map(|n| (n.name.as_str(), n.free()))
            .collect();

        // Map node_name → (registered, in `live`) for the bound short-
        // circuit. `live` is keyed by NodeClaim name; bound is by Node
        // name (`spec.nodeName`). Terminating nodes excluded: a pod still
        // bound there is draining — the intent should fall through to the
        // fit-check so it lands on (or mints) a replacement that's ready
        // when the eviction completes, instead of being "placed" on a node
        // it's about to leave.
        let by_node_name: HashMap<&str, (bool, &str)> = live
            .iter()
            .filter(|n| !n.terminating())
            .filter_map(|n| {
                n.node_name
                    .as_deref()
                    .map(|nn| (nn, (n.registered, n.name.as_str())))
            })
            .collect();
        Self {
            free,
            by_node_name,
            placeable: Vec::with_capacity(cap),
            unplaced: Vec::new(),
        }
    }
}

/// Simulate placing ONE intent against the running state — the
/// per-intent body shared verbatim by [`simulate`] and
/// [`simulate_windowed`].
fn sim_one<'a>(
    st: &mut SimState<'a>,
    live: &'a [LiveNode],
    i: SpawnIntent,
    sketches: &CellSketches,
    bound: &HashMap<String, String>,
    fuse_cache_bytes: u64,
    hw_admits: &impl Fn(&str, Option<&str>, &[String]) -> bool,
) {
    use crate::reconcilers::pool::jobs::intent_pod_footprint;
    let SimState {
        free,
        by_node_name,
        placeable,
        unplaced,
    } = st;
    {
        // Already bound → straight to placeable (no fit-check, no
        // free() decrement — its slot is already counted in
        // `requested`).
        // merged_bug_126: a bound pod on a node the intent has since
        // excluded is about to be drift-reaped — falling through to the
        // fit-check places (or mints) its replacement instead of
        // freezing the stale binding as "placed".
        if let Some(node) = bound.get(&i.intent_id)
            && !crate::reconcilers::pool::candidate::node_excluded(&i, node)
            && let Some(&(registered, nc_name)) = by_node_name.get(node.as_str())
        {
            placeable.push((i, nc_name.to_string(), !registered));
            return;
        }
        let (ic, im, id) = intent_pod_footprint(&i, fuse_cache_bytes).as_triple();
        let open = a_open(&i, sketches);
        // hw-agnostic (`hw_class_names=[]`): eligible on any node
        // whose hw-class admits the intent (arch + features — see
        // `hw_admits` doc). r35 B1 (§13e B5): the gate is "at least one
        // non-trivial constraint axis (arch OR features)". `arch=None ∧
        // features=[]` is genuinely unroutable (featureless darwin/
        // builtin); `arch=Some ∧ features=[]` is the original
        // cold-start non-FOD case (arch routes); `arch=None ∧
        // features=[fetcher]` is the §13e builtin-FOD case (features
        // route, `matches_arch(_, None)` passes through). Distinguished
        // from "all cells lead-time-gated" (non-empty `hw_class_names`,
        // empty `open`) which stays cell-gated and falls through to
        // unplaced.
        let agnostic = i
            .hw_class_names
            .is_empty()
            .then(|| (system_to_arch(&i.system), i.required_features.as_slice()));
        let best = live
            .iter()
            .filter(|n| {
                n.cell.as_ref().is_some_and(|c| {
                    open.contains(c)
                        || agnostic.is_some_and(|(a, f)| {
                            (a.is_some() || !f.is_empty()) && hw_admits(&c.0, a, f)
                        })
                })
            })
            // merged_bug_126: the intent's exclusion set is a consulted
            // axis — the rendered pod can never bind on an excluded
            // node, so simulating a placement there overcounts capacity
            // and `cover_deficit` under-mints. Unbound claims (no Node
            // yet) cannot be excluded (exclusions are node-name-keyed).
            .filter(|n| {
                n.node_name
                    .as_deref()
                    .is_none_or(|nn| !crate::reconcilers::pool::candidate::node_excluded(&i, nn))
            })
            .filter(|n| {
                free.get(n.name.as_str())
                    .is_some_and(|f| f.0 >= ic && f.1 >= im && f.2 >= id)
            })
            .max_by(|a, b| {
                // MostAllocated on cpu: highest post-placement
                // utilization wins. `allocatable.0.max(1)`: a 0-core
                // node (status not yet populated) scores 0 instead of
                // NaN — and was already filtered by the `free >= cores`
                // check unless `i.cores == 0`.
                let score = |n: &LiveNode| -> f64 {
                    let f = free[n.name.as_str()];
                    let alloc = n.allocatable.0.max(1);
                    f64::from(alloc - f.0 + ic) / f64::from(alloc)
                };
                score(a).total_cmp(&score(b))
            });
        match best {
            Some(n) => {
                let f = free.get_mut(n.name.as_str()).expect("filtered above");
                f.0 -= ic;
                f.1 -= im;
                f.2 -= id;
                placeable.push((i, n.name.clone(), !n.registered));
            }
            None => unplaced.push(i),
        }
    }
}

/// Does the production [`simulate`] pack every intent in `u` into `n`
/// uniform `bin`-sized synthetic NodeClaims for `cell`? The predicate
/// [`super::cover::sizing`] iterates upward on. STRIKE-4 close (r26
/// mb_002): the predicate IS [`simulate`] — no second sort/score code
/// path to diverge on.
///
/// `ready` affects `simulate`'s sort ORDER (load-bearing — the axis a
/// reimplemented predicate would diverge on); `eta_seconds` is
/// neutralized to `f64::MIN` so [`a_open`]'s `eta < lead_time` filter
/// passes for every forecast intent regardless of
/// `sketches.lead_time(cell)` — `assign_to_cells` already gated each
/// intent's openness on `cell`, so the lead-time check is redundant in
/// this synthetic env.
pub(super) fn sim_packs(
    cell: &Cell,
    u: &[&SpawnIntent],
    bin: (u32, u64, u64),
    n: u32,
    fuse_cache_bytes: u64,
) -> bool {
    let intents: Vec<SpawnIntent> = u
        .iter()
        .map(|i| SpawnIntent {
            eta_seconds: f64::MIN,
            ..(*i).clone()
        })
        .collect();
    let nodes: Vec<LiveNode> = (0..n)
        .map(|k| LiveNode {
            name: format!("sim{k}"),
            node_name: None,
            registered: true,
            terminating_since: None,
            cell: Some(cell.clone()),
            instance_type: None,
            allocatable: bin,
            requested: (0, 0, 0),
            created_secs: None,
            annotations: BTreeMap::new(),
            status: NodeClaimStatus::default(),
        })
        .collect();
    simulate(
        &intents,
        &nodes,
        &CellSketches::default(),
        &HashMap::new(),
        fuse_cache_bytes,
        |_, _, _| true,
    )
    .1
    .is_empty()
}

/// Per-cell `(on_registered, on_inflight)` placement count. The cell
/// is the placed-on node's cell (not the intent's `A_open` — an intent
/// may target multiple cells; the placement is on exactly one).
/// Placements on nodes absent from `live` (race) or cell-less nodes are
/// dropped. Feeds `CellState::observe_hit_ratio`(super::sketch::
/// CellState::observe_hit_ratio).
pub fn per_cell_hit_ratio(placeable: &[Placement], live: &[LiveNode]) -> HashMap<Cell, (u64, u64)> {
    let by_name: HashMap<&str, &LiveNode> = live.iter().map(|n| (n.name.as_str(), n)).collect();
    let mut out: HashMap<Cell, (u64, u64)> = HashMap::new();
    for (_, node, in_flight) in placeable {
        let Some(cell) = by_name.get(node.as_str()).and_then(|n| n.cell.clone()) else {
            continue;
        };
        let e = out.entry(cell).or_default();
        if *in_flight {
            e.1 += 1;
        } else {
            e.0 += 1;
        }
    }
    out
}

/// Map `karpenter.sh/capacity-type` label values to [`CapacityType`].
/// Distinct from [`CapacityType::parse`] which takes the PG/helm
/// `"spot"`/`"od"` form (migration 059 CHECK constraint). Karpenter's
/// label canon is `"spot"`/`"on-demand"`.
fn cap_from_label(s: &str) -> Option<CapacityType> {
    match s {
        "spot" => Some(CapacityType::Spot),
        "on-demand" => Some(CapacityType::OnDemand),
        _ => None,
    }
}

/// One intent's decoded cell set plus its decode-loss count
/// (merged_bug_006): `refused` counts wire entries the decode could
/// not honor — a length mismatch between the parallel arrays, a term
/// missing the `karpenter.sh/capacity-type` requirement, or an
/// unparseable capacity value. `refused > 0` means the pair is SKEWED
/// (producer regression or scheduler/controller version skew) and the
/// set is untrustworthy evidence; the cover chokepoint refuses the
/// whole intent loudly (`PlacementOutcome::DecodeRefused`) instead of
/// placing against a silently truncated set.
pub struct CellsDecode {
    pub cells: Vec<Cell>,
    pub refused: usize,
}

/// Recover `(hw_class, cap)` cells from a SpawnIntent's parallel
/// `(hw_class_names, node_affinity)` arrays, REPORTING decode losses.
/// One cell per term; hw-agnostic mode emits empty arrays (zero terms,
/// zero losses).
pub fn cells_of_checked(i: &SpawnIntent) -> CellsDecode {
    let mut refused = i.hw_class_names.len().abs_diff(i.node_affinity.len());
    let mut cells = Vec::with_capacity(i.hw_class_names.len());
    for (h, t) in i.hw_class_names.iter().zip(&i.node_affinity) {
        let cap = t
            .match_expressions
            .iter()
            .find(|r| r.key == CAPACITY_TYPE_LABEL)
            .and_then(|r| r.values.first())
            .and_then(|v| cap_from_label(v));
        match cap {
            Some(cap) => cells.push(Cell(h.clone(), cap)),
            None => refused += 1,
        }
    }
    CellsDecode { cells, refused }
}

/// The lossy view of `cells_of_checked` (private — plain-code
/// reference keeps the pub doc link-clean) — kept for consumers whose
/// disposition is NOT minted at the cover chokepoint (the FFD sim
/// places conservatively against whatever decoded; the typed refusal
/// letter is minted once, at `assign_to_cells`).
pub fn cells_of(i: &SpawnIntent) -> Vec<Cell> {
    cells_of_checked(i).cells
}

/// [`a_open`] plus the decode evidence the cover chokepoint folds:
/// `open` is the admissible-open set, `refused` the decode-loss count,
/// `decoded` how many wire entries DID decode (pre lead-time filter —
/// context for the refusal WARN).
pub struct AOpenDecode {
    pub open: Vec<Cell>,
    pub refused: usize,
    pub decoded: usize,
}

/// Admissible-open cell set with decode evidence: Ready → all decoded
/// cells; forecast → those with `eta_seconds < lead_time[cell]`.
pub fn a_open_checked(i: &SpawnIntent, sketches: &CellSketches) -> AOpenDecode {
    let CellsDecode { cells, refused } = cells_of_checked(i);
    let decoded = cells.len();
    let open = if i.ready.unwrap_or(true) {
        cells
    } else {
        cells
            .into_iter()
            .filter(|c| i.eta_seconds < sketches.lead_time(c))
            .collect()
    };
    AOpenDecode {
        open,
        refused,
        decoded,
    }
}

/// Admissible-open cell set for `i`: Ready → all of `cells_of(i)`;
/// forecast → those with `eta_seconds < lead_time[cell]`. B8's
/// `cover_deficit` reuses this (the cheapest-open-cell choice operates
/// on the same set FFD placed against).
pub fn a_open(i: &SpawnIntent, sketches: &CellSketches) -> Vec<Cell> {
    a_open_checked(i, sketches).open
}

/// `(cores, mem_bytes, disk_bytes)` from a `cpu`/`memory`/
/// `ephemeral-storage` Quantity map. Missing keys → 0.
fn parse_resources(m: &BTreeMap<String, Quantity>) -> (u32, u64, u64) {
    let q = |k: &str| m.get(k).map(|q| q.0.as_str());
    (
        // SpawnIntent.cores is whole cores (jobs.rs writes
        // `Quantity(cores.to_string())`); truncate millicores so the
        // `free.0 >= i.cores` comparison is unit-consistent.
        q("cpu").map_or(0, |s| (parse_cpu_millis(s) / 1000) as u32),
        q("memory").map_or(0, parse_bytes),
        q("ephemeral-storage").map_or(0, parse_bytes),
    )
}

/// Parse a Kubernetes CPU Quantity string to millicores.
/// `"64"` → 64000, `"64000m"` → 64000, `"1.5"` → 1500, `"1k"` →
/// 1_000_000. Malformed → `warn!` + 0.
///
/// Handles all decimal-SI suffixes (`n`/`u`/`m`/`k`/`M`/`G`/`T`/`P`/
/// `E`). apimachinery's `Quantity.String()` canonicalizes a DecimalSI
/// value of exactly N×1000 cores as `"Nk"` (rule: largest suffix with
/// no fractional digits) — Karpenter's `status.allocatable.cpu` is a
/// `v1.ResourceList` Quantity, so a 1000-core node serializes as `"1k"`.
/// Binary-SI (`Ki`/`Mi`) is unhandled — never emitted for CPU.
pub(crate) fn parse_cpu_millis(q: &str) -> u64 {
    // Suffix → multiplier (cores). Longest first so `"m"` doesn't
    // shadow nothing-relevant here, but consistent with the idiom.
    let (num, mult): (&str, f64) = [
        ("n", 1e-9),
        ("u", 1e-6),
        ("m", 1e-3),
        ("k", 1e3),
        ("M", 1e6),
        ("G", 1e9),
        ("T", 1e12),
        ("P", 1e15),
        ("E", 1e18),
    ]
    .iter()
    .find_map(|(s, m)| q.strip_suffix(*s).map(|n| (n, *m)))
    .unwrap_or((q, 1.0));
    num.parse::<f64>()
        .map(|c| (c * mult * 1000.0).round() as u64)
        .unwrap_or_else(|_| {
            tracing::warn!(quantity = %q, "unparseable CPU Quantity; treating as 0");
            0
        })
}

/// Parse a Kubernetes memory/storage Quantity string to bytes.
/// Handles binary-SI (`Ki`/`Mi`/`Gi`/`Ti`/`Pi`/`Ei`), decimal-SI
/// (`k`/`K`/`M`/`G`/`T`/`P`/`E`), and bare numbers (incl.
/// DecimalExponent like `"1e6"`). Malformed → 0. Two-char binary
/// suffixes are checked first so `"31Gi"` doesn't strip as `"31G"+"i"`.
///
/// `pub(crate)`: B8's `cover_deficit` parses the instance-menu
/// `mem_bytes`/`disk_bytes` from the same Quantity form.
pub(crate) fn parse_bytes(q: &str) -> u64 {
    const BIN: [(&str, u64); 6] = [
        ("Ei", 1 << 60),
        ("Pi", 1 << 50),
        ("Ti", 1 << 40),
        ("Gi", 1 << 30),
        ("Mi", 1 << 20),
        ("Ki", 1 << 10),
    ];
    const DEC: [(&str, f64); 7] = [
        ("E", 1e18),
        ("P", 1e15),
        ("T", 1e12),
        ("G", 1e9),
        ("M", 1e6),
        ("k", 1e3),
        ("K", 1e3),
    ];
    for (s, m) in BIN {
        if let Some(n) = q.strip_suffix(s) {
            return n.parse::<f64>().map_or(0, |v| (v * m as f64) as u64);
        }
    }
    for (s, m) in DEC {
        if let Some(n) = q.strip_suffix(s) {
            return n.parse::<f64>().map_or(0, |v| (v * m) as u64);
        }
    }
    q.parse::<f64>().map_or(0, |v| v as u64)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rio_proto::types::{NodeSelectorRequirement, NodeSelectorTerm};

    const GI: u64 = 1 << 30;

    // --- builders ------------------------------------------------------

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
            "allocatable": { "cpu": "8", "memory": "32Gi", "ephemeral-storage": "100Gi" },
        }))
        .unwrap();
        NodeClaim {
            metadata: kube::api::ObjectMeta {
                name: Some(name.into()),
                labels: Some(
                    [
                        (HW_CLASS_LABEL.into(), "mid-ebs-x86".into()),
                        (CAPACITY_TYPE_LABEL.into(), "spot".into()),
                    ]
                    .into(),
                ),
                ..Default::default()
            },
            spec: Default::default(),
            status: Some(status),
        }
    }

    /// LiveNode with `cell`, `allocatable` cores/mem/disk, registered.
    /// `requested` defaults 0.
    pub(crate) fn node(
        name: &str,
        hw: &str,
        cap: CapacityType,
        cores: u32,
        mem: u64,
        disk: u64,
    ) -> LiveNode {
        LiveNode {
            name: name.into(),
            node_name: Some(format!("node-{name}")),
            registered: true,
            terminating_since: None,
            cell: Some(Cell(hw.into(), cap)),
            instance_type: None,
            allocatable: (cores, mem, disk),
            requested: (0, 0, 0),
            created_secs: Some(1000.0),
            annotations: BTreeMap::new(),
            status: NodeClaimStatus::default(),
        }
    }

    /// Mark `n` as terminating (`metadata.deletionTimestamp` set —
    /// Karpenter's finalizer is draining it).
    pub(crate) fn set_terminating(mut n: LiveNode) -> LiveNode {
        n.terminating_since = Some(0.0);
        n
    }

    /// Set `(type, status, lastTransitionTime)` conditions on `n.status`.
    /// Built via JSON: `Condition` has non-Default `last_transition_time`.
    pub(crate) fn with_conds(n: LiveNode, conds: &[(&str, &str, f64)]) -> LiveNode {
        let with_reason: Vec<_> = conds.iter().map(|&(ty, st, t)| (ty, st, t, "")).collect();
        with_conds_reason(n, &with_reason)
    }

    /// As [`with_conds`] plus `reason` per condition (4th tuple field).
    pub(crate) fn with_conds_reason(
        mut n: LiveNode,
        conds: &[(&str, &str, f64, &str)],
    ) -> LiveNode {
        let cs: Vec<_> = conds
            .iter()
            .map(|(ty, st, t, reason)| {
                serde_json::json!({
                    "type": ty, "status": st,
                    "lastTransitionTime": format!("1970-01-01T{:02}:{:02}:{:02}Z",
                        (*t as u64) / 3600, ((*t as u64) % 3600) / 60, (*t as u64) % 60),
                    "reason": reason, "message": "",
                })
            })
            .collect();
        n.status.conditions = serde_json::from_value(serde_json::json!(cs)).unwrap();
        n
    }

    /// SpawnIntent targeting `cells` (one affinity term per).
    fn intent(id: &str, cores: u32, mem: u64, cells: &[(&str, CapacityType)]) -> SpawnIntent {
        let (hw_class_names, node_affinity) = cells
            .iter()
            .map(|(h, cap)| {
                let cap_label = match cap {
                    CapacityType::Spot => "spot",
                    CapacityType::OnDemand => "on-demand",
                };
                let term = NodeSelectorTerm {
                    match_expressions: vec![
                        NodeSelectorRequirement {
                            key: HW_CLASS_LABEL.into(),
                            operator: "In".into(),
                            values: vec![(*h).into()],
                        },
                        NodeSelectorRequirement {
                            key: CAPACITY_TYPE_LABEL.into(),
                            operator: "In".into(),
                            values: vec![cap_label.into()],
                        },
                    ],
                };
                ((*h).to_string(), term)
            })
            .unzip();
        SpawnIntent {
            intent_id: id.into(),
            cores,
            mem_bytes: mem,
            disk_bytes: GI,
            ready: Some(true),
            hw_class_names,
            node_affinity,
            ..Default::default()
        }
    }

    fn forecast(mut i: SpawnIntent, eta: f64) -> SpawnIntent {
        i.ready = Some(false);
        i.eta_seconds = eta;
        i
    }

    fn placed_on<'a>(p: &'a [Placement], id: &str) -> &'a str {
        &p.iter().find(|(i, _, _)| i.intent_id == id).unwrap().1
    }

    /// `hw_admits` stub: every hw-class admits every arch+features
    /// (tests that don't exercise the hw-agnostic path don't care).
    fn any_admit(_h: &str, _a: Option<&str>, _f: &[String]) -> bool {
        true
    }

    /// `simulate` with `bound`/`fuse_cache_bytes` defaulted (no
    /// already-bound short-circuit; `fuse=0` so footprint disk ==
    /// `disk_bytes×headroom + LOG_BUDGET` ≈ raw `disk_bytes` for tests
    /// that don't exercise the disk axis). Tests that DO care call
    /// `simulate` directly.
    fn sim(intents: &[SpawnIntent], live: &[LiveNode]) -> (Vec<Placement>, Vec<SpawnIntent>) {
        simulate(
            intents,
            live,
            &CellSketches::default(),
            &HashMap::new(),
            0,
            any_admit,
        )
    }

    fn sim_sk(
        intents: &[SpawnIntent],
        live: &[LiveNode],
        sk: &CellSketches,
    ) -> (Vec<Placement>, Vec<SpawnIntent>) {
        simulate(intents, live, sk, &HashMap::new(), 0, any_admit)
    }

    // --- LiveNode parsing ----------------------------------------------

    #[test]
    fn live_node_from_nodeclaim_reads_registered() {
        let live: LiveNode = nc("a", true).into();
        assert_eq!(live.name, "a");
        assert!(live.registered);
        assert_eq!(live.node_name.as_deref(), Some("node-a"));
        assert_eq!(
            live.cell,
            Some(Cell("mid-ebs-x86".into(), CapacityType::Spot))
        );
        assert_eq!(live.allocatable, (8, 32 * GI, 100 * GI));
        assert_eq!(live.requested, (0, 0, 0));
        assert_eq!(live.free(), (8, 32 * GI, 100 * GI));

        let inflight: LiveNode = nc("b", false).into();
        assert!(!inflight.registered);
        assert!(inflight.node_name.is_none());
    }

    /// `cond()` reads `(status, lastTransitionTime)`; `boot_secs()` =
    /// Registered.transition − created; `age_secs()` = now − created.
    #[test]
    fn live_node_cond_boot_age() {
        let n = with_conds(
            node("a", "h", CapacityType::Spot, 8, GI, GI),
            &[("Launched", "True", 1010.0), ("Registered", "True", 1042.0)],
        );
        assert_eq!(n.cond("Launched"), Some(("True", 1010.0)));
        assert_eq!(n.cond("Registered"), Some(("True", 1042.0)));
        assert_eq!(n.cond("Drifted"), None);
        assert_eq!(n.boot_secs(), Some(42.0));
        assert_eq!(n.age_secs(1100.0), Some(100.0));
        // Registered=False → no boot_secs.
        let nf = with_conds(
            node("b", "h", CapacityType::Spot, 8, GI, GI),
            &[("Registered", "False", 1042.0)],
        );
        assert_eq!(nf.boot_secs(), None);
        // No created_secs → no boot/age.
        let mut nc = n.clone();
        nc.created_secs = None;
        assert_eq!(nc.boot_secs(), None);
        assert_eq!(nc.age_secs(1100.0), None);
    }

    // r42 bug_020: `live_node_idle_secs` test deleted with `idle_secs()`.
    // The test synthesized an `Empty` condition Karpenter v1 never writes
    // — it tested dead code. The never-bound-node case is now exercised by
    // `observe_idle_to_busy` tests asserting
    // `prev_idle[name] == first_observed_tick_secs`.

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
        assert_eq!(live.cell, None, "no labels → no cell");
        assert_eq!(live.allocatable, (0, 0, 0));
        assert!(
            !live.terminating(),
            "no deletionTimestamp → not terminating"
        );
        assert_eq!(
            live.terminating_since, None,
            "no deletionTimestamp → no terminating_since"
        );
    }

    /// `metadata.deletionTimestamp` set ⇒ Karpenter's termination
    /// finalizer is draining the node (~60-90s). `LiveNode.terminating_since`
    /// is the structural carrier so every consumer (FFD placement,
    /// budget, gauges, consolidation) reads ONE field instead of each
    /// re-deriving from `metadata`.
    #[test]
    fn live_node_from_nodeclaim_reads_deletion_timestamp() {
        let mut claim = nc("dying", true);
        let ts = Time(k8s_openapi::jiff::Timestamp::UNIX_EPOCH);
        claim.metadata.deletion_timestamp = Some(ts.clone());
        let live: LiveNode = claim.into();
        assert!(live.terminating(), "deletionTimestamp set → terminating");
        assert_eq!(
            live.terminating_since,
            Some(time_secs(&ts)),
            "terminating_since carries the deletionTimestamp epoch (so \
             emit_live_gauges can compute terminating_age_max_seconds)"
        );
        assert!(
            live.registered,
            "Registered=True still readable while terminating"
        );
    }

    /// §13d "placement ⊇ provisioning": a terminating NodeClaim sits in
    /// the API ~60-90s while Karpenter's finalizer drains it. FFD MUST
    /// NOT place intents there — kube-scheduler refuses to bind onto a
    /// cordoned node. Without this gate the simulation reports
    /// `deficit=0`, `cover_deficit` mints nothing, and the actual pod
    /// stays Pending until the finalizer clears (~80s of latency in the
    /// production smoking gun). With it the intent surfaces in
    /// `unplaced` so `cover_deficit` provisions the replacement on the
    /// SAME tick.
    // r[verify ctrl.nodeclaim.ffd-exclude-terminating]
    #[test]
    fn simulate_consults_intent_exclusions() {
        // merged_bug_126: the only fitting node is in the intent's
        // exclusion set → the intent must surface as UNPLACED (so
        // `cover_deficit` mints a replacement). Pre-fix the FFD ignored
        // `excluded_nodes` entirely and "placed" it there — the
        // recorded red: deficit=0, no mint, the spawn gate then
        // poisoned the derivation as fleet-exhausted.
        let n = node("n1", "h", CapacityType::Spot, 8, 32 * GI, 100 * GI);
        let mut i = intent("a", 4, GI, &[("h", CapacityType::Spot)]);
        i.excluded_nodes = vec!["node-n1".into()];

        let (placeable, unplaced) = sim(std::slice::from_ref(&i), std::slice::from_ref(&n));
        assert!(
            placeable.is_empty(),
            "FFD must not simulate capacity on an excluded node"
        );
        assert_eq!(
            unplaced.len(),
            1,
            "fully-excluded intent surfaces as unplaced so cover mints"
        );

        // A non-excluded sibling exists → packs there, never on the
        // excluded node, even when the excluded node would win
        // MostAllocated.
        let mut loaded = node("n1", "h", CapacityType::Spot, 8, 32 * GI, 100 * GI);
        loaded.requested = (4, 0, 0);
        let fresh = node("n2", "h", CapacityType::Spot, 8, 32 * GI, 100 * GI);
        let (placeable, unplaced) = sim(std::slice::from_ref(&i), &[loaded, fresh]);
        assert_eq!(placed_on(&placeable, "a"), "n2");
        assert!(unplaced.is_empty());

        // Bound short-circuit: a binding on a since-excluded node falls
        // through to the fit-check instead of freezing as "placed" —
        // with no other candidate the intent lands in unplaced and the
        // replacement is pre-minted before the drift reap evicts it.
        let n_only = node("n1", "h", CapacityType::Spot, 8, 32 * GI, 100 * GI);
        let bound: HashMap<String, String> = [("a".to_string(), "node-n1".to_string())].into();
        let (placeable, unplaced) = simulate(
            std::slice::from_ref(&i),
            std::slice::from_ref(&n_only),
            &CellSketches::default(),
            &bound,
            0,
            any_admit,
        );
        assert!(
            placeable.is_empty(),
            "bound-on-excluded falls through, does not short-circuit to placed"
        );
        assert_eq!(unplaced.len(), 1);
    }

    #[test]
    fn simulate_excludes_terminating_nodes() {
        // Partly loaded so the dying node WINS MostAllocated against
        // an empty healthy node ((8-4+4)/8=1.0 > (8-8+4)/8=0.5) — the
        // pre-fix bug placed there because the score loop read it from
        // `free`. With both nodes empty the score ties and `max_by`
        // picks the last element, masking the bug by iteration luck.
        let mut dying =
            set_terminating(node("dying", "h", CapacityType::Spot, 8, 32 * GI, 100 * GI));
        dying.requested = (4, 0, 0);
        let i = intent("a", 4, GI, &[("h", CapacityType::Spot)]);

        // Only a terminating node available → intent is UNPLACED, not
        // "placed" on the dying node. This is the regression: pre-fix
        // the FFD packed `a` onto `dying` and reported deficit=0.
        let (placeable, unplaced) = sim(std::slice::from_ref(&i), std::slice::from_ref(&dying));
        assert!(
            placeable.is_empty(),
            "FFD must not place onto a terminating node"
        );
        assert_eq!(
            unplaced.len(),
            1,
            "intent surfaces as unplaced so cover_deficit mints a replacement"
        );

        // Healthy + terminating: intent packs onto the healthy node
        // ONLY, even though the partly-loaded dying node would win
        // MostAllocated (1.0 vs 0.5) if it were still a candidate.
        let healthy = node("healthy", "h", CapacityType::Spot, 8, 32 * GI, 100 * GI);
        let (placeable, unplaced) = sim(std::slice::from_ref(&i), &[dying.clone(), healthy]);
        assert_eq!(placed_on(&placeable, "a"), "healthy");
        assert!(unplaced.is_empty());

        // Bound short-circuit: a pod already bound to a terminating
        // node's backing Node falls through to the fit-check (the pod
        // is being evicted; the intent should be re-placed). With no
        // other node available it goes to unplaced — `cover_deficit`
        // pre-mints the replacement so it's warm by eviction time.
        let bound: HashMap<String, String> = [("a".to_string(), "node-dying".to_string())].into();
        let (placeable, unplaced) = simulate(
            &[i],
            &[dying],
            &CellSketches::default(),
            &bound,
            0,
            any_admit,
        );
        assert!(
            placeable.is_empty(),
            "bound-to-terminating falls through, does not short-circuit to placed"
        );
        assert_eq!(unplaced.len(), 1);
    }

    /// mb_024(3): a pre-Launch NodeClaim (`status` absent — Karpenter
    /// hasn't reconciled it yet) reads `allocatable` from
    /// `spec.resources.requests` (what cover_deficit asked for), so the
    /// `max_fleet_cores` budget covers it instead of re-minting the
    /// same deficit each tick under Karpenter queue lag.
    #[test]
    fn live_node_spec_resources_fallback() {
        use k8s_openapi::api::core::v1::ResourceRequirements;
        use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
        let nc = NodeClaim {
            metadata: kube::api::ObjectMeta {
                name: Some("pre-launch".into()),
                ..Default::default()
            },
            spec: rio_crds::karpenter::NodeClaimSpec {
                resources: Some(ResourceRequirements {
                    requests: Some(
                        [
                            ("cpu".into(), Quantity("32".into())),
                            ("memory".into(), Quantity("64Gi".into())),
                        ]
                        .into(),
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            },
            status: None,
        };
        let live: LiveNode = nc.into();
        assert_eq!(
            live.allocatable,
            (32, 64 * GI, 0),
            "pre-Launch claim contributes spec.requests to budget (was (0,0,0))"
        );
    }

    #[test]
    fn live_node_capacity_fallback_when_allocatable_absent() {
        // In-flight: Karpenter has populated `capacity` at Launch but
        // kubelet hasn't reported `allocatable` yet.
        let status: NodeClaimStatus = serde_json::from_value(serde_json::json!({
            "capacity": { "cpu": "16", "memory": "64Gi" },
        }))
        .unwrap();
        let nc = NodeClaim {
            metadata: Default::default(),
            spec: Default::default(),
            status: Some(status),
        };
        let live: LiveNode = nc.into();
        assert_eq!(live.allocatable, (16, 64 * GI, 0));
    }

    #[test]
    fn free_subtracts_requested_only_when_registered() {
        let mut n = node("a", "h", CapacityType::Spot, 8, 32 * GI, 100 * GI);
        n.requested = (3, 8 * GI, 10 * GI);
        assert_eq!(n.free(), (5, 24 * GI, 90 * GI));
        n.registered = false;
        assert_eq!(
            n.free(),
            (8, 32 * GI, 100 * GI),
            "in-flight ignores requested"
        );
        // Saturating: over-accounted node reads 0, not underflow.
        n.registered = true;
        n.requested = (99, 0, 0);
        assert_eq!(n.free().0, 0);
    }

    // --- Quantity parsing ----------------------------------------------

    #[test]
    fn parse_bytes_forms() {
        assert_eq!(parse_bytes("0"), 0);
        assert_eq!(parse_bytes("1024"), 1024);
        assert_eq!(parse_bytes("1Ki"), 1024);
        assert_eq!(parse_bytes("31Gi"), 31 * GI);
        assert_eq!(parse_bytes("1.5Gi"), (1.5 * GI as f64) as u64);
        assert_eq!(parse_bytes("2Ti"), 2 << 40);
        assert_eq!(parse_bytes("100M"), 100_000_000);
        assert_eq!(parse_bytes("1k"), 1_000);
        assert_eq!(parse_bytes("1K"), 1_000);
        // DecimalExponent: lowercase `e` falls through to bare parse.
        assert_eq!(parse_bytes("1e6"), 1_000_000);
        assert_eq!(parse_bytes(""), 0);
        assert_eq!(parse_bytes("garbage"), 0);
        assert_eq!(parse_bytes("Gi"), 0, "suffix-only → 0");
    }

    #[test]
    fn parse_cpu_millis_forms() {
        assert_eq!(parse_cpu_millis("64"), 64_000);
        assert_eq!(parse_cpu_millis("136000m"), 136_000);
        assert_eq!(parse_cpu_millis("1.5"), 1_500);
        assert_eq!(parse_cpu_millis("0"), 0);
        assert_eq!(parse_cpu_millis("0m"), 0);
        assert_eq!(parse_cpu_millis("garbage"), 0);
        assert_eq!(parse_cpu_millis(""), 0);
        // Decimal-SI suffixes — apimachinery canonicalizes round
        // multiples of 1000 to these. `"1k"` → 0 was the bug.
        assert_eq!(parse_cpu_millis("1k"), 1_000_000);
        assert_eq!(parse_cpu_millis("10k"), 10_000_000);
        assert_eq!(parse_cpu_millis("2M"), 2_000_000_000);
        assert_eq!(parse_cpu_millis("999"), 999_000); // just below k
        assert_eq!(parse_cpu_millis("1500m"), 1_500); // m via table
        assert_eq!(parse_cpu_millis("500u"), 1); // 0.5 millicore rounds
    }

    #[test]
    fn parse_resources_truncates_millicores() {
        let m: BTreeMap<String, Quantity> = [
            ("cpu".into(), Quantity("7910m".into())),
            ("memory".into(), Quantity("31Gi".into())),
        ]
        .into();
        assert_eq!(parse_resources(&m), (7, 31 * GI, 0));
    }

    // --- cells_of / a_open ---------------------------------------------

    #[test]
    fn cells_of_zips_hw_names_with_cap_label() {
        let i = intent(
            "x",
            4,
            GI,
            &[("h1", CapacityType::Spot), ("h2", CapacityType::OnDemand)],
        );
        assert_eq!(
            cells_of(&i),
            vec![
                Cell("h1".into(), CapacityType::Spot),
                Cell("h2".into(), CapacityType::OnDemand),
            ]
        );
        // hw-agnostic: empty arrays → empty cells.
        assert!(cells_of(&SpawnIntent::default()).is_empty());
    }

    #[test]
    fn cap_from_label_karpenter_forms() {
        assert_eq!(cap_from_label("spot"), Some(CapacityType::Spot));
        assert_eq!(cap_from_label("on-demand"), Some(CapacityType::OnDemand));
        assert_eq!(cap_from_label("od"), None, "PG form, not Karpenter label");
    }

    // --- simulate ------------------------------------------------------

    #[test]
    fn ffd_empty_nodes_all_unplaced() {
        let intents = [
            intent("a", 4, GI, &[("h", CapacityType::Spot)]),
            intent("b", 2, GI, &[("h", CapacityType::Spot)]),
        ];
        let (p, u) = sim(&intents, &[]);
        assert!(p.is_empty());
        assert_eq!(u.len(), 2);
    }

    /// r[ctrl.nodeclaim.ffd-sim]: Ready before forecast, large before
    /// small. 2×8-core nodes; intents: ready-4c, forecast-8c, ready-6c.
    /// Sort = [ready-6c, ready-4c, forecast-8c]. ready-6c → n1 (free 2),
    /// ready-4c → n2 (free 4), forecast-8c → unplaced.
    // r[verify ctrl.nodeclaim.ffd-sim]
    #[test]
    fn ffd_ready_before_forecast_large_before_small() {
        let h = ("h", CapacityType::Spot);
        let nodes = [
            node("n1", "h", CapacityType::Spot, 8, 64 * GI, 100 * GI),
            node("n2", "h", CapacityType::Spot, 8, 64 * GI, 100 * GI),
        ];
        // forecast-8c needs A_open ∋ h:spot → seed lead_time > eta.
        let mut sk = CellSketches::default();
        sk.cell_mut(&Cell("h".into(), CapacityType::Spot))
            .z_active
            .add(60.0);
        let intents = [
            intent("ready-4c", 4, GI, &[h]),
            forecast(intent("forecast-8c", 8, GI, &[h]), 10.0),
            intent("ready-6c", 6, GI, &[h]),
        ];
        let (p, u) = sim_sk(&intents, &nodes, &sk);
        assert_eq!(p.len(), 2);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].intent_id, "forecast-8c");
        // ready-6c placed first (largest ready) → n1 (both empty, n1
        // listed first → equal score, max_by keeps last → actually n2).
        // MostAllocated tiebreak on equal-empty nodes: max_by returns
        // the LAST max, so n2. Then ready-4c sees n2 at 6/8 vs n1 at
        // 0/8 → picks n2? No: n2 free=2 < 4. → n1.
        assert_eq!(placed_on(&p, "ready-6c"), "n2");
        assert_eq!(placed_on(&p, "ready-4c"), "n1");
    }

    /// MostAllocated picks the node that ends up most-utilized.
    /// A(12c, 4 used), B(12c, 0 used). Intent 4c: A→(4+4)/12=0.67,
    /// B→(0+4)/12=0.33 → A.
    #[test]
    fn ffd_most_allocated_bin_select() {
        let mut a = node("A", "h", CapacityType::Spot, 12, 64 * GI, 100 * GI);
        a.requested = (4, 0, 0);
        let b = node("B", "h", CapacityType::Spot, 12, 64 * GI, 100 * GI);
        let intents = [intent("x", 4, GI, &[("h", CapacityType::Spot)])];
        let (p, u) = sim(&intents, &[a, b]);
        assert!(u.is_empty());
        assert_eq!(placed_on(&p, "x"), "A");
    }

    /// MostAllocated tracks the running tally: after placing on B,
    /// the next intent sees B as more-allocated than A.
    #[test]
    fn ffd_most_allocated_tracks_running_free() {
        let nodes = [
            node("A", "h", CapacityType::Spot, 8, 64 * GI, 100 * GI),
            node("B", "h", CapacityType::Spot, 8, 64 * GI, 100 * GI),
        ];
        // Three 3c intents. First → B (equal score, max_by last).
        // Second: A=3/8, B=6/8 → B. Third: B free=2<3 → A.
        let intents = [
            intent("i1", 3, GI, &[("h", CapacityType::Spot)]),
            intent("i2", 3, GI, &[("h", CapacityType::Spot)]),
            intent("i3", 3, GI, &[("h", CapacityType::Spot)]),
        ];
        let (p, u) = sim(&intents, &nodes);
        assert!(u.is_empty());
        assert_eq!(placed_on(&p, "i1"), "B");
        assert_eq!(placed_on(&p, "i2"), "B");
        assert_eq!(placed_on(&p, "i3"), "A");
    }

    /// Forecast intent with `eta=40s`, A={h1,h2}. lead_time[h1]=30,
    /// lead_time[h2]=60. A_open={h2}; h1 nodes ineligible.
    #[test]
    fn ffd_a_open_gates_forecast_by_lead_time() {
        let mut sk = CellSketches::default();
        sk.cell_mut(&Cell("h1".into(), CapacityType::Spot))
            .z_active
            .add(30.0);
        sk.cell_mut(&Cell("h2".into(), CapacityType::Spot))
            .z_active
            .add(60.0);
        let nodes = [
            node("n-h1", "h1", CapacityType::Spot, 8, 64 * GI, 100 * GI),
            node("n-h2", "h2", CapacityType::Spot, 8, 64 * GI, 100 * GI),
        ];
        let i = forecast(
            intent(
                "fc",
                4,
                GI,
                &[("h1", CapacityType::Spot), ("h2", CapacityType::Spot)],
            ),
            40.0,
        );
        // A_open directly: only h2.
        assert_eq!(a_open(&i, &sk), vec![Cell("h2".into(), CapacityType::Spot)]);
        let (p, u) = sim_sk(&[i], &nodes, &sk);
        assert!(u.is_empty());
        assert_eq!(placed_on(&p, "fc"), "n-h2");
        // Ready intent on the same cells ignores lead_time.
        let r = intent(
            "rd",
            4,
            GI,
            &[("h1", CapacityType::Spot), ("h2", CapacityType::Spot)],
        );
        assert_eq!(a_open(&r, &sk).len(), 2);
    }

    #[test]
    fn ffd_affinity_mismatch_unplaced() {
        let nodes = [node("n", "h1", CapacityType::Spot, 8, 64 * GI, 100 * GI)];
        let intents = [intent("x", 4, GI, &[("h2", CapacityType::Spot)])];
        let (p, u) = sim(&intents, &nodes);
        assert!(p.is_empty());
        assert_eq!(u.len(), 1);
        // Same hw, wrong cap → also mismatch.
        let intents = [intent("y", 4, GI, &[("h1", CapacityType::OnDemand)])];
        let (p, u) = sim(&intents, &nodes);
        assert!(p.is_empty());
        assert_eq!(u.len(), 1);
    }

    #[test]
    fn ffd_cell_less_node_ineligible() {
        let mut n = node("n", "h", CapacityType::Spot, 8, 64 * GI, 100 * GI);
        n.cell = None;
        let intents = [intent("x", 4, GI, &[("h", CapacityType::Spot)])];
        let (p, u) = sim(&intents, &[n]);
        assert!(p.is_empty());
        assert_eq!(u.len(), 1);
    }

    /// Cold-start `fit=None` → `hw_class_names=[]`. Places on any
    /// node whose hw-class arch matches `system_to_arch(intent.system)`.
    /// Unmappable system → unplaced. Arch mismatch → unplaced.
    #[test]
    fn ffd_hw_agnostic_intent_places_by_arch() {
        let nodes = [
            node("nx", "h-x86", CapacityType::Spot, 8, 64 * GI, 100 * GI),
            node("na", "h-arm", CapacityType::Spot, 8, 64 * GI, 100 * GI),
        ];
        let hw_admits = |h: &str, a: Option<&str>, _f: &[String]| match h {
            "h-x86" => a == Some("amd64"),
            "h-arm" => a == Some("arm64"),
            _ => false,
        };
        let agn = |id: &str, sys: &str| SpawnIntent {
            intent_id: id.into(),
            cores: 4,
            mem_bytes: GI,
            disk_bytes: GI,
            system: sys.into(),
            ready: Some(true),
            ..Default::default()
        };
        let intents = [
            agn("x", "x86_64-linux"),
            agn("a", "aarch64-linux"),
            agn("u", ""), // unmappable → unplaced
        ];
        let none = HashMap::new();
        let (p, u) = simulate(
            &intents,
            &nodes,
            &CellSketches::default(),
            &none,
            0,
            hw_admits,
        );
        assert_eq!(p.len(), 2);
        assert_eq!(placed_on(&p, "x"), "nx");
        assert_eq!(placed_on(&p, "a"), "na");
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].intent_id, "u");
        // No matching-arch node → unplaced (cover_deficit provisions).
        let (p, u) = simulate(
            &[agn("x2", "x86_64-linux")],
            &nodes[1..],
            &CellSketches::default(),
            &none,
            0,
            hw_admits,
        );
        assert!(p.is_empty());
        assert_eq!(u.len(), 1);
    }

    /// §13d STRIKE-7 (r30 mb_012): `simulate`'s agnostic path passes
    /// `required_features` to `hw_admits`, so a `hw_class_names=[]`
    /// kvm intent can NOT FFD-place onto a non-metal node and a
    /// featureless intent can NOT land on kvm-tainted metal. Pre-fix
    /// `simulate` only knew arch — a cold-start kvm intent FFD-placed
    /// onto any amd64 node, the deficit appeared covered, no metal
    /// NodeClaim minted, kvm pod CrashLoopBackOff on ENXIO `/dev/kvm`.
    #[test]
    fn ffd_agnostic_intent_filtered_by_features() {
        // One non-metal x86 node (provides=[]), one metal x86 node
        // (provides=[kvm]).
        let nodes = [
            node("nstd", "std-x86", CapacityType::Spot, 8, 64 * GI, 100 * GI),
            node(
                "nmetal",
                "metal-x86",
                CapacityType::OnDemand,
                8,
                64 * GI,
                100 * GI,
            ),
        ];
        // hw_admits: arch matches everything (both x86); features gate.
        let provides = |h: &str| -> Vec<String> {
            if h == "metal-x86" {
                vec!["kvm".into()]
            } else {
                vec![]
            }
        };
        let hw_admits = |h: &str, _a: Option<&str>, f: &[String]| {
            rio_common::k8s::features_compatible(f, &provides(h))
        };
        let agn = |id: &str, features: &[&str]| SpawnIntent {
            intent_id: id.into(),
            cores: 4,
            mem_bytes: GI,
            disk_bytes: GI,
            system: "x86_64-linux".into(),
            required_features: features.iter().map(|s| (*s).to_string()).collect(),
            ready: Some(true),
            ..Default::default()
        };
        let none = HashMap::new();
        // kvm intent → only the metal node admits it.
        let (p, u) = simulate(
            &[agn("kvm", &["kvm"])],
            &nodes,
            &CellSketches::default(),
            &none,
            0,
            hw_admits,
        );
        assert_eq!(p.len(), 1, "kvm intent placed (on metal)");
        assert_eq!(placed_on(&p, "kvm"), "nmetal");
        assert!(u.is_empty());
        // featureless intent → only the non-metal node admits it
        // (∅-guard: provides=[kvm] rejects required=[]).
        let (p, u) = simulate(
            &[agn("plain", &[])],
            &nodes,
            &CellSketches::default(),
            &none,
            0,
            hw_admits,
        );
        assert_eq!(p.len(), 1);
        assert_eq!(placed_on(&p, "plain"), "nstd");
        assert!(u.is_empty());
        // kvm intent + only non-metal nodes → unplaced (cover_deficit
        // mints metal). Pre-fix it FFD-placed onto std-x86 → no mint.
        let (p, u) = simulate(
            &[agn("kvm2", &["kvm"])],
            &nodes[..1],
            &CellSketches::default(),
            &none,
            0,
            hw_admits,
        );
        assert!(p.is_empty(), "kvm intent must NOT place on non-metal node");
        assert_eq!(u.len(), 1);
    }

    /// r35 B1 (validator amendment B2): the agnostic-fallback gate is
    /// "at least one non-trivial constraint axis (arch OR features)".
    /// A featureless arch-mappable hw-agnostic intent
    /// (`hw_class_names=[]`, `required_features=[]`,
    /// `system="x86_64-linux"`) MUST keep placing by arch — `arch=Some
    /// ∧ features=[]` is the cold-start non-FOD case the FFD doc-comment
    /// documents as load-bearing. A rewrite gating on `!f.is_empty()`
    /// alone would over-mint via `cover_deficit` for every cold-start
    /// non-FOD intent FFD can no longer place. This test pins the
    /// invariant so a future refactor can't quietly drop the arch axis.
    #[test]
    fn ffd_simulate_places_hw_agnostic_featureless_on_arch_match() {
        let nodes = [
            node("nx", "h-x86", CapacityType::Spot, 8, 64 * GI, 100 * GI),
            node("na", "h-arm", CapacityType::Spot, 8, 64 * GI, 100 * GI),
        ];
        // hw_admits routes by arch only (features unconstrained).
        let hw_admits = |h: &str, a: Option<&str>, _f: &[String]| match h {
            "h-x86" => a.is_none_or(|a| a == "amd64"),
            "h-arm" => a.is_none_or(|a| a == "arm64"),
            _ => false,
        };
        // featureless arch-mappable hw-agnostic intent → places on
        // the arch-matching node.
        let i = SpawnIntent {
            intent_id: "plain".into(),
            cores: 4,
            mem_bytes: GI,
            disk_bytes: GI,
            system: "x86_64-linux".into(),
            ready: Some(true),
            ..Default::default()
        };
        let none = HashMap::new();
        let (p, u) = simulate(
            std::slice::from_ref(&i),
            &nodes,
            &CellSketches::default(),
            &none,
            0,
            hw_admits,
        );
        assert_eq!(
            p.len(),
            1,
            "featureless arch-mappable hw-agnostic intent must place by arch"
        );
        assert_eq!(placed_on(&p, "plain"), "nx");
        assert!(u.is_empty());
        // featureless arch-UNmappable → no constraint axis → unplaced
        // (cover_deficit's no_hosting_class is the right answer; no
        // node can be minted without an arch or a feature to route on).
        let mut iu = i.clone();
        iu.intent_id = "unmappable".into();
        iu.system = "darwin-pdp11".into();
        let (p, u) = simulate(&[iu], &nodes, &CellSketches::default(), &none, 0, hw_admits);
        assert!(
            p.is_empty(),
            "featureless arch-unmappable intent has no constraint axis"
        );
        assert_eq!(u.len(), 1);
        // featured arch-UNmappable (`builtin` FOD) → places by feature.
        let mut iff = i;
        iff.intent_id = "fod".into();
        iff.system = "builtin".into();
        iff.required_features = vec!["fetcher".into()];
        let feat_admits =
            |h: &str, _a: Option<&str>, f: &[String]| h == "h-x86" && f == ["fetcher"];
        let (p, u) = simulate(
            &[iff],
            &nodes,
            &CellSketches::default(),
            &none,
            0,
            feat_admits,
        );
        assert_eq!(p.len(), 1, "builtin FOD must place by feature");
        assert_eq!(placed_on(&p, "fod"), "nx");
        assert!(u.is_empty());
    }

    #[test]
    fn ffd_in_flight_node_placement_flagged() {
        let mut n = node("n", "h", CapacityType::Spot, 8, 64 * GI, 100 * GI);
        n.registered = false;
        let intents = [intent("x", 4, GI, &[("h", CapacityType::Spot)])];
        let (p, _) = sim(&intents, &[n]);
        assert_eq!(p.len(), 1);
        assert!(p[0].2, "in_flight = !registered");
    }

    /// mb_019(A): FFD compares against the SAME `(c,m,d)` triple the
    /// pod will request — `intent_pod_footprint` (= `disk×headroom +
    /// fuse + log`), NOT raw `disk_bytes`. Node 200Gi free; 4 intents
    /// each `disk_bytes=1Gi headroom=1.5` with `fuse=50Gi` → footprint
    /// ≈52.5Gi each → only 3 fit. With raw `disk_bytes` FFD would pack
    /// all 4 (decrementing 1Gi each); kube-scheduler binds 3, the 4th
    /// sits Pending with no covering NodeClaim.
    #[test]
    fn ffd_disk_uses_pod_footprint() {
        let n = node("n", "h", CapacityType::Spot, 32, 64 * GI, 200 * GI);
        let intents: Vec<_> = (0..4)
            .map(|k| {
                let mut i = intent(&format!("i{k}"), 4, GI, &[("h", CapacityType::Spot)]);
                i.disk_bytes = GI;
                i.disk_headroom_factor = Some(1.5);
                i
            })
            .collect();
        let fuse = 50 * GI;
        let (p, u) = simulate(
            &intents,
            &[n],
            &CellSketches::default(),
            &HashMap::new(),
            fuse,
            any_admit,
        );
        // footprint = 1×1.5 + 50 + 1 (log) = 52.5Gi → ⌊200/52.5⌋ = 3.
        assert_eq!(
            p.len(),
            3,
            "footprint-based fit (was 4 with raw disk_bytes)"
        );
        assert_eq!(u.len(), 1);
    }

    /// bug_069: an intent already bound (the tick's Pod LIST saw its pod
    /// with `spec.nodeName`) short-circuits to `placeable` with NO
    /// fit-check. Its own pod's (32c) is in `requested` so `free.0=0`;
    /// fit-checking would evict it → orphan-reap loop on the
    /// progressing-ContainerCreating pod.
    #[test]
    fn ffd_short_circuits_bound_intent() {
        // Tight-fit: 32c node, intent X=32c, X's pod already bound
        // (requested.0=32 → free.0=0).
        let mut n = node("nc", "h", CapacityType::Spot, 32, 64 * GI, 200 * GI);
        n.node_name = Some("ip-10-0-1-5".into());
        n.requested = (32, 32 * GI, 100 * GI);
        let x = intent("X", 32, 32 * GI, &[("h", CapacityType::Spot)]);
        // Without bound short-circuit: free.0=0 < 32 → unplaced.
        let (p, u) = sim(std::slice::from_ref(&x), std::slice::from_ref(&n));
        assert!(p.is_empty(), "without bound: tight-fit evicted by own pod");
        assert_eq!(u.len(), 1);
        // With bound={X→ip-10-0-1-5}: short-circuit to placeable.
        let bound: HashMap<String, String> = [("X".into(), "ip-10-0-1-5".into())].into();
        let (p, u) = simulate(
            std::slice::from_ref(&x),
            std::slice::from_ref(&n),
            &CellSketches::default(),
            &bound,
            0,
            any_admit,
        );
        assert_eq!(p.len(), 1, "bound intent placeable on its actual node");
        assert_eq!(p[0].1, "nc");
        assert!(!p[0].2, "registered node → in_flight=false");
        assert!(u.is_empty());
        // Bound to a node not in `live` (NodeClaim deleted; race) →
        // falls through to fit-check → unplaced.
        let stale: HashMap<String, String> = [("X".into(), "ip-gone".into())].into();
        let (p, _) = simulate(
            std::slice::from_ref(&x),
            std::slice::from_ref(&n),
            &CellSketches::default(),
            &stale,
            0,
            any_admit,
        );
        assert!(p.is_empty(), "stale bound → falls through to fit-check");
    }

    /// FFD never overcommits any node on any axis. Deterministic
    /// many-intent / many-node check (proptest equivalent without the
    /// dep): three node sizes, intents that overflow total capacity.
    #[test]
    fn ffd_never_overcommits() {
        let h = ("h", CapacityType::Spot);
        let nodes = [
            node("s", "h", CapacityType::Spot, 4, 8 * GI, 50 * GI),
            node("m", "h", CapacityType::Spot, 8, 32 * GI, 100 * GI),
            node("l", "h", CapacityType::Spot, 16, 64 * GI, 200 * GI),
        ];
        // 20 intents × 3c = 60c demand; capacity = 28c → ≤9 place.
        let intents: Vec<_> = (0..20)
            .map(|k| intent(&format!("i{k}"), 3, 4 * GI, &[h]))
            .collect();
        let (p, u) = sim(&intents, &nodes);
        assert_eq!(p.len() + u.len(), 20);
        for n in &nodes {
            let (c, m, d) = p
                .iter()
                .filter(|(_, nn, _)| nn == &n.name)
                .fold((0u32, 0u64, 0u64), |(c, m, d), (i, _, _)| {
                    (c + i.cores, m + i.mem_bytes, d + i.disk_bytes)
                });
            assert!(
                c <= n.allocatable.0,
                "{}: cpu {} > {}",
                n.name,
                c,
                n.allocatable.0
            );
            assert!(m <= n.allocatable.1, "{}: mem", n.name);
            assert!(d <= n.allocatable.2, "{}: disk", n.name);
        }
        // Exactly ⌊4/3⌋+⌊8/3⌋+⌊16/3⌋ = 1+2+5 = 8 place (FFD packs
        // largest-first, here uniform 3c so just bin-fills).
        assert_eq!(p.len(), 8);
    }

    /// F9: per-cell `(on_registered, on_inflight)` placement split.
    /// Feeds `observe_hit_ratio` so `schmitt_adjust` widens
    /// `lead_time_q` for cells where placements land mostly in-flight.
    #[test]
    fn per_cell_hit_ratio_splits_by_node_cell() {
        let mut nodes = vec![
            node("r1", "h1", CapacityType::Spot, 8, 0, 0),
            node("r2", "h2", CapacityType::Spot, 8, 0, 0),
            node("if", "h1", CapacityType::Spot, 8, 0, 0),
        ];
        nodes[2].registered = false;
        let p = |n: &str, inf: bool| -> Placement { (SpawnIntent::default(), n.into(), inf) };
        let placeable = vec![
            p("r1", false),
            p("r1", false),
            p("if", true),
            p("r2", false),
            // Placement on a node not in `live` (race) → ignored.
            p("gone", false),
        ];
        let by = per_cell_hit_ratio(&placeable, &nodes);
        let h1 = Cell("h1".into(), CapacityType::Spot);
        let h2 = Cell("h2".into(), CapacityType::Spot);
        assert_eq!(by[&h1], (2, 1), "h1: 2 on r1 (reg) + 1 on if (inflight)");
        assert_eq!(by[&h2], (1, 0));
        assert_eq!(by.len(), 2);
    }

    #[test]
    fn ffd_intent_id_tiebreak_stable() {
        // Equal (ready, cores, mem) → intent_id ascending. Ensures
        // deterministic placement across ticks (no flapping).
        let nodes = [node("n", "h", CapacityType::Spot, 4, 64 * GI, 100 * GI)];
        let intents = [
            intent("zz", 4, GI, &[("h", CapacityType::Spot)]),
            intent("aa", 4, GI, &[("h", CapacityType::Spot)]),
        ];
        let (p, u) = sim(&intents, &nodes);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].0.intent_id, "aa", "intent_id asc tiebreak");
        assert_eq!(u[0].intent_id, "zz");
    }

    // r[verify ctrl.nodeclaim.sim-window]
    /// W9-AU (yield-count + schedulability, structural): on a
    /// current_thread runtime a canary task can run DURING the walk iff
    /// the walk yields — canary starvation ⇔ zero yields, with no
    /// wall-clock axis (deterministic under load, per the
    /// ci-failure-patterns structural preference). The shipped sync
    /// walk starves the canary for the whole tick (the B4 shape); the
    /// windowed walk hands the executor back every FFD_YIELD_QUANTUM
    /// intents.
    #[tokio::test(flavor = "current_thread")]
    async fn windowed_walk_yields_between_chunks() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let n = FFD_YIELD_QUANTUM * 8;
        let intents: Vec<SpawnIntent> = (0..n)
            .map(|k| intent(&format!("i{k:05}"), 4, GI, &[("h", CapacityType::Spot)]))
            .collect();
        let nodes = [node("n", "h", CapacityType::Spot, 8, 64 * GI, 100 * GI)];
        let ran = Arc::new(AtomicUsize::new(0));
        let canary = {
            let ran = Arc::clone(&ran);
            tokio::spawn(async move {
                loop {
                    ran.fetch_add(1, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                }
            })
        };
        // Let the canary start, then snapshot.
        tokio::task::yield_now().await;
        let before = ran.load(Ordering::SeqCst);
        let sketches = CellSketches::default();
        let bound = HashMap::new();
        let out = simulate_windowed(
            &intents,
            &nodes,
            &sketches,
            &bound,
            &HashSet::new(),
            0,
            u64::MAX, // capacity-unbounded: this test pins yielding, not the window
            0,
            &WindowMintability::permissive(),
            any_admit,
        )
        .await;
        let during = ran.load(Ordering::SeqCst) - before;
        canary.abort();
        assert_eq!(
            out.placeable.len() + out.unplaced.len(),
            n,
            "window admits everything at u64::MAX"
        );
        assert!(
            during >= 4,
            "the walk must yield to the runtime between chunks \
             (canary ran {during}x during a {n}-intent walk; 0 <=> the \
             shipped sync-walk starvation)"
        );
    }

    // r[verify ctrl.nodeclaim.sim-window]
    /// W9-AU sentinel-oracle face (the deferred post-S3 leg; H3′ item
    /// 1 — the §1.5-3 composition: the oracle was authored against
    /// S3's t0 sentinel-API post and is consumed here at the LANDED
    /// API, zero delta): the REAL D5 guard — `guard::spawn` probing
    /// THIS test's current_thread runtime, the production freeze-
    /// attribution sentinel — observes the main domain SCHEDULABLE
    /// while windowed walks run back-to-back: probes land at the
    /// walk's yield points, so `main_skew()` stays far under the
    /// stall threshold and `main_stalls()` records ZERO episodes.
    ///
    /// Witness statement (R16, scoped honestly): this face certifies
    /// the ORACLE COMPOSITION — the same `GuardHandle::main_skew`
    /// atomics that attribute production freezes read a schedulable
    /// runtime through the windowed walk. The starvation
    /// DISCRIMINATOR is the canary face above (deterministic,
    /// wall-clock-free): at this population a sync walk completes in
    /// milliseconds and could not trip any honest threshold, so a
    /// magnitude assertion alone cannot discriminate — the pair is
    /// the witness. Slack budget (ci-failure-patterns "widen with
    /// documented slack"): threshold 2s vs µs-scale yield cadence —
    /// 6 orders of magnitude; an OS-level process pause >2s is the
    /// only false-red and would red the VM tier first. (ppppp:
    /// atomics read directly — no recorder snapshot to drain.)
    #[tokio::test(flavor = "current_thread")]
    async fn windowed_walk_keeps_the_main_domain_schedulable() {
        use std::time::Duration;
        let shutdown = rio_common::signal::Token::new();
        let guard = crate::guard::spawn(
            tokio::runtime::Handle::current(),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            crate::guard::GuardConfig {
                // Ephemeral port: this face never queries HTTP — the
                // oracle is the lock-free handle.
                health_addr: ([127, 0, 0, 1], 0).into(),
                probe_interval: Duration::from_millis(10),
                stall_threshold: Duration::from_secs(2),
                ready_probe_budget: Duration::from_secs(1),
            },
            shutdown.clone(),
        );
        let n = FFD_YIELD_QUANTUM * 8;
        let intents: Vec<SpawnIntent> = (0..n)
            .map(|k| intent(&format!("i{k:05}"), 4, GI, &[("h", CapacityType::Spot)]))
            .collect();
        let nodes = [node("n", "h", CapacityType::Spot, 8, 64 * GI, 100 * GI)];
        let sketches = CellSketches::default();
        let bound = HashMap::new();
        // Walk back-to-back across ≥10 probe intervals so sentinel
        // probes land DURING walking (the population: the loop bound
        // sizes it; the assertions are threshold/episode-shaped).
        let t0 = std::time::Instant::now();
        let mut walks = 0u32;
        while t0.elapsed() < Duration::from_millis(120) {
            let out = simulate_windowed(
                &intents,
                &nodes,
                &sketches,
                &bound,
                &HashSet::new(),
                0,
                u64::MAX,
                0,
                &WindowMintability::permissive(),
                any_admit,
            )
            .await;
            assert_eq!(out.placeable.len() + out.unplaced.len(), n);
            walks += 1;
        }
        let skew = guard.main_skew();
        let stalls = guard.main_stalls();
        shutdown.cancel();
        assert_eq!(
            stalls, 0,
            "zero stall episodes across {walks} windowed walks (the \
             sentinel saw every probe scheduled at a yield point)"
        );
        assert!(
            skew < Duration::from_secs(2),
            "main-domain skew {skew:?} stays under the stall threshold \
             through {walks} back-to-back windowed walks"
        );
    }

    // r[verify ctrl.nodeclaim.sim-window]
    /// **W10-AJ (round-10 merged_bug_012, the starvation amplifier).**
    /// PROPOSITION at the head-exceeds-window quantifier: a bucket
    /// head whose own cores exceed the WHOLE window defers WITHOUT
    /// blocking its class — siblings are admitted across EVERY
    /// rotation (count-based: pre-fix the whole bucket starved at
    /// every tick offset, the cores-desc sort keeping the oversized
    /// head permanently in front).
    ///
    /// Pre-fix red (head-block law, captured before the skip landed):
    ///   rotation 0: admitted from class A = 0 (expected 3)
    ///   "can-never-fit head must defer, not starve its bucket"
    #[test]
    fn w10_aj_can_never_fit_head_defers_without_starving_bucket() {
        // Class A: one 64-core head (window is 32 — can NEVER fit) +
        // three 8-core siblings. Class B: flood of 8-core intents so
        // the fast path (everything fits) cannot trigger.
        let mut intents: Vec<SpawnIntent> = vec![
            intent("a-huge", 64, GI, &[("ca", CapacityType::Spot)]),
            intent("a-s1", 8, GI, &[("ca", CapacityType::Spot)]),
            intent("a-s2", 8, GI, &[("ca", CapacityType::Spot)]),
            intent("a-s3", 8, GI, &[("ca", CapacityType::Spot)]),
        ];
        intents.extend(
            (0..20).map(|k| intent(&format!("b{k:02}"), 8, GI, &[("cb", CapacityType::Spot)])),
        );

        // Across every rotation offset, class-A siblings get their RR
        // share and the oversized head is ALWAYS in the remainder.
        for tick in 0..4u64 {
            let mut sorted = intents.clone();
            sort_ffd(&mut sorted);
            let (admitted, remainder) = admit_window(
                sorted,
                &HashMap::new(),
                &HashSet::new(),
                32,
                tick,
                &WindowMintability::permissive(),
            );
            let a_adm = admitted
                .iter()
                .filter(|i| i.intent_id.starts_with("a-s"))
                .count();
            assert!(
                a_adm >= 1,
                "rotation {tick}: class-A siblings admitted past the \
                 can-never-fit head (pre-fix: 0 — the bucket starved)"
            );
            assert!(
                remainder.iter().any(|i| i.intent_id == "a-huge"),
                "rotation {tick}: the oversized head is typed remainder \
                 (deferred until capacity grows), never admitted"
            );
            assert!(
                !admitted.iter().any(|i| i.intent_id == "a-huge"),
                "rotation {tick}: 64c can never enter a 32c window"
            );
        }
    }

    // r[verify ctrl.nodeclaim.sim-window]
    // r[verify ctrl.pool.demand-completeness]
    /// **W10-AI, the window-exemption half (round-10 merged_bug_012).**
    /// A Job-HOLDING intent (live pod — bound or Pending) bypasses the
    /// admission window exactly like a bound intent: its Job already
    /// exists, so deferring it cannot pace anything — it only mis-read
    /// existing demand as remainder (the still-wanted Pending Job then
    /// fell to the orphan reap pre-fix). The sim outcome carries the
    /// per-intent deferred letter for everything genuinely windowed
    /// out.
    #[tokio::test]
    async fn w10_ai_job_held_intent_bypasses_window() {
        let live: Vec<LiveNode> = vec![];
        let sketches = CellSketches::default();
        let bound = HashMap::new();
        // 5 × 8c intents against an 8-core window: only one admits...
        let mut intents: Vec<SpawnIntent> = (0..5)
            .map(|k| intent(&format!("i{k}"), 8, GI, &[("ca", CapacityType::Spot)]))
            .collect();
        intents.push(intent("held", 8, GI, &[("ca", CapacityType::Spot)]));
        // ...but "held" carries a live Pending Job — window-exempt.
        let job_held: HashSet<String> = ["held".to_string()].into();
        let out = simulate_windowed(
            &intents,
            &live,
            &sketches,
            &bound,
            &job_held,
            0,
            8,
            0,
            &WindowMintability::permissive(),
            |_, _, _| true,
        )
        .await;
        assert!(
            !out.deferred.iter().any(|i| i.intent_id == "held"),
            "a Job-holding intent is never window-deferred"
        );
        assert_eq!(
            out.deferred.len(),
            4,
            "the other four split one 8c window slot: one admitted, \
             four deferred — each a typed per-intent letter"
        );
        assert_eq!(
            out.remainder,
            SimRemainder {
                intents: 4,
                cores: 32
            },
            "the aggregate stays a metric beside the letters"
        );
    }

    // r[verify ctrl.nodeclaim.sim-window]
    /// The admission law: bound intents bypass the window; unbound
    /// admission is cores-capped and round-robin-fair across hw-class
    /// buckets (a pathological class cannot evict a sibling's share);
    /// priority order is preserved within a bucket; the remainder is
    /// typed, never dropped.
    #[test]
    fn window_admits_capacity_fairly_and_types_remainder() {
        // Class A floods (100 × 8c), class B wants 2 × 8c. Window fits
        // only 32 cores of unbound demand → RR admits from BOTH
        // classes instead of letting A's flood evict B.
        let mut intents: Vec<SpawnIntent> = (0..100)
            .map(|k| intent(&format!("a{k:03}"), 8, GI, &[("ca", CapacityType::Spot)]))
            .collect();
        intents.push(intent("b000", 8, GI, &[("cb", CapacityType::Spot)]));
        intents.push(intent("b001", 8, GI, &[("cb", CapacityType::Spot)]));
        // A bound intent rides for free regardless of the window.
        let mut bound_i = intent("zbound", 8, GI, &[("ca", CapacityType::Spot)]);
        bound_i.cores = 8;
        intents.push(bound_i);
        let bound: HashMap<String, String> = [("zbound".to_string(), "node-n".to_string())].into();
        let mut sorted = intents.clone();
        sort_ffd(&mut sorted);
        let (admitted, remainder) = admit_window(
            sorted,
            &bound,
            &HashSet::new(),
            32,
            0,
            &WindowMintability::permissive(),
        );
        let a_adm = admitted
            .iter()
            .filter(|i| i.intent_id.starts_with('a'))
            .count();
        let b_adm = admitted
            .iter()
            .filter(|i| i.intent_id.starts_with('b'))
            .count();
        assert!(
            admitted.iter().any(|i| i.intent_id == "zbound"),
            "bound intents bypass the window"
        );
        assert_eq!(b_adm, 2, "RR fairness: the sibling class keeps its share");
        assert_eq!(a_adm, 2, "32 cores = 4 unbound slots, split 2/2 by RR");
        assert_eq!(
            remainder.len(),
            102 - a_adm - b_adm,
            "every non-admitted unbound intent is in the typed remainder"
        );
        // Priority preserved within a bucket: admitted A-intents are
        // the bucket's head in sort order (a000, a001 — id asc among
        // equal keys).
        let a_ids: Vec<&str> = admitted
            .iter()
            .filter(|i| i.intent_id.starts_with('a'))
            .map(|i| i.intent_id.as_str())
            .collect();
        assert_eq!(a_ids, ["a000", "a001"], "bucket head, never skipped past");
    }

    // r[verify ctrl.nodeclaim.sim-window]
    /// W9-AV (window-starved supply unconstructible): at the
    /// capacity-derived window, the windowed pipeline's unplaced
    /// residual demands at least everything the budget brake can mint —
    /// the mint vector equals the full-set pipeline's. (The negation is
    /// reachable: window_cores=1 starves — the strawman red disclosed
    /// in the commit body; the capacity derivation cannot.)
    #[test]
    fn window_never_starves_supply() {
        use super::super::cover;
        let n = 64;
        let intents: Vec<SpawnIntent> = (0..n)
            .map(|k| intent(&format!("i{k:03}"), 8, GI, &[("h", CapacityType::Spot)]))
            .collect();
        // No live nodes: everything is deficit. Budget allows 4 claims
        // of 16c each.
        let live: Vec<LiveNode> = Vec::new();
        let budget = 64u32;
        let sketches = CellSketches::default();
        let bound = HashMap::new();
        let mint = |unplaced: &[SpawnIntent]| -> Vec<(u32, u64, u64)> {
            let none = std::collections::HashSet::new();
            let known: std::collections::HashSet<Cell> =
                [Cell("h".into(), CapacityType::Spot)].into();
            let (by_cell, _) = cover::assign_to_cells(
                unplaced,
                &sketches,
                &none,
                &known,
                cover::cell_rank,
                |_, _| None,
            );
            by_cell
                .iter()
                .flat_map(|(cell, u)| {
                    cover::sizing(
                        cell,
                        u,
                        &cover::SizingCfg {
                            max_node_cores: 16,
                            max_node_mem: 256 * GI,
                            max_node_disk: 450 * GI,
                            budget,
                            fuse_cache_bytes: 0,
                        },
                    )
                    .claims
                })
                .collect()
        };
        // Full set (the pre-window truth).
        let (_, full_unplaced) = simulate(&intents, &live, &sketches, &bound, 0, any_admit);
        let full_mint = mint(&full_unplaced);
        // Windowed at the capacity-derived bound (live free = 0).
        let window = sim_window_cores(0, u64::from(budget));
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let out = rt.block_on(simulate_windowed(
            &intents,
            &live,
            &sketches,
            &bound,
            &HashSet::new(),
            0,
            window,
            0,
            &WindowMintability::permissive(),
            any_admit,
        ));
        assert!(
            out.remainder.intents > 0,
            "premise: the window actually truncated (else the test is vacuous)"
        );
        let windowed_mint = mint(&out.unplaced);
        assert_eq!(
            windowed_mint, full_mint,
            "the windowed deficit mints exactly what the full set would \
             (window ≥ the mint law's per-tick consumption)"
        );
    }

    // r[verify ctrl.nodeclaim.sim-window]
    /// **W11-AE (merged_bug_053)** — *proposition: a provably-
    /// unmintable head never consumes a window rotation — mintable
    /// sibling cores are admitted AND MINTED every tick; population:
    /// the mask/budget/geometry product on a multi-class window under
    /// contention, BOTH rotation phases (the head's rotation-first
    /// tick and the sibling-first tick), with the mint comparison run
    /// through the production `assign_to_cells` + `sizing` pipeline
    /// at each class's own budget.*
    ///
    /// The pre-fix shape is pinned as a PERMANENT NEGATIVE CONTROL
    /// (the falsify twin): [`WindowMintability::permissive`] IS the
    /// pre-fix window (no mintability axis), and under it the 24c
    /// head — whose class budget (8) the mint law will provably
    /// refuse (`budget < chunk`, cover.rs) — admits on its
    /// rotation-first tick, eats the 31-core window, blocks the
    /// sibling bucket entirely, and mints zero: the NET-ZERO tick at
    /// 1/K frequency, persistent under the cores-desc sort. The
    /// sizing doc's old "supply is never window-starved" claim was
    /// quantitatively false on exactly this cell;
    /// `window_never_starves_supply` is single-class and
    /// `window_admits_capacity_fairly_and_types_remainder` never ran
    /// the mint comparison — this is the missing witness.
    #[test]
    fn window_admits_mintable_siblings_every_tick() {
        use super::super::cover;
        // Two classes; no live nodes (live free = 0); window 31
        // forces contention (Σ unbound = 40 > 31).
        // "hi": one 24c head, class budget 8 — provably unmintable
        //       (24 > budget 8 AND 24 > live free 0).
        // "lo": two 8c intents, class budget 64 — mintable.
        let intents = vec![
            intent("h0", 24, GI, &[("hi", CapacityType::Spot)]),
            intent("l0", 8, GI, &[("lo", CapacityType::Spot)]),
            intent("l1", 8, GI, &[("lo", CapacityType::Spot)]),
        ];
        let view = WindowMintability {
            fully_masked: HashSet::new(),
            class_budget: [("hi".to_string(), 8u32), ("lo".to_string(), 64u32)].into(),
            global_budget: 64,
            live_free_cores: 0,
            all_known_masked: false,
        };
        let sketches = CellSketches::default();
        // The MINT comparison: the admitted set through the
        // production assignment + sizing pipeline, each class at its
        // own budget — cores actually claimable this tick.
        let mint_cores = |set: &[SpawnIntent]| -> u32 {
            let none = HashSet::new();
            let known: HashSet<Cell> = [
                Cell("hi".into(), CapacityType::Spot),
                Cell("lo".into(), CapacityType::Spot),
            ]
            .into();
            let (by_cell, _) =
                cover::assign_to_cells(set, &sketches, &none, &known, cover::cell_rank, |_, _| {
                    None
                });
            by_cell
                .iter()
                .map(|(cell, u)| {
                    let budget = if cell.0 == "hi" { 8 } else { 64 };
                    cover::sizing(
                        cell,
                        u,
                        &cover::SizingCfg {
                            max_node_cores: 64,
                            max_node_mem: 256 * GI,
                            max_node_disk: 450 * GI,
                            budget,
                            fuse_cache_bytes: 0,
                        },
                    )
                    .claims
                    .iter()
                    .map(|c| c.0)
                    .sum::<u32>()
                })
                .sum()
        };
        let lo_cores = |set: &[SpawnIntent]| -> u32 {
            set.iter()
                .filter(|i| i.hw_class_names.first().map(String::as_str) == Some("lo"))
                .map(|i| i.cores)
                .sum()
        };
        // BOTH rotation phases (K = 2 buckets): the head-first tick
        // (tick 0 — BTreeMap key order ["hi", "lo"], offset 0) and
        // the sibling-first tick.
        for tick in 0..2 {
            let mut sorted = intents.clone();
            sort_ffd(&mut sorted);
            let (admitted, deferred) =
                admit_window(sorted, &HashMap::new(), &HashSet::new(), 31, tick, &view);
            assert_eq!(
                lo_cores(&admitted),
                16,
                "tick {tick}: mintable sibling cores admitted in full — an \
                 unmintable head consumed the rotation"
            );
            assert!(
                deferred.iter().any(|i| i.intent_id == "h0"),
                "tick {tick}: the unmintable head defers typed (re-seen when \
                 budget/mask/live capacity changes), never silently dropped"
            );
            assert_eq!(
                mint_cores(&admitted),
                16,
                "tick {tick}: the admitted set MINTS the sibling cores \
                 (assign+sizing at per-class budgets)"
            );
        }
        // The pinned pre-fix shape (the falsify twin — dies through
        // ITS conjunct: the permissive view has no mintability axis,
        // exactly the pre-fix window).
        let mut sorted = intents.clone();
        sort_ffd(&mut sorted);
        let (admitted, _) = admit_window(
            sorted,
            &HashMap::new(),
            &HashSet::new(),
            31,
            0,
            &WindowMintability::permissive(),
        );
        assert_eq!(
            lo_cores(&admitted),
            0,
            "negative control: under the pre-fix (permissive) window the \
             head's rotation-first tick starves the sibling bucket"
        );
        assert_eq!(
            mint_cores(&admitted),
            0,
            "negative control: the net-zero tick — zero cores minted \
             across ALL classes while 16 mintable sibling cores sat \
             deferred behind a provably-unmintable head (1/K frequency, \
             persistent under the cores-desc sort)"
        );
    }
}
