//! D4 reactive resource floor: per-dimension peak observation on every
//! non-success worker close, capped at `Ceilings`, with a hard 2.0×
//! headroom on the axis a corroborated resource-exhaustion signal
//! names and a soft 1.2× elsewhere.
//!
//! sh-041u: replaces the per-caller "recognise reason → mint
//! axis-witness → double last_intent" arms with ONE
//! [`observe_peaks`] called at the worker-report dispatch chokepoint.
//! Peaks are evidence on every close (OOM, DiskFull, timeout,
//! exit≠0, SIGTERM); the hard-event reason only BOOSTS headroom on
//! its own axis — it no longer GATES whether the other axes observe
//! at all. The retired per-axis mints (the now-retired witness-
//! constructors) each restated the corroboration law for ONE axis at
//! ONE call site; under that shape every new close path was a missed
//! mint by construction.

use std::time::Duration;

use crate::sla::solve::Ceilings;
use crate::state::DerivationState;

/// Hard cap on `floor.deadline_secs` (24h). Separate from `Ceilings`
/// (which has no time dimension) — a build that hasn't finished in a
/// day is a runaway regardless of pod shape.
pub(super) const DEADLINE_CAP_SECS: u32 = 86_400;

/// sh-041u: the mem-axis CEILING trust band — `peak_memory_bytes ≤
/// assigned_mem × TRUST_BAND_MEM`. The kernel guarantees `memory.peak
/// ≤ memory.max = assigned_mem`, so any honest report sits at ≤ 1.0×;
/// the 5% slack absorbs cgroup rounding. A peak above the band is a
/// forgery → that axis observes nothing (counted refusal). The disk
/// axis has no single band constant: its ceiling is the
/// kubelet-minted `overlay_size_limit_bytes(assigned, H_MAX) + slack`
/// — the same producer-derived band the now-retired sizing
/// hard-limit check used, REPLACED here as a check on
/// `final_resources.peak_disk_bytes` instead of
/// `QuotaTelemetry.hard_limit_bytes` (which is no longer consulted).
pub(super) const TRUST_BAND_MEM: f64 = 1.05;

/// sh-041u r1: the cores-axis CEILING trust band — `cpu_util =
/// cpu_seconds / (wall × assigned_cores) ≤ TRUST_BAND_CORES`. The
/// kernel guarantees `cpu.stat.usage_usec ≤ wall × cpu.max =
/// wall × assigned_cores`, so any honest report sits at ≤ 1.0×; the
/// 5% slack absorbs cgroup accounting jitter. A `cpu_util` above the
/// band is a forgery (or a non-finite `cpu_seconds`) → refused,
/// counted on `uncorroborated_sizing_claim_total{class=cores}`.
pub(super) const TRUST_BAND_CORES: f64 = 1.05;

// r[impl sys.liveness.exit-edge]
/// merged_bug_016: the mem dimension's cap in the SOLVE domain — the
/// largest solved mem whose padded container
/// (`rio_common::footprint::container_mem_bytes`) still fits under
/// the global ceiling. Every mem pin in this module (the floor
/// doubling cap, the hydrate/read-time floor grounding) targets THIS,
/// not the raw `ceil.max_mem`: a floor pinned at the raw ceiling
/// rendered a container of `ceiling + pad` that no class hosts, so
/// the at-cap retry attempts could never run and the designed bounded
/// poison terminal was unreachable (the dead-band funnel shape).
/// `unwrap_or(0)`: a global that cannot host any container (refused
/// by `validate_resolved`, kept fail-closed here) floors at zero —
/// `set_dim` then reports `at_cap` immediately and the caller's
/// retry counter bounds the loop.
pub(super) fn mem_solve_cap(ceil: &Ceilings) -> u64 {
    rio_common::footprint::max_hostable_solve_mem(ceil.max_mem).unwrap_or(0)
}

/// One floor dimension (sh-041u: the per-axis at-cap discriminator).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Mem,
    Disk,
    Deadline,
    Cores,
}

impl Axis {
    const fn bit(self) -> u8 {
        match self {
            Axis::Mem => 1 << 0,
            Axis::Disk => 1 << 1,
            Axis::Deadline => 1 << 2,
            Axis::Cores => 1 << 3,
        }
    }
}

/// Bitset of [`Axis`] values — the per-handler `at_cap` narrowing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AxisSet(u8);

impl AxisSet {
    fn insert(&mut self, a: Axis) {
        self.0 |= a.bit();
    }
    /// Whether `a` is in the set.
    pub fn contains(&self, a: Axis) -> bool {
        self.0 & a.bit() != 0
    }
    /// Whether any of `axes` is in the set.
    pub fn intersects(&self, axes: &[Axis]) -> bool {
        axes.iter().any(|a| self.contains(*a))
    }
    /// Whether the set is empty.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }
}

/// Result of [`observe_peaks`]. sh-041u r2: the persist/metric gate
/// and the kernel input are SEPARATE bits — overloading one bit for
/// both regressed the M_044 persist on a hard-event grow-to-cap clip
/// (floor 0→cap mutated in memory but `hard_promoted=false` skipped
/// the persist, so failover rehydrated floor=0 → re-OOM at probe
/// defaults). `hard_grew` gates persist + the bump metric/log;
/// `hard_promoted` is the kernel-disjoint promotion-exempt input.
/// `at_cap_axes` is per-axis so each handler narrows
/// `row.floor_at_cap` to the axes its kernel arm reads
/// (ExecutorVariant → Cores only; Timeout → Deadline; Infra →
/// Mem|Disk).
///
/// This helper does NOT mutate any retry counter. All counter
/// increments live at the call site, AFTER the cap check, so at-cap
/// and non-floor failures see the same `max_*_retries` bound (the
/// previous in-helper increment poisoned at-cap one attempt earlier).
#[derive(Debug, Default, Clone, Copy)]
pub struct FloorOutcome {
    /// At least one axis STRICTLY grew under 2.0× headroom (or cores
    /// jumped to `prov_max`) — regardless of whether the grow clipped
    /// at cap. Gates the M_044 persist and the
    /// `rio_scheduler_resource_floor_bumps_total` metric/log: a
    /// grow-to-cap clip MUST persist (the in-memory floor moved
    /// old→cap; the next dispatch IS larger).
    pub hard_grew: bool,
    /// At least one axis strictly grew under 2.0× headroom AND that
    /// axis is not at cap. Kernel input only
    /// (`r[sched.retry.promotion-exempt]` / `FloorOutcomeView.
    /// promoted`): preserved DISJOINT with `at_cap_axes` per-axis so
    /// the kernel's `decide()` fold invariant holds. `hard_promoted ⇒
    /// hard_grew`; the converse fails on a grow-to-cap clip.
    pub hard_promoted: bool,
    /// At least one axis grew under soft 1.2× headroom only.
    /// In-memory; never gates exemption or persist (a soft observe
    /// can be `target < last_intent` — granting exemption would be
    /// the retired I-170/I-199 over-broad heuristic).
    pub soft_promoted: bool,
    /// Per-axis: `target ≥ cap` (mem/disk/deadline via [`set_dim`])
    /// or `base ≥ prov_max` (cores via
    /// [`bump_cores_to_provisionable`] — preserving the two-attempt
    /// at-cap semantics). Consumers narrow per handler.
    pub at_cap_axes: AxisSet,
}

impl FloorOutcome {
    /// sh-041u r3: the ONE `(grew, at_cap) → {at_cap_axes, hard_grew,
    /// hard_promoted}` fold law for a hard-headroom axis. All four
    /// axes (mem/disk via [`observe_sizing_axis`], deadline, cores)
    /// route through this body so the kernel-disjointness invariant
    /// (`hard_promoted ⇒ !at_cap_axes∋axis` per axis) and the
    /// `hard_promoted ⇒ hard_grew` invariant are LEXICALLY singular —
    /// r2 open-coded the fold at three sites and missed the cores
    /// `grew` bit on the at-cap arm (the same persist-skip bug class
    /// the split was meant to close). r4: the input is the named
    /// [`SetDimOutcome`] so a producer-side `(at_cap, grew)` reorder —
    /// or a new axis helper that returns the swapped tuple by analogy
    /// — is a type error, not a silent re-open of the persist-skip.
    fn fold_hard(&mut self, axis: Axis, step: SetDimOutcome) {
        let SetDimOutcome { grew, at_cap } = step;
        if at_cap {
            self.at_cap_axes.insert(axis);
        }
        if grew {
            self.hard_grew = true;
            if !at_cap {
                self.hard_promoted = true;
            }
        }
    }
}

/// sh-041u r4: the chokepoint's input contract — what BOTH per-axis
/// producers ([`set_dim`], [`bump_cores_to_provisionable`]) return and
/// what [`FloorOutcome::fold_hard`] consumes. Named (not the r3
/// positional `(bool, bool)`) so the contract is carried in the type:
/// a positional bool-tuple is swap-silent, and a swap re-opens the
/// exact persist-skip class r3 closed with zero compiler signal.
#[derive(Debug, Clone, Copy)]
pub(super) struct SetDimOutcome {
    /// `*floor` strictly grew (`new > old`).
    pub grew: bool,
    /// `target ≥ cap` (mem/disk/deadline) or `base ≥ prov_max`
    /// (cores). NOT disjoint with `grew` — a grow-to-cap clip is
    /// `{grew: true, at_cap: true}`.
    pub at_cap: bool,
}

/// Worker-reported peaks for one closed attempt. `None` = no signal
/// (axis untouched). `wall` is the SCHEDULER's own
/// `running_since.elapsed()` (stamped at the Running transition; a
/// worker cannot mint it) — never a worker-supplied duration.
#[derive(Debug, Default, Clone, Copy)]
pub struct ObservedPeaks {
    /// `CompletionReport.peak_memory_bytes` (kernel `memory.peak`).
    pub mem_bytes: Option<u64>,
    /// `final_resources.peak_disk_bytes` (prjquota running-max).
    pub disk_bytes: Option<u64>,
    /// `final_resources.cpu_seconds_total` (cgroup cpu.stat).
    pub cpu_seconds: Option<f64>,
    /// `running_since.elapsed()` — the scheduler's own anchor.
    pub wall: Option<Duration>,
}

impl ObservedPeaks {
    /// Build from a `CompletionReport`'s fields. `0` / `None` =
    /// no-signal → axis untouched. `wall` is NOT a parameter: the
    /// scheduler-side anchor is derived inside
    /// `observe_resource_floor` from `running_since`, so a
    /// worker-supplied duration cannot reach the trust gate by
    /// construction.
    // r[impl sched.executor.input-bounds+2]
    pub fn from_report(
        peak_memory_bytes: u64,
        cpu_seconds_total: Option<f64>,
        peak_disk_bytes: Option<u64>,
    ) -> Self {
        Self {
            mem_bytes: (peak_memory_bytes > 0).then_some(peak_memory_bytes),
            disk_bytes: peak_disk_bytes.filter(|&d| d > 0),
            cpu_seconds: sanitize_positive_finite_secs(cpu_seconds_total),
            wall: None,
        }
    }

    // r[impl sched.floor.witnessed-peaks]
    /// sh-045 r1 — the witnessed-lane peak constructor (chokepoint
    /// #4). Owns the per-axis "which input do I trust on a witnessed
    /// close" policy so a future axis change edits ONE body beside
    /// [`AttemptCloseReason::axis_hard`], not the open-coded match
    /// arms r0 spread back into completion.rs.
    ///
    /// On the witnessed-HARD axis (mem when `OomKilled`, disk when
    /// `EvictedEmptyDirSizeLimit`): kubelet killed AT the limit, so
    /// `last.X` IS the authoritative peak. The heartbeat's
    /// parent-cgroup `memory.peak` includes ~100-300 MiB rio-builder
    /// overhead and would trip the [`TRUST_BAND_MEM`] ceiling →
    /// `count_refusal("mem")` → mem floor stays 0 → re-OOM at the same
    /// size (the r0 `max(last, hb)` regression that test
    /// `witnessed_oom_heartbeat_mem_overhead_still_hard_doubles`
    /// pins).
    ///
    /// On every OFF-axis: the heartbeat informs (soft 1.2× observe)
    /// but is clamped to the same trust ceiling `observe_sizing_axis`
    /// would refuse at, so parent-cgroup overhead cannot falsely
    /// increment the alert-on-nonzero
    /// `rio_scheduler_uncorroborated_sizing_claim_total` counter.
    ///
    /// `hb = None` (skew, sub-5s build, RPC-dropped) falls back to
    /// the witnessed-axis-only synthesis — the structurally honest
    /// "no off-axis evidence" that reproduces pre-sh-045 behaviour
    /// exactly.
    pub fn from_witnessed(
        last: &crate::state::SolvedIntent,
        hb: Option<&(f64, u64, rio_proto::types::ResourceUsage)>,
        reason: &AttemptCloseReason,
    ) -> Self {
        let mem_hard = reason.axis_hard(Axis::Mem);
        let disk_hard = reason.axis_hard(Axis::Disk);
        match hb {
            Some(&(wall_s, peak_mem, ref ru)) => {
                let mut p = Self::from_report(
                    if mem_hard {
                        last.mem_bytes
                    } else {
                        peak_mem.min(mem_trust_ceiling(last.mem_bytes))
                    },
                    ru.cpu_seconds_total,
                    // Disk: heartbeat `peak_disk_bytes` is the same
                    // prjquota running-max the worker-reported lane
                    // ships (no parent-cgroup overhead term), so the
                    // off-axis ceiling is the disk arm's own
                    // `overlay_size_limit_bytes(.., H_MAX) + slack` —
                    // a true peak never exceeds it; pass-through is
                    // chokepoint-#2-symmetric.
                    if disk_hard {
                        Some(last.disk_bytes)
                    } else {
                        ru.peak_disk_bytes
                    },
                );
                // The heartbeat's wall is sampled with `cpu_seconds`
                // (same instant). `observe_resource_floor` caps it at
                // the scheduler's `running_since` anchor.
                p.wall = sanitize_positive_finite_secs(Some(wall_s))
                    .map(rio_common::clamped::clamped_duration_secs);
                p
            }
            None => Self::from_report(
                if mem_hard { last.mem_bytes } else { 0 },
                None,
                disk_hard.then_some(last.disk_bytes),
            ),
        }
    }
}

// r[impl sched.executor.input-bounds+2]
/// sh-041u r1: proto3 `double` admits ±Inf/NaN and arbitrarily large
/// finite — bounded at intake so the cores arm never divides a
/// non-finite numerator and the wall-anchor never reaches
/// `clamped_duration_secs` non-finite. Shared by
/// [`ObservedPeaks::from_report`] (cpu), [`ObservedPeaks::from_witnessed`]
/// (wall), and the gRPC heartbeat intake (cpu + wall) so the predicates
/// cannot drift. sh-045 r2: `cpu_seconds = 0` from a RUNNING build is
/// degenerate (the sample raced cgroup creation) — the `> 0.0` gate
/// treats it as `None`, which the unification of the `from_report` and
/// intake filters made explicit (the pre-r1 intake filter admitted
/// `Some(0.0)`; `from_report` never did).
pub fn sanitize_positive_finite_secs(c: Option<f64>) -> Option<f64> {
    c.filter(|&c| c.is_finite() && c > 0.0)
}

/// sh-045 r2: the mem-axis trust ceiling — `assigned × TRUST_BAND_MEM`.
/// ONE body so [`ObservedPeaks::from_witnessed`]'s off-axis clamp
/// equals what [`observe_sizing_axis`] refuses at — they cannot drift
/// (the false-positive-on-`uncorroborated_sizing_claim_total` the
/// witnessed constructor exists to prevent).
fn mem_trust_ceiling(assigned: u64) -> u64 {
    (assigned as f64 * TRUST_BAND_MEM) as u64
}

// r[impl sched.floor.timeout-cores-suppressed-metric]
/// sh-045 r2 — the cores-arm utilization gate, returning ONE of four
/// distinguishable outcomes so every consumer (cores promotion,
/// `timeout_cores_suppressed`, `count_refusal("cores")`) matches on
/// the same derived value with NO recomputation.
#[derive(Debug, Clone, Copy)]
enum CpuUtilGate {
    /// `wall ≥ min_wall ∧ threshold ≤ cpu_util ≤ TRUST_BAND_CORES` —
    /// the input WOULD fire the cores promotion.
    Passed,
    /// `wall ≥ min_wall ∧ (cpu_util > TRUST_BAND_CORES ∨ NaN)` —
    /// forged-HIGH `cpu_seconds` (or non-finite). The counted
    /// `count_refusal("cores")` population.
    BandRefused,
    /// `wall ≥ min_wall ∧ cpu_util < threshold` — not compute-bound.
    BelowThreshold,
    /// `last_cores == 0 ∨ wall < min_wall` — preconditions unmet
    /// (trivially-short / cold-start).
    Ineligible,
}

/// See [`CpuUtilGate`]. The r0 emit-guard omitted the
/// [`TRUST_BAND_CORES`] ceiling and over-counted band-refused inputs;
/// the r1 `Option<f64>` collapsed three states into `None` so the
/// band-refusal counter recomputed `wall.as_secs_f64()`, `cpu_util`,
/// the `min_wall`/NaN/band checks. This tri-state is the ONE predicate.
fn cpu_util_gate(cpu: f64, wall: Duration, last_cores: u32, cfg: &ObserveCfg) -> CpuUtilGate {
    let wall_s = wall.as_secs_f64();
    if last_cores == 0 || wall_s < cfg.compute_bound_min_wall_secs {
        return CpuUtilGate::Ineligible;
    }
    let cpu_util = cpu / (wall_s * f64::from(last_cores));
    // sh-041u r2: a NaN that bypasses the `from_report` intake filter
    // is band-refused here as the [`TRUST_BAND_CORES`] doc claims —
    // explicit `is_nan()` so clippy's `neg_cmp_op_on_partial_ord`
    // autofix can't silently re-open the hole.
    if cpu_util.is_nan() || cpu_util > TRUST_BAND_CORES {
        CpuUtilGate::BandRefused
    } else if cpu_util >= cfg.compute_bound_threshold {
        CpuUtilGate::Passed
    } else {
        CpuUtilGate::BelowThreshold
    }
}

// r[impl sched.trust.report-corroboration+6]
/// sh-041u — the close reason `observe_peaks` dispatches headroom on.
/// SCHEDULER-MINTED (from `BuildResultStatus` × `FailureClass` × the
/// witnessed-letter — never free text), so the bug_102 demand ("trust
/// gated at the consequence") is preserved: the gate-inside-mutation
/// SHAPE survives; the witness-required-to-compile demand narrows to
/// the HARD arm — soft 1.2× is ceiling-band-checked only, with
/// consequence bounded by `hard_promoted` gating promotion-exempt and
/// `hard_grew` gating the M_044 persist. The variants below are the
/// ONLY producers — quantifier: census(floor_mutation_census).
#[derive(Debug, Clone, Copy)]
pub enum AttemptCloseReason {
    /// E2 worker `InfrastructureFailure` carrying a typed
    /// `FailureClass` (CgroupOom / DiskFull). `Unspecified` and
    /// uncarried classify as [`Self::Other`].
    Infra(rio_proto::types::FailureClass),
    /// E5 worker `TimedOut` (status-borne).
    Timeout,
    /// E3a worker `ExecutorVariantFailure` (heuristic exit≠0).
    ExecutorVariant,
    /// AD5 SIGTERM-abort report (preemption / scale-down) for work
    /// the scheduler still wants — the sh-041 original case (a
    /// compute-bound build interrupted by spot reclaim).
    WorkerAbort,
    /// Chokepoint #4: a controller-WITNESSED per-container kubelet
    /// attribution (`OomKilled` / `EvictedEmptyDirSizeLimit`).
    Witnessed(rio_proto::types::AttemptTerminalReason),
    /// Transient / Permanent / unsolicited-Cancelled / Unspecified —
    /// soft observe only (no axis is hard for these).
    Other,
}

impl AttemptCloseReason {
    /// Derive from a worker `CompletionReport` status. `None` for
    /// success statuses (Built/Substituted/AlreadyValid): the success
    /// path's `record_build_sample` writes `peak_memory_bytes` to
    /// `build_samples`, and the SLA fit's `p90 × headroom(n_eff)`
    /// (asymptote 1.25 > 1.2) dominates the soft floor at the solve
    /// chokepoint — a success-path soft observe is steady-state
    /// redundant, so the chokepoint skips it.
    pub(super) fn from_status(
        status: rio_proto::types::BuildResultStatus,
        classification: Option<&rio_proto::types::FailureClassification>,
    ) -> Option<Self> {
        use rio_proto::types::BuildResultStatus as S;
        use rio_proto::types::FailureClass;
        match status {
            S::Built | S::Substituted | S::AlreadyValid => None,
            S::InfrastructureFailure => {
                let class = classification
                    .map(|fc| FailureClass::try_from(fc.class).unwrap_or(FailureClass::Unspecified))
                    .filter(|&c| c != FailureClass::Unspecified);
                Some(match class {
                    Some(c) => Self::Infra(c),
                    None => Self::Other,
                })
            }
            S::TimedOut => Some(Self::Timeout),
            S::ExecutorVariantFailure => Some(Self::ExecutorVariant),
            S::TransientFailure
            | S::PermanentFailure
            | S::CachedFailure
            | S::DependencyFailed
            | S::LogLimitExceeded
            | S::OutputRejected
            | S::NotDeterministic
            | S::InputRejected
            | S::Cancelled
            | S::Unspecified => Some(Self::Other),
        }
    }

    // r[impl sched.floor.axis-trust]
    /// sh-045 — the per-axis HARD-headroom gate. Replaces
    /// `hard_{mem,disk,cores}()`: ONE `(reason, axis) → bool` match
    /// with **NO `_` arm**, so a new `AttemptCloseReason` variant (or
    /// new `FailureClass` / `AttemptTerminalReason` payload) cannot
    /// ship without positioning on every axis (rustc exhaustiveness —
    /// the §Nth-strike "Partition at type level" template). Soft 1.2×
    /// observe is NOT this chokepoint's concern — soft stays on
    /// sh-041u's `peaks.X.is_some()` → `observe_sizing_axis(..,
    /// hard_claim: bool, ..)` path; `axis_hard` decides hard-promotion
    /// only.
    ///
    /// **The cores-hard boundary is the I-170/I-199 line**: closes
    /// where the build was running and the close is NOT
    /// derivation-intrinsic (i.e., the same drv reruns at the same
    /// inputs) — `{ExecutorVariant, WorkerAbort, Infra(CgroupOom |
    /// DiskFull), Witnessed(OomKilled | EvictedEmptyDirSizeLimit)}`.
    /// `Other` is out (derivation-intrinsic by definition: a build
    /// that never reruns at the same inputs cannot benefit from a
    /// `prov_max` floor — the I-170/I-199 over-fire). `Timeout` is out
    /// by owner decision: `cpu_util` cannot discriminate
    /// serial-saturated from parallel-saturated, the cores arm jumps to
    /// `prov_max` so a wrong promotion costs `prov_max×` capacity, and
    /// the discriminator (`cpu.stat throttled_usec`) is being plumbed
    /// (transport-only) but not yet gating. Status quo pinned by
    /// `cores_hard_promote_gated_on_reason`; the
    /// `rio_scheduler_timeout_cores_suppressed_total` counter measures
    /// prevalence before any future revisit.
    ///
    /// The Witnessed/Infra cores=Hard widening is safe under the
    /// existing trust envelope: `wall` is scheduler-anchored
    /// (`running_since.elapsed()`), `cpu_util ≤ TRUST_BAND_CORES`
    /// ceiling-band-refuses forged-HIGH, `cpu_util ≥
    /// compute_bound_threshold ∧ wall ≥ min_wall` floor-gates
    /// trivially-short runs, the `won`-flag is the once-per-attempt
    /// cap, and the M_044 `GREATEST()` ratchet is unchanged.
    ///
    /// Mem/disk: unchanged from the retired `hard_{mem,disk}()` —
    /// `Infra(CgroupOom)` / `Witnessed(OomKilled)` are mem-hard;
    /// `Infra(DiskFull)` / `Witnessed(EvictedEmptyDirSizeLimit)` are
    /// disk-hard (live_057-b worker quota-attributed lane + sh-039
    /// controller-witnessed lane; node-condition `EvictedDiskPressure`
    /// stays classify-only by I-199 — [`witnessed_disposition`]
    /// returns `None` for it so it never reaches this predicate).
    /// Deadline: only `Timeout` is hard (the deadline ratchet's
    /// existing `matches!` body, now a row here).
    pub(super) fn axis_hard(&self, axis: Axis) -> bool {
        use AttemptCloseReason::{ExecutorVariant, Infra, Other, Timeout, Witnessed, WorkerAbort};
        use Axis::{Cores, Deadline, Disk, Mem};
        use rio_proto::types::{AttemptTerminalReason as T, FailureClass as F};
        match (self, axis) {
            // ── Infra(CgroupOom) — worker per-container OOM attribution
            (Infra(F::CgroupOom), Mem) => true,
            (Infra(F::CgroupOom), Disk) => false,
            (Infra(F::CgroupOom), Deadline) => false,
            (Infra(F::CgroupOom), Cores) => true,
            // ── Infra(DiskFull) — worker per-container quota attribution
            (Infra(F::DiskFull), Mem) => false,
            (Infra(F::DiskFull), Disk) => true,
            (Infra(F::DiskFull), Deadline) => false,
            (Infra(F::DiskFull), Cores) => true,
            // ── Infra(Unspecified) — UNREACHABLE BY CONSTRUCTION:
            //    [`Self::from_status`] (the SOLE production constructor
            //    of `Infra(..)` — quantifier:
            //    census(floor_mutation_census)) filters
            //    `FailureClass::Unspecified` to `Self::Other` before
            //    `Infra(..)` is minted. The retired `matches!`-based
            //    `hard_{mem,disk}()` returned `false` here; the
            //    `unreachable!()` is DELIBERATE so a future direct
            //    `Infra(fc.class())` from wire (proto3 default `class
            //    = 0`) panics LOUDLY at the chokepoint instead of
            //    silently soft-observing. To harden structurally,
            //    narrow `Infra`'s payload to a non-`Unspecified`
            //    newtype.
            (Infra(F::Unspecified), Mem | Disk | Deadline | Cores) => {
                unreachable!("from_status routes Unspecified → Self::Other")
            }
            // ── Timeout — worker self-kill at scheduler deadline (E5)
            (Timeout, Mem) => false,
            (Timeout, Disk) => false,
            (Timeout, Deadline) => true,
            (Timeout, Cores) => false,
            // ── ExecutorVariant — heuristic exit≠0 (sh-031)
            (ExecutorVariant, Mem) => false,
            (ExecutorVariant, Disk) => false,
            (ExecutorVariant, Deadline) => false,
            (ExecutorVariant, Cores) => true,
            // ── WorkerAbort — SIGTERM for still-wanted work (sh-041)
            (WorkerAbort, Mem) => false,
            (WorkerAbort, Disk) => false,
            (WorkerAbort, Deadline) => false,
            (WorkerAbort, Cores) => true,
            // ── Witnessed(OomKilled) — kubelet per-container OOM
            (Witnessed(T::OomKilled), Mem) => true,
            (Witnessed(T::OomKilled), Disk) => false,
            (Witnessed(T::OomKilled), Deadline) => false,
            (Witnessed(T::OomKilled), Cores) => true,
            // ── Witnessed(EvictedEmptyDirSizeLimit) — kubelet per-pod
            //    emptyDir attribution (sh-039)
            (Witnessed(T::EvictedEmptyDirSizeLimit), Mem) => false,
            (Witnessed(T::EvictedEmptyDirSizeLimit), Disk) => true,
            (Witnessed(T::EvictedEmptyDirSizeLimit), Deadline) => false,
            (Witnessed(T::EvictedEmptyDirSizeLimit), Cores) => true,
            // ── Witnessed(other) — UNREACHABLE BY CONSTRUCTION:
            //    [`witnessed_disposition`] is the SOLE producer of
            //    `Witnessed(..)` (gated by `observe_witnessed_floor`;
            //    quantifier: census(witnessed_disposition_product_census)),
            //    and it returns `None` for every letter below. A
            //    future flip there (e.g. `EvictedDiskPressure` →
            //    `Some`) compiles cleanly THEN panics here on first
            //    production close — that panic is the DESIRED forcing
            //    function (the row must move out of this bundle and
            //    take an explicit per-axis position). To make the
            //    coupling structural instead of convention-only, mint
            //    a 2-variant `WitnessedPromoted` enum at
            //    `witnessed_disposition` so this bundle becomes
            //    type-empty.
            (
                Witnessed(
                    T::Unspecified
                    | T::EvictedDiskPressure
                    | T::EvictedOther
                    | T::Completed
                    | T::Error
                    | T::DeadlineExceeded
                    | T::Cancelled
                    | T::Preempted
                    | T::Reaped
                    | T::NoEligibleSource,
                ),
                Mem | Disk | Deadline | Cores,
            ) => unreachable!("witnessed_disposition returns None for these"),
            // ── Other — derivation-intrinsic (E3b PermanentFailure /
            //    NotDeterministic / InputRejected / LogLimitExceeded)
            (Other, Mem) => false,
            (Other, Disk) => false,
            (Other, Deadline) => false,
            (Other, Cores) => false,
        }
    }

    /// The metric/log label — the caller-census alphabet, derived
    /// from the reason instead of restated per call site.
    pub(super) fn label(&self) -> &'static str {
        use rio_proto::types::{AttemptTerminalReason as T, FailureClass};
        match self {
            Self::Infra(FailureClass::CgroupOom) => "cgroup_oom",
            Self::Infra(FailureClass::DiskFull) => "disk_full",
            Self::Infra(FailureClass::Unspecified) => "unspecified",
            Self::Timeout => "timeout",
            Self::ExecutorVariant => "executor_variant",
            Self::WorkerAbort => "worker_abort",
            Self::Witnessed(T::OomKilled) => "witnessed_oom",
            Self::Witnessed(T::EvictedEmptyDirSizeLimit) => "witnessed_disk",
            Self::Witnessed(_) => "witnessed_other",
            Self::Other => "other",
        }
    }
}

/// Tunables for [`observe_peaks`]' cores-axis gate (the same fields
/// `SlaConfig` already carries).
#[derive(Debug, Clone, Copy)]
pub struct ObserveCfg {
    pub compute_bound_threshold: f64,
    pub compute_bound_min_wall_secs: f64,
}

impl From<&crate::sla::config::SlaConfig> for ObserveCfg {
    fn from(c: &crate::sla::config::SlaConfig) -> Self {
        Self {
            compute_bound_threshold: c.compute_bound_threshold,
            compute_bound_min_wall_secs: c.compute_bound_min_wall_secs,
        }
    }
}

// r[impl sched.sla.reactive-floor+8]
// r[impl sched.retry.promotion-exempt+4]
/// sh-041u — the unified peak observe. For each axis with a `Some`
/// peak: `floor.X = max(floor.X, peak_X × headroom(reason, X))
/// .min(cap_X)`. `headroom = 2.0` iff a corroborated hard event fired
/// on that axis (the floor band `peak ≥ assigned/2` from bug_090
/// gates whether the hard CLAIM is plausible — fail → degrade to soft
/// 1.2×, no `hard_promoted`, counted refusal); else `1.2` (mem/disk),
/// `1.0` (deadline — no soft headroom; only `Timeout` ever moves it).
/// The cores axis jumps to `prov_max` iff `cpu_util ≥ threshold ∧
/// wall ≥ min_wall` (sh-031). The CEILING band (`peak ≤ assigned ×
/// band` per axis) refuses forged-HIGH peaks: that axis observes
/// nothing, counted on `uncorroborated_sizing_claim_total`.
///
/// The doubling base is the PEAK, not `last_intent` — but for an
/// honest hard event `peak ≈ assigned` (memory.peak saturates at
/// memory.max under OOM; prjquota peak saturates at the limit under
/// DiskFull; wall ≈ assigned_deadline at timeout), so `peak × 2.0 ≈
/// assigned × 2` reproduces the old "cold-start: floor=0, last=4Gi,
/// real OOM → 8Gi" doubling without re-introducing `last_intent` as a
/// separate base (the bug_090 floor band `peak ≥ assigned/2` ⇒
/// `peak × 2.0 ≥ assigned`). bug_027's reconciled `last_intent`
/// remains the corroboration ANCHOR (the band denominators), not the
/// doubling input.
pub fn observe_peaks(
    state: &mut DerivationState,
    peaks: ObservedPeaks,
    reason: AttemptCloseReason,
    ceil: &Ceilings,
    prov_max_cores: u32,
    cfg: &ObserveCfg,
) -> FloorOutcome {
    use rio_common::k8s::{
        DISK_HEADROOM_MAX, DISK_HEADROOM_MIN, KUBELET_QUOTA_BLOCK_SLACK, overlay_size_limit_bytes,
    };
    // sh-041u r1: copy the four Copy scalars instead of cloning the
    // whole `SolvedIntent` (Vec<NodeSelectorTerm>, Vec<String>, …) on
    // every non-success close — the clone existed solely to dodge a
    // borrowck overlap with `&mut floor` below.
    let (last_mem, last_disk, last_cores, last_deadline) = match state.sched.last_intent.as_ref() {
        Some(l) => (l.mem_bytes, l.disk_bytes, l.cores, l.deadline_secs),
        // Cold start (never minted by this leader): no anchor to
        // corroborate against. The `set_dim` body would no-op on a
        // zero base anyway; the caller's retry budget bounds it.
        None => return FloorOutcome::default(),
    };
    let floor = &mut state.sched.resource_floor;
    let mut o = FloorOutcome::default();

    // ── mem ────────────────────────────────────────────────────────
    // CEILING band: kernel guarantees memory.peak ≤ memory.max
    // = assigned_mem. A peak above TRUST_BAND_MEM × assigned is a
    // forgery — refuse, count, axis observes nothing. The bug_102
    // forgery resistance: per-attempt floor growth is bounded by
    // ≤ 2 × TRUST_BAND_MEM × assigned, equivalent to the retired
    // `2 × last_intent`. FLOOR band: a hard-event CLAIM with low
    // peak is implausible (bug_090) — degrade to soft.
    if let Some(peak) = peaks.mem_bytes
        && last_mem > 0
    {
        observe_sizing_axis(
            &mut floor.mem_bytes,
            &mut o,
            Axis::Mem,
            peak,
            mem_trust_ceiling(last_mem),
            last_mem / 2,
            reason.axis_hard(Axis::Mem),
            mem_solve_cap(ceil),
            ("mem", "cgroup_oom"),
        );
    }

    // ── disk ───────────────────────────────────────────────────────
    // sh-041u (round-2): the disk-axis bands REPLACE the retired
    // `QuotaTelemetry.hard_limit_bytes ∈ [overlay(assigned,
    // H_MIN), overlay(assigned, H_MAX) + slack]` check — that
    // tested the worker-reported hard limit; this tests the
    // worker-reported PEAK against the same producer-derived
    // denomination (`overlay_size_limit_bytes` — the kubelet-
    // stamped sizeLimit a real prjquota peak saturates at). The
    // ceiling is vs the H_MAX-expanded effective assigned (a
    // legitimate DiskFull peak can reach overlay(assigned, 1.95)
    // ≈ 1.95×assigned), not the raw `last.disk_bytes`.
    if let Some(peak) = peaks.disk_bytes
        && last_disk > 0
    {
        let eff_min = overlay_size_limit_bytes(last_disk, DISK_HEADROOM_MIN);
        let eff_max = overlay_size_limit_bytes(last_disk, DISK_HEADROOM_MAX)
            .saturating_add(KUBELET_QUOTA_BLOCK_SLACK);
        observe_sizing_axis(
            &mut floor.disk_bytes,
            &mut o,
            Axis::Disk,
            peak,
            eff_max,
            eff_min / 2,
            reason.axis_hard(Axis::Disk),
            ceil.max_disk,
            ("disk", "disk_full"),
        );
    }

    // ── deadline ───────────────────────────────────────────────────
    // No soft headroom: only a corroborated `Timeout` ever moves it.
    // sh-045 r1: gated on the chokepoint's own `axis_hard(Deadline)`
    // row (extensionally `== matches!(Timeout)` today; the rewire
    // makes the Deadline column live so a future row added there
    // actually reaches this arm). The anchor is the scheduler's own
    // `running_since` elapsed (stamped at the Running transition; a
    // worker cannot mint it), vs the reconciled
    // `last_intent.deadline_secs` (bug_027: `max(resolved, carried)`
    // — a carried deadline at the cap reads as `wall ≥ cap` and takes
    // the counted at-cap arm).
    let mut deadline_promoted = false;
    if reason.axis_hard(Axis::Deadline) {
        match (peaks.wall, last_deadline) {
            (Some(wall), assigned) if assigned > 0 => {
                let wall_s = wall.as_secs();
                if wall_s >= u64::from(assigned) / 2 {
                    let target = wall_s.saturating_mul(2);
                    let mut f = u64::from(floor.deadline_secs);
                    let step = set_dim(&mut f, target, u64::from(DEADLINE_CAP_SECS));
                    floor.deadline_secs = f as u32;
                    o.fold_hard(Axis::Deadline, step);
                    deadline_promoted = true;
                } else {
                    count_refusal("timed_out");
                }
            }
            _ => count_refusal("timed_out"),
        }
    }

    // ── cores (sh-012, sh-031, sh-041, sh-045) ─────────────────────
    // r[impl sched.floor.compute-bound-provisionable]
    // `cpu_util = cpu_seconds / (wall × assigned_cores) ≥ threshold`
    // jumps cores to the partition-aware provisionable max. Gated on
    // `axis_hard(Cores)` — the not-derivation-intrinsic closes
    // (sh-041: spot-reclaim `WorkerAbort`; sh-031: `ExecutorVariant`
    // self-timeout; sh-045: `Infra(CgroupOom|DiskFull)` worker-reported
    // and `Witnessed(OomKilled|EvictedEmptyDirSizeLimit)` heartbeat-
    // sourced). `min_wall` guards trivially-short saturated runs (the
    // inverse-cost bound); `TRUST_BAND_CORES` refuses forged-HIGH
    // `cpu_seconds`.
    if let (Some(cpu), Some(wall)) = (peaks.cpu_seconds, peaks.wall) {
        match cpu_util_gate(cpu, wall, last_cores, cfg) {
            CpuUtilGate::Passed if reason.axis_hard(Axis::Cores) => {
                let step =
                    bump_cores_to_provisionable(&mut floor.cores, last_cores, prov_max_cores);
                o.fold_hard(Axis::Cores, step);
            }
            // r[impl sched.floor.timeout-cores-suppressed-metric]
            // sh-045 r2: count CPU-saturated Timeouts where the
            // cores-arm gate WOULD have fired had `(Timeout, Cores)`
            // been hard AND the deadline arm DID promote (r0's nesting
            // — `last_deadline > 0 ∧ wall ≥ assigned/2` — restored;
            // r1 widened to deadline-refused Timeouts and made any
            // recorded baseline non-comparable). ONE predicate
            // ([`cpu_util_gate`]), two consumers — the metric counts
            // exactly the population it claims to mirror. A non-trivial
            // rate is the trigger to revisit `(Timeout, Cores)` once
            // `cpu_throttled_usec` is gating; near-zero confirms the
            // status quo.
            CpuUtilGate::Passed if deadline_promoted => {
                metrics::counter!("rio_scheduler_timeout_cores_suppressed_total").increment(1);
            }
            // The band-refused counted refusal — only on cores-hard
            // reasons (the lane that would otherwise have promoted).
            // No recomputation: same [`cpu_util_gate`] derivation.
            CpuUtilGate::BandRefused if reason.axis_hard(Axis::Cores) => {
                count_refusal("cores");
            }
            CpuUtilGate::Passed
            | CpuUtilGate::BandRefused
            | CpuUtilGate::BelowThreshold
            | CpuUtilGate::Ineligible => {}
        }
    }

    o
}

/// sh-041u r1: the ONE mem/disk band law — `ceiling` band refuses
/// forged-HIGH (`count_refusal(hi)`, axis observes nothing); else a
/// `hard_claim` with `peak < floor_band` degrades to soft
/// (`count_refusal(lo)`); `target = peak × {2.0|1.2}` feeds
/// [`set_dim`]; hard outcomes route through [`FloorOutcome::fold_hard`]
/// (the chokepoint for the `at_cap`/`grew` law). Extracted so a
/// band-law tweak (soft headroom, the fold law) edits one body, not
/// two — the mem and disk arms were byte-identical modulo their bands.
#[allow(clippy::too_many_arguments)]
fn observe_sizing_axis(
    floor: &mut u64,
    o: &mut FloorOutcome,
    axis: Axis,
    peak: u64,
    ceiling: u64,
    floor_band: u64,
    hard_claim: bool,
    cap: u64,
    (refusal_hi, refusal_lo): (&'static str, &'static str),
) {
    if peak > ceiling {
        count_refusal(refusal_hi);
        return;
    }
    let hard = hard_claim && peak >= floor_band;
    if hard_claim && !hard {
        count_refusal(refusal_lo);
    }
    let target = (peak as f64 * if hard { 2.0 } else { 1.2 }) as u64;
    let step = set_dim(floor, target, cap);
    if hard {
        o.fold_hard(axis, step);
    } else {
        if step.at_cap {
            o.at_cap_axes.insert(axis);
        }
        // sh-041u r3: `!at_cap` preserves the baseline disjoint
        // semantics — `set_dim`'s slot-0 widening to non-disjoint
        // `grew` (r2) would otherwise newly fire `soft_promoted` (and
        // the `headroom="soft"` bump metric) on a soft grow-to-cap.
        // r4: that leaves the in-memory `*floor = cap` mutation
        // unlogged/uncounted by design — symmetric with the hard
        // path's at_cap-not-promoted (kernel input stays disjoint;
        // `soft_promoted` gates neither persist nor exemption, so the
        // observability cost is the only effect, and the
        // `headroom="soft"` series is documented as in-band only).
        if step.grew && !step.at_cap {
            o.soft_promoted = true;
        }
    }
}

fn count_refusal(class: &'static str) {
    metrics::counter!(
        "rio_scheduler_uncorroborated_sizing_claim_total",
        "class" => class
    )
    .increment(1);
}

// r[impl sched.attempt.witnessed-terminal+3]
/// live_058-b: the per-reason disposition table — controller-WITNESSED
/// terminal letter → `Some(AttemptCloseReason::Witnessed(..))` for the
/// TWO per-container kubelet attributions (OomKilled, sh-039's
/// EvictedEmptyDirSizeLimit), `None` (classify-only) for every other
/// letter. Consumed by the establishment sweep's charge arm: the
/// dispatch is over the producer's FULL wire type, so a new letter
/// cannot ship without taking a position here (rustc exhaustiveness;
/// the product census pins exactly TWO `Some` rows, and the review
/// default for a new row is `None`). Node-condition
/// `EvictedDiskPressure` stays `None` by ruling (I-199 — promoting it
/// would re-create the ambient over-fire: one node-pressure event
/// evicting k pods → k sticky M_044 floor doublings).
pub(super) fn witnessed_disposition(
    reason: rio_proto::types::AttemptTerminalReason,
) -> Option<AttemptCloseReason> {
    use rio_proto::types::AttemptTerminalReason as R;
    match reason {
        // Promoting row 1/2: per-container kubelet attribution — the
        // pod hit ITS memory limit; nothing ambient about it.
        R::OomKilled => Some(AttemptCloseReason::Witnessed(reason)),
        // sh-039 — promoting row 2/2: kubelet's POD-ATTRIBUTED
        // emptyDir-sizeLimit / ephemeral local storage eviction (the
        // three POD_ATTRIBUTED_NEEDLES grammars). Kubelet's own
        // per-pod statement that THIS build exceeded ITS declared
        // disk; the limit it names IS the scheduler-stamped sizeLimit
        // (overlay_size_limit_bytes(last_intent.disk_bytes, h)), so
        // the witnessed letter carries the same per-container
        // authority as OomKilled.
        R::EvictedEmptyDirSizeLimit => Some(AttemptCloseReason::Witnessed(reason)),
        // Classify-only BY RULING (I-199), UNTOUCHED at sh-039:
        // node-condition shapes only (pod-attributed split to the row
        // above at the controller producer). Classify-only.
        R::EvictedDiskPressure => None,
        // Wire-default / unclassifiable; ambient by definition;
        // expected one-shot exit; pod death not the build's fault;
        // Job-level kill (no per-container attribution);
        // controller-synthesized verdicts; platform disruption;
        // controller reap; spawn-gate verdict.
        R::Unspecified
        | R::EvictedOther
        | R::Completed
        | R::Error
        | R::DeadlineExceeded
        | R::Cancelled
        | R::Preempted
        | R::Reaped
        | R::NoEligibleSource => None,
    }
}

/// Every wire letter exactly once — the product census's iteration
/// domain, pinned by an exhaustive index match in the census test so
/// a new variant cannot ship without joining this set AND taking a
/// disposition row above (the `GcPhase3Outcome::ALL` form).
#[cfg(test)]
pub(super) const WITNESSED_LETTERS: [rio_proto::types::AttemptTerminalReason; 12] = {
    use rio_proto::types::AttemptTerminalReason as R;
    [
        R::Unspecified,
        R::OomKilled,
        R::EvictedDiskPressure,
        R::EvictedOther,
        R::Completed,
        R::Error,
        R::DeadlineExceeded,
        R::Cancelled,
        R::Preempted,
        R::Reaped,
        R::NoEligibleSource,
        R::EvictedEmptyDirSizeLimit,
    ]
};

// r[impl scheduler.sla.ceiling.stale-solve-revalidation+2]
/// live_051(d): the read-time projection of a resource floor under the
/// LIVE ceiling vector — the floor-axis half of the stale-solve
/// revalidation law. A persisted M_044 floor is evidence minted UNDER
/// a ceiling vector: `set_dim` caps at the boot-resolved global and
/// the at-cap arm catches the floor UP to that cap for the persist, so
/// a floor bumped under a phantom global persists AT it and re-hydrates
/// across boots with no re-clamp anywhere — it then (i) pins every
/// dispatch at the stale value and (ii) re-raises mem PAST the live
/// global at the bypass seam, feeding the silent empty-cells channel.
/// *A floor above the live global is stale evidence*: every consumer
/// takes this projection (each dimension `min`'d at its live ceiling;
/// deadline at [`DEADLINE_CAP_SECS`], its only cap — `Ceilings` has no
/// time dimension), so boots, reloads, and mid-run shrinks are all
/// covered at the next read.
///
/// CLAMP-DOWN, not invalidate-and-resolve: the floor's honest
/// evidentiary content is "this pname exhausted every size up to X
/// under the OLD ceiling"; under a smaller live ceiling that content
/// saturates at the expressible maximum (the live cap), which the
/// projection preserves while invalidate-and-resolve would destroy it
/// (the next dispatch would re-walk the OOM ladder from the fitted
/// estimate, re-paying every OOM the floor already witnessed). The
/// DURABLE row is intentionally NOT healed downward: the M_044 writer
/// is a per-dimension `GREATEST()` ratchet (`update_resource_floor`,
/// db/derivations.rs — a downward write is structurally a no-op
/// there), and preserving the row means a later ceiling RE-GROWTH
/// re-admits the witnessed evidence instead of re-paying the ladder.
/// The projection makes the stale row unreachable as dispatch input
/// either way (consumption-total: hydrate + every read site).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ClampedFloor {
    pub(super) mem_bytes: u64,
    pub(super) disk_bytes: u64,
    pub(super) cores: u32,
}

impl ClampedFloor {
    /// Project `floor` under the live `ceil` — the ONLY constructor,
    /// so a consumer holding a `ClampedFloor` holds clamped values by
    /// type (the read-consume sites take this projection, never the
    /// raw floor; the census in sla_contract.rs pins the membership).
    ///
    /// sh-031b: `prov_max_cores` is the partition-aware provisionable
    /// cores cap ([`crate::sla::config::SlaConfig::
    /// provisionable_max_cores`] — feature/arch-routed AND
    /// non-ICE-exhausted). The cores projection grounds there, not at
    /// the catalog-absolute `ceil.max_cores`: a stale floor above the
    /// drv's own routable max emptied `h_all_filter`'s candidate set
    /// (the floor feeds `eff_min`) and the drv requeued forever.
    /// `prov_max_cores` is time-varying (ICE masks open/close) and
    /// read at consumption time, so a mask re-opening between bump and
    /// solve re-admits the larger class. mem/disk stay at the
    /// catalog-absolute `Ceilings` (memory/disk ICE-masking is
    /// class-orthogonal).
    pub(super) fn of(
        floor: &crate::state::ResourceFloor,
        ceil: &Ceilings,
        prov_max_cores: u32,
    ) -> Self {
        Self {
            // merged_bug_016: the mem floor grounds at the SOLVE-domain
            // cap (`mem_solve_cap`) — a floor at the raw global renders
            // an unhostable `ceiling + pad` container downstream.
            mem_bytes: floor.mem_bytes.min(mem_solve_cap(ceil)),
            disk_bytes: floor.disk_bytes.min(ceil.max_disk),
            cores: floor.cores.min(prov_max_cores),
        }
    }
}

// r[impl scheduler.sla.ceiling.stale-solve-revalidation+2]
/// The hydrate-seam half of the floor clamp law: clamp a floor IN
/// PLACE to the live ceilings (mem/disk) and the deadline cap. Applied
/// where DB floors meet in-memory state (the I-208 hydrate seam,
/// merge.rs) so a row persisted under a larger old global enters
/// memory already grounded. The recovery constructor
/// (`from_recovery_row`) loads raw persisted values — those are
/// covered by the read-time [`ClampedFloor`] projection at every
/// consumption site, so the law is total over both boot paths.
pub(super) fn clamp_floor_to_live(f: &mut crate::state::ResourceFloor, ceil: &Ceilings) {
    // merged_bug_016: mem grounds at the solve-domain cap, mirroring
    // `ClampedFloor::of` (the two halves of the clamp law must agree
    // on the mem pin or the hydrate seam re-opens the band).
    f.mem_bytes = f.mem_bytes.min(mem_solve_cap(ceil));
    f.disk_bytes = f.disk_bytes.min(ceil.max_disk);
    f.deadline_secs = f.deadline_secs.min(DEADLINE_CAP_SECS);
    // sh-031b: the hydrate seam keeps the COARSE catalog-absolute
    // cores cap (`ceil.max_cores`). The partition-aware provisionable
    // cap is per-drv (feat/arch) AND time-varying (ICE), so it
    // belongs at the read-time [`ClampedFloor::of`] projection — every
    // dispatch consumer reads through that, so the law is total. A
    // hydrate-time partition clamp would freeze a transient ICE mask
    // into the in-memory floor.
    f.cores = f.cores.min(ceil.max_cores as u32);
}

/// sh-041u: the canonical per-dimension body — `floor =
/// max(floor, target).min(cap)`. The ×2 doubling lives in `headroom`
/// alone (the retired per-axis ×2 body is REPLACED, not reused).
/// Returns `(grew, at_cap)`: `grew` iff the floor STRICTLY grew
/// (`next > old`), regardless of whether the grow clipped at cap;
/// `at_cap` iff `target ≥ cap` (a hard-event peak at the dispatched
/// ceiling reads as at-cap on the same observation, not the next
/// one). These are NOT disjoint — a grow-to-cap clip (`old < cap ≤
/// target`) sets BOTH: the immediate retry IS at a strictly larger
/// shape (old → cap), so `grew=true` must gate the M_044 persist and
/// the bump metric (sh-041u r2: the disjoint form regressed both —
/// floor mutated in memory but persist/metric skipped, so failover
/// rehydrated floor=0 → re-OOM at probe defaults).
///
/// The kernel's `FloorOutcomeView` disjointness invariant (the
/// `decide()` fold's `ControllerTermination` arm /
/// `decide_requeue_within_caps` predicate) is enforced at the
/// [`FloorOutcome::fold_hard`] chokepoint — `hard_promoted` is set
/// only when `grew && !at_cap` per axis — NOT here.
///
/// merged_bug_016: callers pass the SOLVE-domain mem cap
/// ([`mem_solve_cap`]) so an at-cap floor renders a hostable
/// container; the disk cap is the raw `ceil.max_disk`.
fn set_dim(floor: &mut u64, target: u64, cap: u64) -> SetDimOutcome {
    let old = *floor;
    *floor = old.max(target).min(cap);
    SetDimOutcome {
        grew: *floor > old,
        at_cap: target >= cap,
    }
}

// r[impl sched.floor.compute-bound-provisionable]
/// sh-031b: the cores-axis bump — JUMP straight to `prov_max` instead
/// of [`set_dim`]'s peak-based body. ComputeBound (a corroborated
/// `cpu_util ≥ threshold` while running) has no threshold semantics:
/// the build saturated its assigned cores, so the only useful next
/// probe is the largest provisionable shape. If THAT saturates too,
/// `at_cap` poisons `ComputeBoundAtCap` — there is nothing larger to
/// try and "shrink the regime" is the only remediation.
///
/// Returns the same [`SetDimOutcome`] shape as [`set_dim`] (sh-041u
/// r3: the divergent `CoresOutcome{promoted}` shape missed `grew` on
/// the at-cap arm, so a `floor=0, last=prov_max` initial dispatch
/// strictly grew `cores` 0→prov_max in memory but skipped the M_044
/// persist — the same bug class r2 closed for mem/disk/deadline).
/// `grew = prov_max > old` (strict in-memory grow); `at_cap = base ≥
/// prov_max` where `base = max(floor, last)` tests the DISPATCHED
/// shape (live_040 — the retired per-axis body's at_cap derivation),
/// so a stale floor or a clamped-at-cap `last_intent` reads as
/// already-at-cap. This is the cores axis's `at_cap` derivation — NOT
/// [`set_dim`]'s `target ≥ cap` (which would mark at_cap on the FIRST
/// jump and break the two-attempt semantics). The at-cap heal-down
/// (`old > prov_max → floor = prov_max`) is `grew=false`: the M_044
/// persist is a GREATEST ratchet so the row stays high; the read-time
/// [`ClampedFloor::of`] projection covers the durable row.
///
/// `prov_max == 0` (the routed partition is empty or fully
/// ICE-exhausted) is NOT compute-bound-at-cap — it's "nothing
/// provisionable at all", which the existing unschedulable / fleet
/// disclosure handles. Return the no-op so the caller's generic
/// budget bounds it without a misleading `ComputeBoundAtCap`
/// diagnostic naming `0c`.
fn bump_cores_to_provisionable(floor: &mut u32, last: u32, prov_max: u32) -> SetDimOutcome {
    if prov_max == 0 {
        return SetDimOutcome {
            grew: false,
            at_cap: false,
        };
    }
    let old = *floor;
    let at_cap = old.max(last) >= prov_max;
    *floor = prov_max;
    SetDimOutcome {
        grew: prov_max > old,
        at_cap,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bug_065 (R33'(ii)): the corroboration band's headroom bounds
    /// are the SHARED consts, and the live scheduler curve plus the
    /// controller fallback must stay inside them — this pin is the
    /// static cadence-of-denomination witness coupling the band to
    /// its producer. `headroom(n_eff) = 1.25 + 0.7/sqrt(n_eff)` on
    /// the clamped domain `n_eff >= 1` is monotone decreasing:
    /// max at 1 (== DISK_HEADROOM_MAX exactly), infimum 1.25
    /// (== DISK_HEADROOM_MIN, open). If S4's evidence work re-shapes
    /// the curve past either bound, this test names the coupling
    /// instead of letting genuine exhaustions read as forgeries.
    #[test]
    fn headroom_curve_stays_inside_the_shared_band() {
        use crate::sla::fit::headroom;
        use crate::sla::types::RingNEff;
        use rio_common::k8s::{DISK_HEADROOM_MAX, DISK_HEADROOM_MIN};
        // The exact endpoints.
        assert!((headroom(RingNEff(1.0)) - DISK_HEADROOM_MAX).abs() < 1e-12);
        assert!(headroom(RingNEff(1e18)) > DISK_HEADROOM_MIN);
        // The whole sampled domain, incl. the sub-1 clamp.
        for n in [0.1, 1.0, 2.0, 4.0, 9.0, 25.0, 100.0, 1e6, 1e12] {
            let h = headroom(RingNEff(n));
            assert!(
                h > DISK_HEADROOM_MIN && h <= DISK_HEADROOM_MAX,
                "headroom({n}) = {h} escaped the shared band"
            );
        }
    }

    // The controller's flat fallback (1.5) sits inside the band —
    // mirrored compile-time controller-side
    // (OVERLAY_HEADROOM_FALLBACK's const assert); carried here so the
    // scheduler's own tree fails to build if the shared band ever
    // excludes it.
    const _: () = assert!(
        rio_common::k8s::DISK_HEADROOM_MIN <= 1.5 && 1.5 <= rio_common::k8s::DISK_HEADROOM_MAX
    );

    const CEIL: Ceilings = Ceilings {
        max_cores: 64.0,
        max_mem: 256 << 30,
        max_disk: 200 << 30,
        default_disk: 20 << 30,
    };
    /// sh-031b: the cores axis caps at the partition-aware
    /// provisionable max, threaded as the 4th arg. The mem/disk/
    /// deadline arms ignore it; for those tests this is just
    /// `CEIL.max_cores` (no feat/arch/ICE context at the unit level).
    const PROV_MAX: u32 = CEIL.max_cores as u32;
    const CFG: ObserveCfg = ObserveCfg {
        compute_bound_threshold: 0.8,
        compute_bound_min_wall_secs: 60.0,
    };

    fn st() -> DerivationState {
        let row = crate::db::RecoveryDerivationRow::test_default("floor-t", "x86_64-linux");
        DerivationState::from_recovery_row(row, crate::state::DerivationStatus::Ready).unwrap()
    }

    fn intent(mem: u64, disk: u64, cores: u32, deadline: u32) -> crate::state::SolvedIntent {
        crate::state::SolvedIntent {
            mem_bytes: mem,
            disk_bytes: disk,
            cores,
            deadline_secs: deadline,
            ..Default::default()
        }
    }

    fn observe(
        s: &mut DerivationState,
        peaks: ObservedPeaks,
        reason: AttemptCloseReason,
    ) -> FloorOutcome {
        observe_peaks(s, peaks, reason, &CEIL, PROV_MAX, &CFG)
    }

    // r[verify sched.sla.reactive-floor+8]
    /// sh-041u red-first (a) — *proposition: a `CgroupOom` close with
    /// peaks on every axis hard-doubles MEM and soft-observes DISK.*
    /// Under the retired per-axis-witness shape, the OOM mint touched
    /// mem only and the disk peak (12 GiB on a 40 GiB assigned) was
    /// silently dropped.
    #[test]
    fn observe_peaks_oom_hard_mem_soft_disk() {
        let mut s = st();
        s.sched.last_intent = Some(intent(4 << 30, 40 << 30, 4, 600));
        let o = observe(
            &mut s,
            ObservedPeaks {
                mem_bytes: Some(4 << 30),
                disk_bytes: Some(12 << 30),
                cpu_seconds: Some(80.0),
                wall: Some(Duration::from_secs(300)),
            },
            AttemptCloseReason::Infra(rio_proto::types::FailureClass::CgroupOom),
        );
        assert_eq!(
            s.sched.resource_floor.mem_bytes,
            8 << 30,
            "peak × 2.0 (hard axis): the bug_090 floor band passes \
             (peak == assigned) so headroom = 2.0"
        );
        assert_eq!(
            s.sched.resource_floor.disk_bytes,
            ((12u64 << 30) as f64 * 1.2) as u64,
            "soft observe: the disk peak is evidence even though the \
             REASON was mem (RED at base: disk stayed 0)"
        );
        assert_eq!(
            s.sched.resource_floor.cores, 0,
            "cpu_util = 80/(300×4) = 0.067 ≪ 0.8: cores untouched"
        );
        assert_eq!(s.sched.resource_floor.deadline_secs, 0, "not Timeout");
        assert!(o.hard_promoted && o.soft_promoted);
        assert!(o.at_cap_axes.is_empty());
    }

    // r[verify sched.trust.report-corroboration+6]
    /// sh-041u red-first (d) — bug_102 ceiling band: a forged-HIGH mem
    /// peak (16× assigned, physically impossible under `memory.max`)
    /// REFUSES — that axis observes nothing.
    #[test]
    fn observe_peaks_trust_band_refuses_forged_mem() {
        let mut s = st();
        s.sched.last_intent = Some(intent(4 << 30, 0, 0, 0));
        let o = observe(
            &mut s,
            ObservedPeaks {
                mem_bytes: Some(64 << 30),
                ..Default::default()
            },
            AttemptCloseReason::Infra(rio_proto::types::FailureClass::CgroupOom),
        );
        assert_eq!(
            s.sched.resource_floor.mem_bytes, 0,
            "forged-HIGH peak (16× assigned): refused, axis observes \
             nothing — per-attempt floor growth bounded by ≤ 2 × \
             TRUST_BAND_MEM × assigned"
        );
        assert!(!o.hard_promoted && !o.soft_promoted);
    }

    // r[verify sched.trust.report-corroboration+6]
    /// sh-041u red-first (e) — bug_090 floor band: a `CgroupOom` claim
    /// with a forged-LOW peak (100 MiB on a 4 GiB assigned —
    /// implausible for a real OOM) DEGRADES to soft 1.2×: never
    /// `hard_grew` (so never M_044-persists), never `hard_promoted`
    /// (so never rides promotion-exempt).
    #[test]
    fn observe_peaks_forged_low_peak_oom_degrades_to_soft() {
        let mut s = st();
        s.sched.last_intent = Some(intent(4 << 30, 0, 0, 0));
        let o = observe(
            &mut s,
            ObservedPeaks {
                mem_bytes: Some(100 << 20),
                ..Default::default()
            },
            AttemptCloseReason::Infra(rio_proto::types::FailureClass::CgroupOom),
        );
        assert_eq!(
            s.sched.resource_floor.mem_bytes,
            ((100u64 << 20) as f64 * 1.2) as u64,
            "forged-LOW peak: degraded to soft 1.2× — a hard claim \
             with low peak is implausible (bug_090)"
        );
        assert!(!o.hard_promoted, "never hard_promoted");
        assert!(o.soft_promoted);
    }

    /// sh-041u (round-2) — the disk-axis trust bands REWRITTEN for
    /// `final_resources.peak_disk_bytes` (was: a check on
    /// `QuotaTelemetry.hard_limit_bytes`, no longer consulted). Every
    /// kubelet-mintable peak (overlay(assigned, h) for h ∈
    /// [H_MIN, H_MAX]) corroborates as hard; a forged peak above the
    /// H_MAX-expanded effective assigned refuses; a sub-band peak
    /// degrades to soft.
    #[test]
    fn disk_band_admits_prjquota_peaks_and_refuses_forged_peaks() {
        use rio_common::k8s::{
            DISK_HEADROOM_MAX, DISK_HEADROOM_MIN, KUBELET_QUOTA_BLOCK_SLACK,
            overlay_size_limit_bytes,
        };
        let gi = 1u64 << 30;
        let probe = |assigned: u64, peak: u64| -> (FloorOutcome, u64) {
            let mut s = st();
            s.sched.last_intent = Some(intent(0, assigned, 0, 0));
            let o = observe(
                &mut s,
                ObservedPeaks {
                    disk_bytes: Some(peak),
                    ..Default::default()
                },
                AttemptCloseReason::Infra(rio_proto::types::FailureClass::DiskFull),
            );
            (o, s.sched.resource_floor.disk_bytes)
        };
        for assigned in [gi, 2 * gi, 3 * gi, 100 * gi] {
            // Every kubelet-mintable headroom (curve extremes + flat
            // fallback) under DiskFull → hard 2.0×. The 100GiB cell
            // is a grow-to-cap clip on every h (target ≈ 250–390GiB
            // ≥ 200GiB cap): hard_grew=true (persists), at_cap=true,
            // hard_promoted=false (sh-041u r2 — kernel-disjoint).
            for h in [DISK_HEADROOM_MIN, 1.5, DISK_HEADROOM_MAX] {
                let peak = overlay_size_limit_bytes(assigned, h);
                for slack in [0, KUBELET_QUOTA_BLOCK_SLACK] {
                    let (o, floor) = probe(assigned, peak + slack);
                    assert!(
                        o.hard_grew,
                        "minted peak refused: assigned={assigned} h={h} slack={slack}"
                    );
                    let want = (((peak + slack) as f64 * 2.0) as u64).min(CEIL.max_disk);
                    assert_eq!(floor, want);
                    assert_eq!(
                        o.hard_promoted,
                        want < CEIL.max_disk,
                        "hard_promoted iff !at_cap (assigned={assigned} h={h})"
                    );
                }
            }
            // Forged-HIGH (3.9× — above overlay(assigned, 1.95)):
            // refuses, axis observes nothing.
            let forged = assigned.saturating_mul(39) / 10;
            let (o, floor) = probe(assigned, forged);
            assert!(
                !o.hard_promoted && !o.soft_promoted,
                "a 3.9× fabricated peak must not move floors"
            );
            assert_eq!(floor, 0);
            // Sub-band (peak < overlay(assigned, H_MIN)/2) degrades
            // a hard claim to soft.
            let tiny = overlay_size_limit_bytes(assigned, DISK_HEADROOM_MIN) / 4;
            let (o, _) = probe(assigned, tiny);
            assert!(!o.hard_promoted && o.soft_promoted);
        }
        // Cold start (no assigned shape) refuses everything.
        let (o, floor) = {
            let mut s = st();
            let o = observe(
                &mut s,
                ObservedPeaks {
                    disk_bytes: Some(3 * gi),
                    ..Default::default()
                },
                AttemptCloseReason::Infra(rio_proto::types::FailureClass::DiskFull),
            );
            (o, s.sched.resource_floor.disk_bytes)
        };
        assert!(!o.hard_promoted && !o.soft_promoted);
        assert_eq!(floor, 0);
    }

    // r[verify sched.sla.reactive-floor+8]
    /// **sh-012 D4 cores axis** — *proposition: corroborated
    /// cpu_util ≥ threshold jumps cores; ≪ threshold leaves it.*
    /// (sh-041u: ported from the retired compute-bound-band.)
    #[test]
    fn headroom_cores_band() {
        let mk = |cores: u32, cpu: f64, wall: u64| -> (FloorOutcome, u32) {
            let mut s = st();
            s.sched.last_intent = Some(intent(0, 0, cores, 7200));
            let o = observe(
                &mut s,
                ObservedPeaks {
                    cpu_seconds: Some(cpu),
                    wall: Some(Duration::from_secs(wall)),
                    ..Default::default()
                },
                AttemptCloseReason::ExecutorVariant,
            );
            (o, s.sched.resource_floor.cores)
        };
        // sh-031 regression — chunk-collect-corrupt at iter6 (scaled
        // to fit PROV_MAX=64): 32 cores saturated for 1810s, deadline
        // 7200s. Under the old assigned-deadline denominator:
        // 57600 / (7200×32) = 0.25 → refused. Under the elapsed-wall
        // denominator: 57600 / (1810×32) = 0.99 → corroborates.
        let (o, c) = mk(32, 57_600.0, 1810);
        assert!(
            o.hard_promoted && c == PROV_MAX,
            "sh-031: a saturated build that exits before its nix \
             deadline corroborates (cpu_util=0.99 over elapsed wall)"
        );
        // cpu_util = 2280/(600×4) = 0.95 ≥ 0.8 → jumps.
        let (o, c) = mk(4, 2280.0, 600);
        assert!(o.hard_promoted && c == PROV_MAX);
        // cpu_util = 120/(600×4) = 0.05 ≪ 0.8 → refuses.
        let (o, c) = mk(4, 120.0, 600);
        assert!(!o.hard_promoted && c == 0);
        // Exactly at threshold: corroborates (closed lower bound).
        let (o, _) = mk(4, 1920.0, 600);
        assert!(o.hard_promoted);
        // min_wall_secs guard: a 5s compile error that briefly pegged
        // its cores (cpu_util≈1.0) refuses — the inverse-cost bound.
        let (o, c) = mk(4, 20.0, 5);
        assert!(!o.hard_promoted && c == 0);
        // None anchors refuse: cold start (cores=0).
        let (o, c) = mk(0, 2280.0, 600);
        assert!(!o.hard_promoted && c == 0);
    }

    /// sh-031b: ComputeBound jumps to the partition-aware provisionable
    /// max (NOT ×2), then poisons at_cap on the next saturated attempt.
    #[test]
    fn compute_bound_jumps_to_provisionable_max_then_at_cap() {
        let mut s = st();
        s.sched.last_intent = Some(intent(0, 0, 4, 600));
        let peaks = ObservedPeaks {
            cpu_seconds: Some(2280.0),
            wall: Some(Duration::from_secs(600)),
            ..Default::default()
        };
        // Partition-aware prov_max < catalog-absolute (48 < 64).
        let o = observe_peaks(
            &mut s,
            peaks,
            AttemptCloseReason::ExecutorVariant,
            &CEIL,
            48,
            &CFG,
        );
        assert!(o.hard_promoted && !o.at_cap_axes.contains(Axis::Cores));
        assert_eq!(
            s.sched.resource_floor.cores, 48,
            "first corroborated ComputeBound jumps straight to the \
             partition's provisionable max (NOT ×2=8)"
        );
        // Second observation at prov_max: at_cap, no growth.
        s.sched.last_intent = Some(intent(0, 0, 48, 600));
        let peaks2 = ObservedPeaks {
            cpu_seconds: Some(48.0 * 600.0 * 0.9),
            wall: Some(Duration::from_secs(600)),
            ..Default::default()
        };
        let o = observe_peaks(
            &mut s,
            peaks2,
            AttemptCloseReason::ExecutorVariant,
            &CEIL,
            48,
            &CFG,
        );
        assert!(!o.hard_grew && !o.hard_promoted && o.at_cap_axes.contains(Axis::Cores));
        assert_eq!(s.sched.resource_floor.cores, 48);
        // ICE re-opens: prov_max grows past base → promotes again.
        let o = observe_peaks(
            &mut s,
            peaks2,
            AttemptCloseReason::ExecutorVariant,
            &CEIL,
            96,
            &CFG,
        );
        assert!(o.hard_promoted && !o.at_cap_axes.contains(Axis::Cores));
        assert_eq!(s.sched.resource_floor.cores, 96);
        // prov_max=0 (partition empty): NOT at_cap.
        let o = observe_peaks(
            &mut s,
            peaks2,
            AttemptCloseReason::ExecutorVariant,
            &CEIL,
            0,
            &CFG,
        );
        assert!(!o.hard_promoted && !o.at_cap_axes.contains(Axis::Cores));
    }

    /// sh-031b: a stale `floor.cores` ABOVE the live provisionable max
    /// takes the at-cap arm and heals DOWN.
    #[test]
    fn compute_bound_stale_floor_above_prov_max_heals_at_cap() {
        let mut s = st();
        s.sched.resource_floor.cores = 191;
        s.sched.last_intent = Some(intent(0, 0, 96, 600));
        let o = observe_peaks(
            &mut s,
            ObservedPeaks {
                cpu_seconds: Some(96.0 * 600.0 * 0.9),
                wall: Some(Duration::from_secs(600)),
                ..Default::default()
            },
            AttemptCloseReason::ExecutorVariant,
            &CEIL,
            96,
            &CFG,
        );
        assert!(!o.hard_grew && !o.hard_promoted && o.at_cap_axes.contains(Axis::Cores));
        assert_eq!(s.sched.resource_floor.cores, 96);
    }

    /// sh-041u r3: cores grow-to-cap — `floor=0, last=prov_max`
    /// (initial dispatch sized at the partition max via the SLA p90).
    /// `base=max(0,prov_max)≥prov_max` → at_cap, AND `floor`
    /// strictly grows 0→prov_max → `hard_grew=true` so the M_044
    /// persist fires. r2's `CoresOutcome{promoted}` shape returned
    /// `promoted=false` here and skipped the persist — the bug class
    /// the `hard_grew` split closed for mem/disk survived on cores.
    #[test]
    fn compute_bound_grow_to_cap_sets_hard_grew() {
        let mut s = st();
        s.sched.last_intent = Some(intent(0, 0, PROV_MAX, 600));
        let o = observe(
            &mut s,
            ObservedPeaks {
                cpu_seconds: Some(f64::from(PROV_MAX) * 600.0 * 0.9),
                wall: Some(Duration::from_secs(600)),
                ..Default::default()
            },
            AttemptCloseReason::ExecutorVariant,
        );
        assert!(
            o.hard_grew && !o.hard_promoted && o.at_cap_axes.contains(Axis::Cores),
            "grow-to-cap: hard_grew gates persist; hard_promoted stays \
             kernel-disjoint with at_cap (got {o:?})"
        );
        assert_eq!(s.sched.resource_floor.cores, PROV_MAX);
    }

    #[test]
    fn oom_doubles_from_est_then_floor() {
        let mut s = st();
        s.sched.last_intent = Some(intent(4 << 30, 0, 0, 0));
        let oom = AttemptCloseReason::Infra(rio_proto::types::FailureClass::CgroupOom);
        let o = observe(
            &mut s,
            ObservedPeaks {
                mem_bytes: Some(4 << 30),
                ..Default::default()
            },
            oom,
        );
        assert!(o.hard_promoted && o.at_cap_axes.is_empty());
        assert_eq!(s.sched.resource_floor.mem_bytes, 8 << 30);
        assert_eq!(s.retry.infra_count, 0);
        // Second observation at the new shape (peak == assigned == 8).
        s.sched.last_intent = Some(intent(8 << 30, 0, 0, 0));
        let o = observe(
            &mut s,
            ObservedPeaks {
                mem_bytes: Some(8 << 30),
                ..Default::default()
            },
            oom,
        );
        assert!(o.hard_promoted);
        assert_eq!(s.sched.resource_floor.mem_bytes, 16 << 30);
    }

    #[test]
    fn at_ceiling_reports_at_cap_no_mutation() {
        // merged_bug_016: the mem cap is the SOLVE-domain cap.
        let mut s = st();
        s.sched.resource_floor.mem_bytes = mem_solve_cap(&CEIL);
        s.sched.last_intent = Some(intent(mem_solve_cap(&CEIL), 0, 0, 0));
        let o = observe(
            &mut s,
            ObservedPeaks {
                mem_bytes: Some(mem_solve_cap(&CEIL)),
                ..Default::default()
            },
            AttemptCloseReason::Infra(rio_proto::types::FailureClass::CgroupOom),
        );
        assert!(!o.hard_grew && !o.hard_promoted && o.at_cap_axes.contains(Axis::Mem));
        assert_eq!(s.retry.infra_count, 0);
        assert_eq!(s.sched.resource_floor.mem_bytes, mem_solve_cap(&CEIL));
    }

    #[test]
    fn deadline_uses_24h_cap() {
        let mut s = st();
        s.sched.last_intent = Some(intent(0, 0, 0, 3600));
        let o = observe(
            &mut s,
            ObservedPeaks {
                wall: Some(Duration::from_secs(3600)),
                ..Default::default()
            },
            AttemptCloseReason::Timeout,
        );
        assert!(o.hard_promoted && !o.at_cap_axes.contains(Axis::Deadline));
        assert_eq!(s.sched.resource_floor.deadline_secs, 7200);
        // At cap: at_cap=true, no counter mutation.
        s.sched.resource_floor.deadline_secs = DEADLINE_CAP_SECS;
        s.sched.last_intent = Some(intent(0, 0, 0, DEADLINE_CAP_SECS));
        let o = observe(
            &mut s,
            ObservedPeaks {
                wall: Some(Duration::from_secs(u64::from(DEADLINE_CAP_SECS))),
                ..Default::default()
            },
            AttemptCloseReason::Timeout,
        );
        assert!(!o.hard_grew && !o.hard_promoted && o.at_cap_axes.contains(Axis::Deadline));
        assert_eq!(s.retry.timeout_count, 0, "helper never mutates counters");
        assert_eq!(s.retry.infra_count, 0);
    }

    #[test]
    fn last_intent_at_ceiling_is_at_cap_not_promoted_mem() {
        // peak == assigned == solve-cap, floor=0: target = peak×2 ≥
        // cap → grow-to-cap clip. hard_grew=true (floor 0→cap, the
        // M_044 persist MUST fire — sh-041u r2); hard_promoted=false
        // (kernel-disjoint with at_cap).
        let mut s = st();
        s.sched.last_intent = Some(intent(mem_solve_cap(&CEIL), 0, 0, 0));
        let o = observe(
            &mut s,
            ObservedPeaks {
                mem_bytes: Some(mem_solve_cap(&CEIL)),
                ..Default::default()
            },
            AttemptCloseReason::Infra(rio_proto::types::FailureClass::CgroupOom),
        );
        assert!(
            o.at_cap_axes.contains(Axis::Mem) && o.hard_grew && !o.hard_promoted,
            "grow-to-cap clip ⇒ hard_grew (persist gate) ∧ at_cap ∧ \
             ¬hard_promoted (kernel-disjoint); got {o:?}"
        );
        assert_eq!(
            s.sched.resource_floor.mem_bytes,
            mem_solve_cap(&CEIL),
            "floor catches up to cap so persisted state starts at_cap"
        );
    }

    #[test]
    fn last_intent_at_ceiling_is_at_cap_not_promoted_disk() {
        use rio_common::k8s::overlay_size_limit_bytes;
        let mut s = st();
        s.sched.last_intent = Some(intent(0, CEIL.max_disk, 0, 0));
        // Peak at the kubelet-minted overlay limit for max_disk:
        // target = peak × 2 ≥ max_disk → at_cap.
        let o = observe(
            &mut s,
            ObservedPeaks {
                disk_bytes: Some(overlay_size_limit_bytes(CEIL.max_disk, 1.5)),
                ..Default::default()
            },
            AttemptCloseReason::Infra(rio_proto::types::FailureClass::DiskFull),
        );
        assert!(
            o.at_cap_axes.contains(Axis::Disk) && o.hard_grew && !o.hard_promoted,
            "disk grow-to-cap clip ⇒ hard_grew ∧ at_cap ∧ ¬hard_promoted; got {o:?}"
        );
        assert_eq!(s.sched.resource_floor.disk_bytes, CEIL.max_disk);
    }

    #[test]
    fn last_intent_at_ceiling_is_at_cap_not_promoted_deadline() {
        let mut s = st();
        s.sched.last_intent = Some(intent(0, 0, 0, DEADLINE_CAP_SECS));
        let o = observe(
            &mut s,
            ObservedPeaks {
                wall: Some(Duration::from_secs(u64::from(DEADLINE_CAP_SECS))),
                ..Default::default()
            },
            AttemptCloseReason::Timeout,
        );
        assert!(
            o.at_cap_axes.contains(Axis::Deadline) && o.hard_grew && !o.hard_promoted,
            "deadline grow-to-cap clip ⇒ hard_grew ∧ at_cap ∧ ¬hard_promoted; got {o:?}"
        );
        assert_eq!(s.sched.resource_floor.deadline_secs, DEADLINE_CAP_SECS);
    }

    #[test]
    fn cold_start_zero_base_is_noop_not_promote() {
        // Pre-first-mint: no `last_intent` → no anchor → no observe.
        let mut s = st();
        let o = observe(
            &mut s,
            ObservedPeaks {
                mem_bytes: Some(4 << 30),
                ..Default::default()
            },
            AttemptCloseReason::Infra(rio_proto::types::FailureClass::CgroupOom),
        );
        assert!(!o.hard_grew && !o.hard_promoted && !o.soft_promoted && o.at_cap_axes.is_empty());
        assert_eq!(s.sched.resource_floor.mem_bytes, 0);
    }

    // r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
    /// live_051(d) floor-axis product census — cells from the alphabet
    /// (dim × {fresh-below, at-live-cap, above-live-cap (the stale
    /// row), zero}) driven through BOTH halves of the clamp law (the
    /// read projection and the in-place hydrate clamp). The
    /// above-live-cap rows are R24's unit cells: a 383-era persisted
    /// value projects/clamps to the live cap; below-cap rows are
    /// byte-untouched (kill-isolation: no over-clamp).
    #[test]
    fn floor_axis_product_clamp_law() {
        // (input, live-cap, expected-mem, expected-disk) — hand-written
        // oracle rows, NOT derived from the impl's own min(). The two
        // dimensions carry DIFFERENT laws since merged_bug_016: disk
        // clamps at the raw cap; mem clamps at the SOLVE-domain cap
        // (`cap − pad` for caps at/above the container floor — a mem
        // floor at the raw cap renders an unhostable `cap + pad`
        // container downstream). Real-unit caps so both laws have
        // distinct oracle values.
        const GI: u64 = 1 << 30;
        const PAD: u64 = 256 << 20; // rio_common::footprint pad, by value
        let rows: &[(u64, u64, u64, u64)] = &[
            (0, 64 * GI, 0, 0),    // zero stays zero
            (GI, 64 * GI, GI, GI), // fresh below: untouched
            // at the mem SOLVE cap: untouched on both axes.
            (64 * GI - PAD, 64 * GI, 64 * GI - PAD, 64 * GI - PAD),
            // at the RAW cap: disk untouched, mem heals DOWN to the
            // solve cap (the merged_bug_016 funnel-heal cell).
            (64 * GI, 64 * GI, 64 * GI - PAD, 64 * GI),
            // stale above (383-era): both clamped, each to its own cap.
            (3_072 * GI, 64 * GI, 64 * GI - PAD, 64 * GI),
            // degenerate cap below the container floor: disk clamps
            // raw; mem fails CLOSED to zero (`max_hostable_solve_mem =
            // None` — validate_resolved refuses such globals, the
            // projection is the backstop).
            (50, 100, 0, 50),
        ];
        for &(input, cap, want_mem, want_disk) in rows {
            // Read half: the projection.
            let f = crate::state::ResourceFloor {
                mem_bytes: input,
                disk_bytes: input,
                deadline_secs: 60,
                cores: 0,
            };
            let ceil = Ceilings {
                max_cores: 64.0,
                max_mem: cap,
                max_disk: cap,
                default_disk: 1,
            };
            let p = ClampedFloor::of(&f, &ceil, ceil.max_cores as u32);
            assert_eq!(
                p.mem_bytes, want_mem,
                "mem projection of {input} at cap {cap}"
            );
            assert_eq!(
                p.disk_bytes, want_disk,
                "disk projection of {input} at cap {cap}"
            );
            // Hydrate half: the in-place clamp.
            let mut g = f;
            clamp_floor_to_live(&mut g, &ceil);
            assert_eq!(
                g.mem_bytes, want_mem,
                "mem hydrate-clamp of {input} at cap {cap}"
            );
            assert_eq!(
                g.disk_bytes, want_disk,
                "disk hydrate-clamp of {input} at cap {cap}"
            );
            assert_eq!(g.deadline_secs, 60, "deadline below its cap: untouched");
        }
        // Deadline dimension: capped at DEADLINE_CAP_SECS (Ceilings has
        // no time axis — the one dimension whose cap is the const).
        let mut g = crate::state::ResourceFloor {
            mem_bytes: 0,
            disk_bytes: 0,
            deadline_secs: u32::MAX,
            cores: u32::MAX,
        };
        clamp_floor_to_live(&mut g, &CEIL);
        assert_eq!(g.deadline_secs, DEADLINE_CAP_SECS);
        assert_eq!(g.cores, CEIL.max_cores as u32);
    }

    /// The at-cap heal composes with the projection: a floor ABOVE the
    /// live cap takes [`set_dim`]'s `.min(cap)` (floor catches DOWN to
    /// cap) — the in-memory heal the M_044 persist then writes (a
    /// no-op against the GREATEST ratchet, by design — see
    /// [`ClampedFloor`]).
    #[test]
    fn stale_floor_above_live_cap_heals_down_via_at_cap_arm() {
        let mut s = st();
        s.sched.resource_floor.mem_bytes = CEIL.max_mem * 4; // 383-era row
        s.sched.last_intent = Some(intent(mem_solve_cap(&CEIL), 0, 0, 0));
        let o = observe(
            &mut s,
            ObservedPeaks {
                mem_bytes: Some(mem_solve_cap(&CEIL)),
                ..Default::default()
            },
            AttemptCloseReason::Infra(rio_proto::types::FailureClass::CgroupOom),
        );
        assert!(!o.hard_grew && !o.hard_promoted && o.at_cap_axes.contains(Axis::Mem));
        assert_eq!(
            s.sched.resource_floor.mem_bytes,
            mem_solve_cap(&CEIL),
            "set_dim's .min(cap) heals the in-memory floor down to the \
             LIVE solve-domain cap (merged_bug_016)"
        );
    }

    // r[verify sys.liveness.exit-edge]
    /// **W11-AB (merged_bug_016's R30 face)** — *proposition: an
    /// over-ceiling true mem need reaches the designed bounded
    /// at-cap terminal in BOUNDED steps, and every attempt along the
    /// way — including the at-cap attempts the retry counter charges
    /// — renders a HOSTABLE container under the shared footprint
    /// law; the negation is the advisory-forever loop (pre-fix: the
    /// doubling cap was the raw global, so the at-cap dispatch
    /// rendered `global + pad`, which no class hosts — the counted
    /// attempts could never run and the strand requeued forever).*
    /// Population: the OOM doubling walk from a 1 GiB first dispatch
    /// to at-cap, every step checked.
    #[test]
    fn at_cap_terminal_reachable_in_bounded_steps_and_runnable() {
        let mut s = st();
        let oom = AttemptCloseReason::Infra(rio_proto::types::FailureClass::CgroupOom);
        let mut assigned = 1u64 << 30;
        let mut steps = 0u32;
        loop {
            s.sched.last_intent = Some(intent(assigned, 0, 0, 0));
            let o = observe(
                &mut s,
                ObservedPeaks {
                    mem_bytes: Some(assigned),
                    ..Default::default()
                },
                oom,
            );
            steps += 1;
            // Every dispatch the next attempt would mint (>= the
            // floor) renders a hostable container — the exit edge's
            // REACHABILITY: the terminal's attempts can actually run.
            let next_dispatch = s.sched.resource_floor.mem_bytes;
            assert!(
                rio_common::footprint::container_mem_bytes(next_dispatch) <= CEIL.max_mem,
                "step {steps}: dispatch at {next_dispatch} renders an \
                 unhostable container — the advisory-forever strand"
            );
            if o.at_cap_axes.contains(Axis::Mem) {
                break;
            }
            assigned = next_dispatch;
            assert!(steps < 64, "doubling walk diverged");
        }
        // Hand-derived: 1→2→4→…→128 are steps 1–7 (`hard_promoted`,
        // `!at_cap`); step 8 (target=256 GiB ≥ cap'=256 GiB−256 MiB)
        // is the grow-to-cap clip — `hard_grew=true` (the M_044
        // persist gate), `at_cap=true` (loop breaks here),
        // `hard_promoted=false` (kernel-disjoint). 8 steps total.
        assert!(steps <= 8, "bounded steps to the at-cap terminal");
        assert_eq!(
            s.sched.resource_floor.mem_bytes,
            mem_solve_cap(&CEIL),
            "the terminal state is the hostable solve-domain cap"
        );
    }

    #[test]
    fn non_hard_reasons_soft_only() {
        let mut s = st();
        s.sched.last_intent = Some(intent(4 << 30, 0, 0, 0));
        for r in [
            AttemptCloseReason::Other,
            AttemptCloseReason::ExecutorVariant,
            AttemptCloseReason::WorkerAbort,
        ] {
            let mut s2 = s.clone();
            let o = observe(
                &mut s2,
                ObservedPeaks {
                    mem_bytes: Some(4 << 30),
                    ..Default::default()
                },
                r,
            );
            assert!(!o.hard_promoted, "{r:?} is never hard-mem");
            assert!(o.soft_promoted);
            assert_eq!(
                s2.sched.resource_floor.mem_bytes,
                ((4u64 << 30) as f64 * 1.2) as u64
            );
        }
        assert_eq!(s.retry.infra_count, 0);
    }

    // r[verify sched.retry.promotion-exempt+4]
    /// sh-041u r2: [`set_dim`] returns `{grew, at_cap}` — NOT
    /// disjoint. A grow-to-cap clip sets BOTH (the floor strictly
    /// grew old→cap, AND target ≥ cap). The kernel's
    /// `FloorOutcomeView` disjointness is enforced at the
    /// [`FloorOutcome`] fold (`hard_promoted = grew ∧ !at_cap`
    /// per-axis), not here.
    #[test]
    fn set_dim_at_cap_semantics() {
        // (floor, target, cap) → (new_floor, grew, at_cap)
        for (mut f, target, cap, want_f, want_g, want_a) in [
            (0u64, 8, 100, 8, true, false),    // plain grow
            (8, 4, 100, 8, false, false),      // no-op (target < floor)
            (0, 200, 100, 100, true, true),    // grow-to-cap clip: BOTH set
            (100, 200, 100, 100, false, true), // already at cap (no grow)
            (400, 200, 100, 100, false, true), // stale-above heals down (no grow)
            (50, 100, 100, 100, true, true),   // target == cap: grow-to-cap
            (0, 0, 100, 0, false, false),      // zero target
        ] {
            let SetDimOutcome { grew, at_cap } = set_dim(&mut f, target, cap);
            assert_eq!(
                (f, grew, at_cap),
                (want_f, want_g, want_a),
                "set_dim({target}, {cap})"
            );
        }
    }

    // r[verify sched.trust.report-corroboration+6]
    /// sh-041u r1 — *proposition: a derivation-intrinsic `Other` close
    /// (E3b NotDeterministic / InputRejected / …) with a saturated
    /// `cpu_util` does NOT hard-promote cores.* RED at 7c5d6799b: the
    /// reason-ungated cores arm jumped to `prov_max`, persisted via
    /// M_044, then `handle_permanent_failure` poisoned — the I-199
    /// over-fire on the cores axis.
    #[test]
    fn cores_hard_promote_gated_on_reason() {
        use rio_proto::types::{AttemptTerminalReason as T, FailureClass as F};
        let recorder = rio_test_support::metrics::CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&recorder);
        let mut s = st();
        s.sched.last_intent = Some(intent(0, 0, 4, 600));
        let saturated = ObservedPeaks {
            cpu_seconds: Some(2280.0),
            wall: Some(Duration::from_secs(600)),
            ..Default::default()
        };
        // sh-045: the negative arm is exactly {Other, Timeout} —
        // Infra(CgroupOom) MOVED to the positive arm under the
        // axis_hard widen. Timeout stays NEGATIVE (cpu_util cannot
        // discriminate serial- from parallel-saturated; the deadline
        // ratchet is the single-axis response).
        for r in [AttemptCloseReason::Other, AttemptCloseReason::Timeout] {
            let mut s2 = s.clone();
            let o = observe(&mut s2, saturated, r);
            assert!(
                s2.sched.resource_floor.cores == 0 && !o.at_cap_axes.contains(Axis::Cores),
                "{r:?} is not axis_hard(Cores): cores untouched (got {o:?}, \
                 cores={})",
                s2.sched.resource_floor.cores
            );
        }
        // r[verify sched.floor.timeout-cores-suppressed-metric]
        // The Timeout iteration above (cpu_util=0.95, wall=600≥60,
        // wall≥assigned/2=300) is exactly the suppressed shape.
        assert_eq!(
            recorder.get("rio_scheduler_timeout_cores_suppressed_total{}"),
            1,
            "the suppressed counter increments on a CPU-saturated Timeout"
        );
        // sh-045: the positive arm is exactly the not-derivation-
        // intrinsic closes (the I-170/I-199 boundary).
        for r in [
            AttemptCloseReason::ExecutorVariant,
            AttemptCloseReason::WorkerAbort,
            AttemptCloseReason::Infra(F::CgroupOom),
            AttemptCloseReason::Infra(F::DiskFull),
            AttemptCloseReason::Witnessed(T::OomKilled),
            AttemptCloseReason::Witnessed(T::EvictedEmptyDirSizeLimit),
        ] {
            let mut s2 = s.clone();
            let o = observe(&mut s2, saturated, r);
            assert!(
                o.hard_promoted && s2.sched.resource_floor.cores == PROV_MAX,
                "{r:?} is axis_hard(Cores): cores jumps to prov_max"
            );
        }
    }

    // r[verify sched.floor.axis-trust]
    /// sh-045 (e) — *the (AttemptCloseReason × Axis) product census.*
    /// Enumerated via a no-`_` index match (the
    /// `witnessed_disposition_product_census` form) so the test list is
    /// compiler-pinned. The cores=true set is exactly the
    /// not-derivation-intrinsic closes; Timeout is NOT in (the owner
    /// decision pinned). A future widen is a deliberate test edit.
    #[test]
    fn axis_hard_exhaustive_product_census() {
        use rio_proto::types::{AttemptTerminalReason as T, FailureClass as F};
        // The reachable variant set (Infra(Unspecified) and
        // Witnessed(non-promoting) are unreachable! arms — they panic
        // by design and are excluded here; the no-`_` arm in axis_hard
        // is the rustc-exhaustiveness pin).
        const REASONS: [AttemptCloseReason; 8] = [
            AttemptCloseReason::Infra(F::CgroupOom),
            AttemptCloseReason::Infra(F::DiskFull),
            AttemptCloseReason::Timeout,
            AttemptCloseReason::ExecutorVariant,
            AttemptCloseReason::WorkerAbort,
            AttemptCloseReason::Witnessed(T::OomKilled),
            AttemptCloseReason::Witnessed(T::EvictedEmptyDirSizeLimit),
            AttemptCloseReason::Other,
        ];
        // Closure-set census: every reachable variant indexed by a
        // no-`_` match (a new variant fails this match at compile
        // time).
        fn index(r: &AttemptCloseReason) -> usize {
            match r {
                AttemptCloseReason::Infra(F::CgroupOom) => 0,
                AttemptCloseReason::Infra(F::DiskFull) => 1,
                AttemptCloseReason::Infra(F::Unspecified) => {
                    unreachable!("from_status routes Unspecified → Other")
                }
                AttemptCloseReason::Timeout => 2,
                AttemptCloseReason::ExecutorVariant => 3,
                AttemptCloseReason::WorkerAbort => 4,
                AttemptCloseReason::Witnessed(T::OomKilled) => 5,
                AttemptCloseReason::Witnessed(T::EvictedEmptyDirSizeLimit) => 6,
                AttemptCloseReason::Witnessed(_) => {
                    unreachable!("witnessed_disposition gates the alphabet")
                }
                AttemptCloseReason::Other => 7,
            }
        }
        let mut seen = [0u8; REASONS.len()];
        for r in &REASONS {
            seen[index(r)] += 1;
        }
        assert_eq!(seen, [1; REASONS.len()], "REASONS is the alphabet");
        // The cores=true set, pinned literal.
        let cores_hard: Vec<_> = REASONS
            .iter()
            .filter(|r| r.axis_hard(Axis::Cores))
            .map(index)
            .collect();
        assert_eq!(
            cores_hard,
            vec![0, 1, 3, 4, 5, 6],
            "the cores-hard set is exactly the not-derivation-intrinsic \
             closes; Timeout (idx 2) and Other (idx 7) are OUT"
        );
        // axis_hard returns a value for every (variant × axis) pair —
        // no panics on the reachable set.
        for r in &REASONS {
            for a in [Axis::Mem, Axis::Disk, Axis::Deadline, Axis::Cores] {
                let _ = r.axis_hard(a);
            }
        }
    }

    // r[verify sched.floor.axis-trust]
    /// sh-045 (f) — *proposition: a forged-HIGH `cpu_seconds` from the
    /// heartbeat lane (cpu_util > TRUST_BAND_CORES) refuses on the
    /// cores axis regardless of producer.* GREEN at c0 (the band gate
    /// at `observe_peaks` is producer-agnostic; at c0 the
    /// `Witnessed(_)` arm short-circuits earlier on
    /// `!axis_hard(Cores)`, after c4 the band-refusal arm fires) —
    /// proves the trust
    /// envelope survives the heartbeat producer.
    #[test]
    fn heartbeat_forged_cpu_seconds_band_refused() {
        use rio_proto::types::AttemptTerminalReason as T;
        let mut s = st();
        s.sched.last_intent = Some(intent(0, 0, 32, 0));
        // cpu_util = 32×600×5 / (600×32) = 5.0 > TRUST_BAND_CORES.
        let forged = ObservedPeaks {
            cpu_seconds: Some(32.0 * 600.0 * 5.0),
            wall: Some(Duration::from_secs(600)),
            ..Default::default()
        };
        let o = observe(
            &mut s,
            forged,
            AttemptCloseReason::Witnessed(T::EvictedEmptyDirSizeLimit),
        );
        assert!(
            !o.hard_promoted && !o.hard_grew && s.sched.resource_floor.cores == 0,
            "forged-HIGH heartbeat cpu_seconds is band-refused on the \
             witnessed lane (cores untouched)"
        );
    }

    // r[verify sched.executor.input-bounds+2]
    /// sh-041u r1 — *proposition: a forged-HIGH `cpu_seconds` (∞,
    /// NaN, or any `cpu_util > TRUST_BAND_CORES`) refuses on the
    /// cores axis — never `hard_grew` (so never M_044-persists),
    /// never `hard_promoted`.*
    #[test]
    fn cores_trust_band_refuses_forged_cpu_seconds() {
        let probe = |cpu: f64| -> (FloorOutcome, u32) {
            let mut s = st();
            s.sched.last_intent = Some(intent(0, 0, 4, 600));
            let o = observe(
                &mut s,
                ObservedPeaks {
                    cpu_seconds: Some(cpu),
                    wall: Some(Duration::from_secs(600)),
                    ..Default::default()
                },
                AttemptCloseReason::ExecutorVariant,
            );
            (o, s.sched.resource_floor.cores)
        };
        // ∞ / NaN: refused at intake (`from_report` filters
        // non-finite); a direct ObservedPeaks bypass still refuses
        // via the explicit `is_nan() || > band` guard (sh-041u r3).
        for cpu in [f64::INFINITY, f64::NAN, 1e18, 600.0 * 4.0 * 1.5] {
            let (o, cores) = probe(cpu);
            assert!(
                !o.hard_promoted && !o.hard_grew && cores == 0,
                "cpu_seconds={cpu}: forged-HIGH cpu_util refused"
            );
        }
        // `from_report` drops non-finite at the intake filter.
        let p = ObservedPeaks::from_report(0, Some(f64::INFINITY), None);
        assert!(p.cpu_seconds.is_none());
        let p = ObservedPeaks::from_report(0, Some(f64::NAN), None);
        assert!(p.cpu_seconds.is_none());
        // Honest band edge (cpu_util = 1.0): corroborates.
        let (o, cores) = probe(600.0 * 4.0);
        assert!(o.hard_promoted && cores == PROV_MAX);
    }

    /// sh-041u: success statuses return `None` from
    /// [`AttemptCloseReason::from_status`] — chokepoint #2 skips the
    /// observe (the SLA fit's `p90 × headroom` dominates the soft
    /// floor at the solve, so a success-path soft observe is
    /// steady-state redundant).
    #[test]
    fn from_status_success_is_none() {
        use rio_proto::types::BuildResultStatus as S;
        for s in [S::Built, S::Substituted, S::AlreadyValid] {
            assert!(AttemptCloseReason::from_status(s, None).is_none());
        }
        assert!(matches!(
            AttemptCloseReason::from_status(S::TimedOut, None),
            Some(AttemptCloseReason::Timeout)
        ));
    }

    // r[verify sched.attempt.witnessed-terminal+3]
    /// live_058-b: the witnessed-reason × establish-disposition
    /// product census (the R25 proof obligation — the injectivity of
    /// the promotion set is COUNTED from the generated table, never
    /// asserted in prose). Membership is pinned through an exhaustive
    /// index match (the `GcPhase3Outcome::ALL` form), so a new wire
    /// letter cannot ship without joining `WITNESSED_LETTERS`, taking
    /// a `witnessed_disposition` row (rustc exhaustiveness), and
    /// filing its oracle row here.
    #[test]
    fn witnessed_disposition_product_census() {
        use rio_proto::types::AttemptTerminalReason as R;
        // Closure-set census: every letter appears in WITNESSED_LETTERS
        // exactly once (a new variant fails this match at compile time).
        fn index(r: R) -> usize {
            match r {
                R::Unspecified => 0,
                R::OomKilled => 1,
                R::EvictedDiskPressure => 2,
                R::EvictedOther => 3,
                R::Completed => 4,
                R::Error => 5,
                R::DeadlineExceeded => 6,
                R::Cancelled => 7,
                R::Preempted => 8,
                R::Reaped => 9,
                R::NoEligibleSource => 10,
                R::EvictedEmptyDirSizeLimit => 11,
            }
        }
        let mut seen = [0u8; WITNESSED_LETTERS.len()];
        for r in WITNESSED_LETTERS {
            seen[index(r)] += 1;
        }
        assert_eq!(
            seen,
            [1; WITNESSED_LETTERS.len()],
            "WITNESSED_LETTERS is the alphabet"
        );

        // Exactly TWO `Some(..)` rows — the per-container kubelet
        // attributions (OomKilled, sh-039's EvictedEmptyDirSizeLimit).
        // Node-condition EvictedDiskPressure stays `None` (I-199
        // untouched).
        assert_eq!(
            WITNESSED_LETTERS
                .iter()
                .filter(|r| witnessed_disposition(**r).is_some())
                .count(),
            2,
            "the two per-container kubelet attributions are the ONLY \
             promoting letters"
        );
        // The product table — hand-written oracle rows.
        for (letter, want_some) in [
            (R::Unspecified, false),
            (R::OomKilled, true),
            (R::EvictedDiskPressure, false),
            (R::EvictedOther, false),
            (R::Completed, false),
            (R::Error, false),
            (R::DeadlineExceeded, false),
            (R::Cancelled, false),
            (R::Preempted, false),
            (R::Reaped, false),
            (R::NoEligibleSource, false),
            (R::EvictedEmptyDirSizeLimit, true),
        ] {
            assert_eq!(
                witnessed_disposition(letter).is_some(),
                want_some,
                "letter={letter:?}"
            );
        }
    }
}
