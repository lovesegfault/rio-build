//! D4 reactive resource floor: per-dimension doubling on explicit
//! resource-exhaustion signals, capped at `Ceilings`, falling through
//! to the relevant retry counter once capped.
//!
//! Replaces the legacy `promote_size_class_floor` (class-name ladder
//! walk). Under SLA there are no class names; cold-start safety
//! (first-ever build of a pname OOMs at probe defaults) still needs an
//! immediate-retry-bigger mechanism — waiting for `refit()` is too
//! slow.

use rio_proto::types::TerminationReason;

use crate::sla::solve::Ceilings;
use crate::state::DerivationState;

/// Hard cap on `floor.deadline_secs` (24h). Separate from `Ceilings`
/// (which has no time dimension) — a build that hasn't finished in a
/// day is a runaway regardless of pod shape.
pub(super) const DEADLINE_CAP_SECS: u32 = 86_400;

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
/// `bump_dim` then reports `at_cap` immediately and the caller's
/// retry counter bounds the loop.
pub(super) fn mem_solve_cap(ceil: &Ceilings) -> u64 {
    rio_common::footprint::max_hostable_solve_mem(ceil.max_mem).unwrap_or(0)
}

/// Result of [`bump_floor_or_count`]. Two independent bits because
/// callers need both: `promoted` gates the promotion-exempt path
/// (`r[sched.retry.promotion-exempt+2]`); `at_cap` tells the caller
/// the floor is already at the relevant ceiling so no further growth
/// is possible — the caller's retry-counter increment + cap-check is
/// what bounds this case.
///
/// This helper does NOT mutate any retry counter. All counter
/// increments live at the call site, AFTER the cap check, so at-cap
/// and non-floor failures see the same `max_*_retries` bound (the
/// previous in-helper increment poisoned at-cap one attempt earlier).
#[derive(Debug, Default, Clone, Copy)]
pub struct FloorOutcome {
    /// Floor changed; next dispatch will be larger. Promotion-exempt.
    pub promoted: bool,
    /// Floor was already at the relevant cap (mem/disk → `Ceilings`,
    /// deadline → 24h). NOT mutually exclusive with `promoted=false`
    /// for non-resource reasons (both false there).
    pub at_cap: bool,
}

// r[impl sched.sla.reactive-floor+5]
// r[impl sched.retry.promotion-exempt+3]
/// Double the relevant `resource_floor` dimension on an explicit
/// resource-exhaustion signal, or — if already at the cap — report
/// `at_cap=true` so the caller's retry counter bounds it. See
/// [`FloorOutcome`]. No retry counters are mutated here; the CALLER
/// increments after its cap check so at-cap and non-floor failures
/// poison at the same attempt number.
///
/// The doubling base is `state.sched.last_intent` — stamped by the
/// pull mint (live_040: the mint is the dispatch decision) with the
/// RECONCILED dispatch shape (bug_027: `DispatchShape::reconcile`
/// lifts the solve's deadline to `max(resolved, carried)`, so `last`
/// is what the pod actually ran under — a carried deadline at the cap
/// reads as `base ≥ cap` and takes the counted at-cap arm instead of
/// an exempt under-double). The `max(floor, last)` form means a stale
/// floor (lower than what was actually minted) doesn't under-double;
/// if both are zero (cold start, never minted), the helper returns
/// `{promoted:false, at_cap:false}` — the caller's unconditional
/// post-check increment bounds this (I-200).
pub fn bump_floor_or_count(
    state: &mut DerivationState,
    reason: TerminationReason,
    ceil: &Ceilings,
) -> FloorOutcome {
    use TerminationReason as R;
    let floor = &mut state.sched.resource_floor;
    let last = state.sched.last_intent.as_ref();
    match reason {
        // merged_bug_016: the doubling cap is the SOLVE-domain mem cap
        // (`mem_solve_cap`), not the raw global — at-cap dispatches
        // must render a hostable container (`== ceiling` exactly) so
        // the counted at-cap attempts actually run and the retry
        // counter's bounded poison terminal stays reachable.
        R::OomKilled => bump_dim(
            &mut floor.mem_bytes,
            last.map_or(0, |i| i.mem_bytes),
            mem_solve_cap(ceil),
        ),
        // LIVE with exactly ONE producer (live_057-b): the worker
        // quota-attributed DiskFull lane — completion.rs consumes the
        // TYPED `failure_classification` wire field (FailureClass::
        // DiskFull + QuotaTelemetry; `rio_proto::DISK_FULL_MSG` is
        // DISPLAY/NARRATION ONLY — quantifier: census(forged_free_text_never_moves_resource_floors) — merged_bug_100: free text drives
        // nothing), band-corroborates it against the assigned shape
        // (`CorroborationWitness::corroborated_sizing`), and bumps
        // through the witness-demanding chokepoint with the
        // `disk_full` label; precise by construction at the prjquota
        // seam (`apply_disk_override` — the prjquota-vs-statvfs
        // classification at result assembly), with the worker's
        // once-per-attempt assignment-token dedup — exactly the
        // re-entry lane the parked-era note named. Witnessed
        // evictions remain CLASSIFY-ONLY BY RULING
        // ([`witnessed_disposition`]): the controller's classifier
        // folds NODE-CONDITION evictions ("DiskPressure",
        // "ephemeral-storage") together with the pod-attributed
        // shapes ("ephemeral local storage", "EmptyDir volume") into
        // the ONE EvictedDiskPressure letter (pool/job.rs
        // pod_termination_reason), so the letter carries no per-pod
        // sizing authority — promoting it would re-create the I-199
        // ambient-cause over-fire on the disk axis (one node-pressure
        // event evicting k innocent builder pods would double k
        // sticky M_044 floors). Their sizing recovery IS this worker
        // lane on the next attempt: the kubelet-evicted pname
        // re-dispatches, and if its own quota is truly the constraint
        // the worker lane classifies and doubles.
        R::EvictedDiskPressure => bump_dim(
            &mut floor.disk_bytes,
            last.map_or(0, |i| i.disk_bytes),
            ceil.max_disk,
        ),
        R::DeadlineExceeded => {
            // u32 dimension: widen for the shared body, narrow back. Cap
            // is 86_400 so the cast cannot truncate.
            let mut f = u64::from(floor.deadline_secs);
            let o = bump_dim(
                &mut f,
                u64::from(last.map_or(0, |i| i.deadline_secs)),
                u64::from(DEADLINE_CAP_SECS),
            );
            floor.deadline_secs = f as u32;
            o
        }
        // sh-012 D4 fourth axis: a corroborated compute-bound
        // executor-variant exit≠0 (`CorroborationWitness::
        // corroborated_compute_bound` — cpu_util ≥ threshold) doubles
        // cores. u32 dimension; same widen-narrow shape as deadline.
        // Cap is `Ceilings.max_cores` (the catalog-derived global), so
        // the cast cannot truncate.
        R::ComputeBound => {
            let mut f = u64::from(floor.cores);
            let o = bump_dim(
                &mut f,
                u64::from(last.map_or(0, |i| i.cores)),
                ceil.max_cores as u64,
            );
            floor.cores = f as u32;
            o
        }
        // Non-resource reasons (pod-kill, node failure, expected
        // one-shot exit, unclassified) are not sizing signals.
        R::EvictedOther | R::Completed | R::Error | R::Unknown => FloorOutcome::default(),
    }
}

// r[impl sched.trust.report-corroboration+4]
/// bug_102 — the typed corroboration witness `bump_resource_floor`
/// DEMANDS: trust is gated at the CONSEQUENCE (the floor mutation),
/// not re-derived per carrier at each call site. The wave-11 gate
/// covered only the `failure_classification`-carried claims
/// (CgroupOom/DiskFull); `TimedOut` rides STATUS and bypassed it —
/// `handle_timeout_failure` bumped the deadline floor unconditionally
/// and pre-verdict, so a hostile builder ratcheted a cross-tenant 24h
/// deadline floor in ~5 cheap reports (the GREATEST() ratchet never
/// heals downward). With the demand INSIDE the mutation, an ungated
/// axis is unrepresentable: no caller can compile a bump without
/// presenting a witness, and every witness is minted by a verifying
/// constructor against a scheduler-owned anchor the worker cannot
/// choose.
///
/// The axis is PRIVATE (the inner enum is not visible outside this
/// module), so the constructors below are the ONLY mints — quantifier: census(floor_mutation_census) —:
/// - [`Self::corroborated_sizing`] — the mem/disk bands vs the
///   assigned shape (the wave-11 bands, moved here so the band law
///   and the witness mint are one site);
/// - [`Self::corroborated_timeout`] — attempt-open-duration >=
///   assigned_deadline/2 (the scheduler's own `running_since` stamp
///   vs the reconciled `last_intent.deadline_secs`);
/// - [`Self::witnessed`] — the establishment sweep's
///   controller-witnessed OomKilled disposition row (the
///   `sched.attempt.witnessed-terminal` mark; kubelet per-container
///   attribution, deduped by the establishment `won` flag);
/// - [`Self::corroborated_compute_bound`] — `cpu_seconds_total /
///   (assigned_deadline × assigned_cores) >= threshold` (sh-012, the
///   D4 cores axis: an executor-variant exit≠0 that demonstrably
///   exhausted its parallelism budget).
///
/// The `(TerminationReason, label)` pair DERIVES from the witness
/// ([`Self::reason`]/[`Self::label`]) — one producer for the mapping
/// the four call sites previously each restated.
pub(super) struct CorroborationWitness {
    axis: WitnessAxis,
}

/// Private: only the verifying constructors mint witnesses.
#[derive(Debug, Clone, Copy)]
enum WitnessAxis {
    /// Typed wire claim, band-corroborated (CgroupOom or DiskFull).
    Sizing(rio_proto::types::FailureClass),
    /// Worker TimedOut, anchored on the attempt's own open duration.
    Timeout,
    /// Controller-witnessed OomKilled at establishment.
    WitnessedOom,
    /// sh-012: executor-variant exit≠0 with corroborated cpu
    /// utilization ≥ threshold (the D4 cores axis).
    ComputeBound,
}

impl CorroborationWitness {
    /// The acceptance bands (CONSUMER-owned tolerances — they only
    /// bound forgery and live in exactly one place, here):
    ///
    /// * CGROUP_OOM: `peak_memory_bytes >= assigned_mem / 2` —
    ///   memory.peak saturates at memory.max under a real oom kill.
    /// * DISK_FULL (bug_065, R33'(ii) — the band consumes the SHARED
    ///   quota denomination, never a re-open-coded formula): an
    ///   ENFORCED project quota on the overlays emptyDir carries the
    ///   stamped sizeLimit =
    ///   `rio_common::k8s::overlay_size_limit_bytes(assigned_disk, h)`
    ///   for the intent's headroom `h`, and every producible `h` lies
    ///   in `[DISK_HEADROOM_MIN, DISK_HEADROOM_MAX]` (the
    ///   `headroom(n_eff)` codomain + the controller fallback; pinned
    ///   by `headroom_curve_stays_inside_the_shared_band` below
    ///   against the live curve). NOTE the deployed kubelet minors
    ///   assign NON-ENFORCING sentinel quotas (usage tracking only —
    ///   the rio_common::k8s denomination doc + the
    ///   vm-kubelet-projquota cells pin this), so kubelet-quota'd
    ///   nodes currently produce NO DiskFull claims at all; the band
    ///   is the contract for enforced-quota producers (the
    ///   vm-quota-probe manual-limit world; any future enforcing
    ///   kubelet) and refuses sentinel-armed claims by construction.
    ///   Acceptance:
    ///   `hard in [overlay(assigned, H_MIN),
    ///             overlay(assigned, H_MAX) + KUBELET_QUOTA_BLOCK_SLACK]`
    ///   AND `peak_used >= hard / 2`. The wave-11 band
    ///   (`[assigned/2, assigned*4]` of RAW disk) accepted any
    ///   fabricated limit within 8x of the solve axis — a forger
    ///   could move floors with a hard limit kubelet can never mint — quantifier: census(disk_band_admits_minted_products_and_refuses_off_formula_limits) —;
    ///   the producer-derived band admits exactly the mintable
    ///   products (plus quota-block rounding).
    ///
    /// No assigned shape (cold start) => `None`: nothing to
    /// corroborate against.
    pub(super) fn corroborated_sizing(
        claim: &super::report_ctx::SizingClaim,
        assigned_mem: u64,
        assigned_disk: u64,
    ) -> Option<Self> {
        use rio_common::k8s::{
            DISK_HEADROOM_MAX, DISK_HEADROOM_MIN, KUBELET_QUOTA_BLOCK_SLACK,
            overlay_size_limit_bytes,
        };
        use rio_proto::types::FailureClass;
        let corroborated = match claim.class {
            FailureClass::Unspecified => false, // unreachable: never constructed
            FailureClass::CgroupOom => {
                assigned_mem > 0 && claim.peak_memory_bytes >= assigned_mem / 2
            }
            FailureClass::DiskFull => claim.quota.is_some_and(|q| {
                assigned_disk > 0
                    && q.hard_limit_bytes
                        >= overlay_size_limit_bytes(assigned_disk, DISK_HEADROOM_MIN)
                    && q.hard_limit_bytes
                        <= overlay_size_limit_bytes(assigned_disk, DISK_HEADROOM_MAX)
                            .saturating_add(KUBELET_QUOTA_BLOCK_SLACK)
                    && q.peak_used_bytes >= q.hard_limit_bytes / 2
            }),
        };
        corroborated.then_some(Self {
            axis: WitnessAxis::Sizing(claim.class),
        })
    }

    /// The timeout axis (bug_102's unsealed face): the attempt must
    /// have demonstrably RUN at least half its assigned deadline —
    /// `attempt_open` is the scheduler's own `running_since` elapsed
    /// (stamped at the Running transition; a worker cannot mint it),
    /// `assigned_deadline_secs` the reconciled dispatch deadline
    /// (`last_intent` — bug_027's `max(resolved, carried)`).
    ///
    /// `None` anchors refuse: no `running_since` (failover-recovered
    /// node — the lossy-Instant conservative default; the NEXT
    /// attempt re-corroborates) or no assigned deadline (cold start)
    /// mean there is nothing to corroborate against — classify-only,
    /// the conservative direction (a floor never moves on absent
    /// evidence).
    pub(super) fn corroborated_timeout(
        attempt_open: Option<std::time::Duration>,
        assigned_deadline_secs: u32,
    ) -> Option<Self> {
        let open = attempt_open?;
        if assigned_deadline_secs == 0 {
            return None;
        }
        (open.as_secs() >= u64::from(assigned_deadline_secs) / 2).then_some(Self {
            axis: WitnessAxis::Timeout,
        })
    }

    /// The compute-bound axis (sh-012, the D4 fourth dimension): an
    /// executor-variant exit≠0 demonstrably exhausted its parallelism
    /// budget — `cpu_util = cpu_seconds_total / (assigned_deadline ×
    /// assigned_cores) >= threshold`. `cpu_seconds_total` is the
    /// builder's own `cpu.stat usage_usec` cumulative read (carried in
    /// `CompletionReport.final_resources` even on the executor-error
    /// path); `assigned_{cores,deadline}` are the reconciled
    /// `last_intent` (scheduler-stamped at the pull mint, never
    /// worker-mintable). A genuine compile-error exit dies fast with
    /// `cpu_util ≪ threshold` and refuses; a parallelism-exhausted
    /// build saturates and corroborates.
    ///
    /// `None` anchors refuse: missing telemetry (old builder), zero
    /// assigned cores/deadline (cold start, never minted) — there is
    /// nothing to corroborate against, so classify-only (the
    /// conservative direction: a floor never moves on absent
    /// evidence).
    pub(super) fn corroborated_compute_bound(
        cpu_seconds_total: Option<f64>,
        assigned_cores: u32,
        assigned_deadline_secs: u32,
        threshold: f64,
    ) -> Option<Self> {
        let cpu = cpu_seconds_total?;
        if assigned_cores == 0 || assigned_deadline_secs == 0 {
            return None;
        }
        let cpu_util = cpu / (f64::from(assigned_deadline_secs) * f64::from(assigned_cores));
        (cpu_util >= threshold).then_some(Self {
            axis: WitnessAxis::ComputeBound,
        })
    }

    /// The controller-witnessed lane: exactly the
    /// [`WitnessedDisposition::PromoteMemFloor`] row (witnessed
    /// OomKilled — the one structurally unambiguous kubelet
    /// attribution); every other disposition is classify-only and
    /// mints nothing.
    pub(super) fn witnessed(disposition: WitnessedDisposition) -> Option<Self> {
        match disposition {
            WitnessedDisposition::PromoteMemFloor => Some(Self {
                axis: WitnessAxis::WitnessedOom,
            }),
            WitnessedDisposition::ClassifyOnly => None,
        }
    }

    /// The floor dimension this witness authorizes (consumed by
    /// `bump_floor_or_count` via the caller).
    pub(super) fn reason(&self) -> rio_proto::types::TerminationReason {
        use rio_proto::types::{FailureClass, TerminationReason as R};
        match self.axis {
            WitnessAxis::Sizing(FailureClass::CgroupOom) => R::OomKilled,
            WitnessAxis::Sizing(FailureClass::DiskFull) => R::EvictedDiskPressure,
            // Unreachable: corroborated_sizing never mints it.
            WitnessAxis::Sizing(FailureClass::Unspecified) => R::Unknown,
            WitnessAxis::Timeout => R::DeadlineExceeded,
            WitnessAxis::WitnessedOom => R::OomKilled,
            WitnessAxis::ComputeBound => R::ComputeBound,
        }
    }

    /// The metric/log label — the caller-census alphabet
    /// (`{cgroup_oom, disk_full, timeout, witnessed_oom,
    /// compute_bound}`, lib.rs HELP in lockstep), derived from the
    /// witness instead of restated per call site.
    pub(super) fn label(&self) -> &'static str {
        use rio_proto::types::FailureClass;
        match self.axis {
            WitnessAxis::Sizing(FailureClass::CgroupOom) => "cgroup_oom",
            WitnessAxis::Sizing(FailureClass::DiskFull) => "disk_full",
            WitnessAxis::Sizing(FailureClass::Unspecified) => "unspecified",
            WitnessAxis::Timeout => "timeout",
            WitnessAxis::WitnessedOom => "witnessed_oom",
            WitnessAxis::ComputeBound => "compute_bound",
        }
    }
}

/// live_058-b: establish-time disposition of a controller-WITNESSED
/// terminal letter (the witnessed-terminal mark's reason). Consumed by
/// the establishment sweep's charge arm — the dispatch over the
/// producer's FULL wire type, so a new letter cannot ship without
/// taking a position here (rustc exhaustiveness; the product census
/// pins one row per letter with EXACTLY ONE promoting row, and the
/// review default for a new row is classify-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WitnessedDisposition {
    /// THE one promoting row: witnessed `OOMKilled` doubles the MEM
    /// floor at establishment (`bump_resource_floor`, label
    /// `witnessed_oom`), gated on the establishment transaction's
    /// append+decide `won` flag — at most once per attempt, ever.
    PromoteMemFloor,
    /// Mark + witnessed-clock window + establish + requeue; the floor
    /// is never touched.
    ClassifyOnly,
}

// r[impl sched.attempt.witnessed-terminal]
/// The per-reason disposition table. The promotion set is derived
/// PER-REASON, never inherited: kubelet `OOMKilled` is a per-container
/// `containerStatuses` attribution — the one controller-witnessed
/// reason that is structurally unambiguous, and the only letter the
/// I-199 retirement derivation covers (the retired heuristic promoted
/// on AMBIGUOUS signals; see `bump_resource_floor`'s doc). Every
/// other letter is classify-only.
pub(super) fn witnessed_disposition(
    reason: rio_proto::types::AttemptTerminalReason,
) -> WitnessedDisposition {
    use WitnessedDisposition as D;
    use rio_proto::types::AttemptTerminalReason as R;
    match reason {
        // Wire-default / unclassifiable: never a sizing signal.
        R::Unspecified => D::ClassifyOnly,
        // THE promoting row: per-container kubelet attribution — the
        // pod hit ITS memory limit; nothing ambient about it.
        R::OomKilled => D::PromoteMemFloor,
        // Classify-only BY RULING (I-199), REFINED not reopened
        // (live060-f): the ruling's rationale is AMBIGUITY — "a
        // node-condition eviction says nothing about THIS build's
        // disk use" — and for the node-condition shapes
        // ("DiskPressure", "ephemeral-storage") it stands untouched.
        // The controller TODAY folds the pod-attributed shapes
        // ("ephemeral local storage", "EmptyDir volume" — kubelet's
        // own per-pod statement that THIS build exceeded ITS declared
        // limit, carrying none of that ambiguity) into the same wire
        // letter (pool/job.rs pod_termination_reason), so this row
        // must stay classify-only: promoting the folded letter would
        // re-create the I-199 ambient over-fire on the disk axis.
        // The SPLIT letter exists in the shared vocabulary
        // (rio_common::classify::AttemptTerminalKind::
        // EvictedEmptyDirSizeLimit — inert/unproduced), and its
        // promote-with-witness arm (through the bug_102 corroboration
        // chokepoint) is RULED pending the wire carrier: the
        // controller→scheduler report enum has no pod-attributed
        // value and adding one is a .fields wire change barred this
        // wave (zero amendment wire changes). Trigger to revive: the
        // next eviction-shaped sizing incident, or the owner
        // commissioning the additive enum value under the .fields
        // ritual; live060-a's worker quota lane alone also revives
        // the upward ladder (the designed producer — see the lane
        // above).
        R::EvictedDiskPressure => D::ClassifyOnly,
        // Ambient by definition (MemoryPressure / PID pressure /
        // node-shutdown): node-cause, never per-pod sizing evidence.
        R::EvictedOther => D::ClassifyOnly,
        // Expected one-shot exit: not a failure.
        R::Completed => D::ClassifyOnly,
        // Pod death that was not the build's fault (panic, operator
        // SIGKILL, node failure).
        R::Error => D::ClassifyOnly,
        // The deadline floor's producer is the worker-reported
        // TimedOut lane: the Job-level kill carries no per-container
        // attribution of WHY the deadline passed (wedge vs slow vs
        // node), so the witnessed letter stays classify-only.
        R::DeadlineExceeded => D::ClassifyOnly,
        // Controller-synthesized verdicts close charge-free in their
        // own intake arm when the assignment is active; a marked
        // straggler is operator/platform action, never sizing
        // evidence.
        R::Cancelled => D::ClassifyOnly,
        // Platform disruption (DisruptionTarget): ambient.
        R::Preempted => D::ClassifyOnly,
        // Controller reap: platform action.
        R::Reaped => D::ClassifyOnly,
        // Spawn-gate verdict: no pod and no attempt — handled before
        // attempt resolution, a mark cannot exist for it (this row is
        // the belt).
        R::NoEligibleSource => D::ClassifyOnly,
    }
}

/// Every wire letter exactly once — the product census's iteration
/// domain, pinned by an exhaustive index match in the census test so
/// a new variant cannot ship without joining this set AND taking a
/// disposition row above (the `GcPhase3Outcome::ALL` form).
#[cfg(test)]
pub(super) const WITNESSED_LETTERS: [rio_proto::types::AttemptTerminalReason; 11] = {
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
    ]
};

// r[impl scheduler.sla.ceiling.stale-solve-revalidation+2]
/// live_051(d): the read-time projection of a resource floor under the
/// LIVE ceiling vector — the floor-axis half of the stale-solve
/// revalidation law. A persisted M_044 floor is evidence minted UNDER
/// a ceiling vector: `bump_dim` caps at the boot-resolved global and
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
    pub(super) fn of(floor: &crate::state::ResourceFloor, ceil: &Ceilings) -> Self {
        Self {
            // merged_bug_016: the mem floor grounds at the SOLVE-domain
            // cap (`mem_solve_cap`) — a floor at the raw global renders
            // an unhostable `ceiling + pad` container downstream.
            mem_bytes: floor.mem_bytes.min(mem_solve_cap(ceil)),
            disk_bytes: floor.disk_bytes.min(ceil.max_disk),
            cores: floor.cores.min(ceil.max_cores as u32),
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
    f.cores = f.cores.min(ceil.max_cores as u32);
}

/// Per-dimension doubling shared by the three resource arms above.
///
/// `base = max(floor, last)` is what was actually dispatched
/// (`snapshot.rs` clamps `solve` output at `floor.max(...).min(cap)`,
/// where the mem dimension's cap is [`mem_solve_cap`] — the
/// solve-domain projection of the global, merged_bug_016 — and the
/// disk dimension's cap is the raw `ceil.max_disk`; the caps passed
/// to this fn by [`bump_floor_or_count`] match those dispatch pins
/// dimension-for-dimension, which is what makes `base ≥ cap` mean
/// "the dispatched shape cannot grow").
/// `at_cap` therefore tests `base`, not bare `floor`: when `floor=0`
/// and `last == cap` (SLA fit predicted ≥ceiling, clamped at dispatch),
/// the next dispatch is still `cap` — no growth possible — so callers'
/// retry counters must engage. Testing bare `floor` returned
/// `{promoted:true, at_cap:false}` for that case, burning one
/// uncounted at-ceiling retry before `at_cap` engaged on the second
/// failure.
///
/// On `at_cap`, `floor` catches up to `cap` so the M_044 persist sees
/// the change and a fresh-state scheduler instance starts at_cap.
fn bump_dim(floor: &mut u64, last: u64, cap: u64) -> FloorOutcome {
    let base = (*floor).max(last);
    if base >= cap {
        *floor = cap;
        FloorOutcome {
            promoted: false,
            at_cap: true,
        }
    } else {
        let next = base.saturating_mul(2).min(cap);
        let promoted = next > base;
        *floor = next;
        FloorOutcome {
            promoted,
            at_cap: false,
        }
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

    /// bug_065: the band accepts exactly the kubelet-mintable
    /// products. RED-FIRST (recorded in the commit body): under the
    /// wave-11 raw band (`[assigned/2, assigned*4]`) the 3.9x forged
    /// limit CORROBORATED — a worker could move floors with a hard
    /// limit kubelet can never assign — quantifier: non-normative(narrates the retired pre-fix band; the live claim is bound at the band doc and pinned by this test's own asserts) —.
    #[test]
    fn disk_band_admits_minted_products_and_refuses_off_formula_limits() {
        use rio_common::k8s::{
            DISK_HEADROOM_MAX, DISK_HEADROOM_MIN, KUBELET_QUOTA_BLOCK_SLACK,
            overlay_size_limit_bytes,
        };
        let gi = 1u64 << 30;
        let claim = |hard: u64, peak: u64| super::super::report_ctx::SizingClaim {
            class: rio_proto::types::FailureClass::DiskFull,
            peak_memory_bytes: 0,
            quota: Some(rio_proto::types::QuotaTelemetry {
                peak_used_bytes: peak,
                hard_limit_bytes: hard,
                node_free_bytes: 50 * gi,
            }),
        };
        // FITTED SCALE (the warm-fit population, W13-G): 2 GiB
        // assigned disk. Every headroom the producers can mint
        // corroborates — incl. the curve extremes and the flat
        // fallback, with kubelet's quota-block rounding on top.
        for assigned in [gi, 2 * gi, 3 * gi, 100 * gi] {
            for h in [DISK_HEADROOM_MIN, 1.5, DISK_HEADROOM_MAX] {
                let hard = overlay_size_limit_bytes(assigned, h);
                for slack in [0, KUBELET_QUOTA_BLOCK_SLACK] {
                    assert!(
                        CorroborationWitness::corroborated_sizing(
                            &claim(hard + slack, hard),
                            0,
                            assigned
                        )
                        .is_some(),
                        "minted product refused: assigned={assigned} h={h} slack={slack}"
                    );
                }
            }
            // Off-formula limits refuse in BOTH directions: the bare
            // disk identity (h=1.0 — below every mintable headroom)
            // and the wave-11-acceptable 3.9x fabrication.
            assert!(
                CorroborationWitness::corroborated_sizing(&claim(assigned, assigned), 0, assigned)
                    .is_none(),
                "the bare-disk identity is not a mintable hard limit"
            );
            let forged = assigned.saturating_mul(39) / 10;
            assert!(
                CorroborationWitness::corroborated_sizing(&claim(forged, forged), 0, assigned)
                    .is_none(),
                "a 3.9x fabricated limit must not move floors"
            );
            // The peak conjunct survives re-denomination: a minted
            // hard limit with a sub-half peak still refuses.
            let hard = overlay_size_limit_bytes(assigned, 1.5);
            assert!(
                CorroborationWitness::corroborated_sizing(&claim(hard, hard / 2 - 1), 0, assigned)
                    .is_none(),
                "peak below hard/2 must keep refusing"
            );
        }
        // Cold start (no assigned shape) still refuses everything.
        assert!(CorroborationWitness::corroborated_sizing(&claim(3 * gi, 3 * gi), 0, 0).is_none());
        // The kubelet NON-ENFORCING sentinel (-1 -> reads ~u64::MAX;
        // the vm-kubelet-projquota cells pin the producer side): a
        // sentinel-armed claim must never corroborate.
        assert!(
            CorroborationWitness::corroborated_sizing(&claim(u64::MAX, u64::MAX / 2), 0, 100 * gi)
                .is_none(),
            "the non-enforcing sentinel is not a mintable hard limit"
        );
    }

    const CEIL: Ceilings = Ceilings {
        max_cores: 64.0,
        max_mem: 256 << 30,
        max_disk: 200 << 30,
        default_disk: 20 << 30,
    };

    fn st() -> DerivationState {
        let row = crate::db::RecoveryDerivationRow::test_default("floor-t", "x86_64-linux");
        DerivationState::from_recovery_row(row, crate::state::DerivationStatus::Ready).unwrap()
    }

    fn intent(mem: u64, disk: u64, deadline: u32) -> crate::state::SolvedIntent {
        crate::state::SolvedIntent {
            mem_bytes: mem,
            disk_bytes: disk,
            deadline_secs: deadline,
            ..Default::default()
        }
    }

    #[test]
    fn oom_doubles_from_est_then_floor() {
        let mut s = st();
        s.sched.last_intent = Some(intent(4 << 30, 0, 0));
        let o = bump_floor_or_count(&mut s, TerminationReason::OomKilled, &CEIL);
        assert!(o.promoted && !o.at_cap);
        assert_eq!(s.sched.resource_floor.mem_bytes, 8 << 30);
        assert_eq!(s.retry.infra_count, 0);
        // Second bump: floor (8) > est (4) → base=8 → 16.
        let o = bump_floor_or_count(&mut s, TerminationReason::OomKilled, &CEIL);
        assert!(o.promoted && !o.at_cap);
        assert_eq!(s.sched.resource_floor.mem_bytes, 16 << 30);
    }

    #[test]
    fn at_ceiling_reports_at_cap_no_mutation() {
        // merged_bug_016: the mem cap is the SOLVE-domain cap
        // (`mem_solve_cap` = global − pad) — a floor at the raw
        // global is ABOVE it and heals down via the at-cap arm, so
        // the at-cap dispatch renders a hostable container.
        let mut s = st();
        s.sched.resource_floor.mem_bytes = mem_solve_cap(&CEIL);
        let o = bump_floor_or_count(&mut s, TerminationReason::OomKilled, &CEIL);
        assert!(!o.promoted && o.at_cap);
        // Helper does NOT mutate retry counters; caller owns that.
        assert_eq!(s.retry.infra_count, 0);
        assert_eq!(s.sched.resource_floor.mem_bytes, mem_solve_cap(&CEIL));
    }

    #[test]
    fn deadline_uses_24h_cap() {
        let mut s = st();
        s.sched.last_intent = Some(intent(0, 0, 3600));
        let o = bump_floor_or_count(&mut s, TerminationReason::DeadlineExceeded, &CEIL);
        assert!(o.promoted && !o.at_cap);
        assert_eq!(s.sched.resource_floor.deadline_secs, 7200);
        // At cap: at_cap=true, no counter mutation.
        s.sched.resource_floor.deadline_secs = DEADLINE_CAP_SECS;
        let o = bump_floor_or_count(&mut s, TerminationReason::DeadlineExceeded, &CEIL);
        assert!(!o.promoted && o.at_cap);
        assert_eq!(s.retry.timeout_count, 0, "helper never mutates counters");
        assert_eq!(s.retry.infra_count, 0);
    }

    #[test]
    fn last_intent_at_ceiling_is_at_cap_not_promoted_mem() {
        // floor=0, last_intent.mem == ceil.max_mem (SLA fit predicted
        // ≥ceiling, snapshot.rs:480 clamped). Next dispatch is `ceil`
        // again — no growth possible. Pre-fix: at_cap tested bare
        // `floor` (0), returned {promoted:true, at_cap:false} → callers
        // skipped retry-count++ → one uncounted max-size retry burned.
        let mut s = st();
        // merged_bug_016: the dispatch clamp pins at the SOLVE-domain
        // cap, so "dispatched at ceiling" means `mem_solve_cap`, and
        // the floor catch-up lands there too (a raw-global floor
        // would render an unhostable `global + pad` container).
        s.sched.last_intent = Some(intent(mem_solve_cap(&CEIL), 0, 0));
        let o = bump_floor_or_count(&mut s, TerminationReason::OomKilled, &CEIL);
        assert!(
            !o.promoted && o.at_cap,
            "dispatched at ceiling ⇒ no growth possible; got {o:?}"
        );
        assert_eq!(
            s.sched.resource_floor.mem_bytes,
            mem_solve_cap(&CEIL),
            "floor catches up to cap so persisted state starts at_cap"
        );
    }

    #[test]
    fn last_intent_at_ceiling_is_at_cap_not_promoted_disk() {
        let mut s = st();
        s.sched.last_intent = Some(intent(0, CEIL.max_disk, 0));
        let o = bump_floor_or_count(&mut s, TerminationReason::EvictedDiskPressure, &CEIL);
        assert!(!o.promoted && o.at_cap, "got {o:?}");
        assert_eq!(s.sched.resource_floor.disk_bytes, CEIL.max_disk);
    }

    #[test]
    fn last_intent_at_ceiling_is_at_cap_not_promoted_deadline() {
        let mut s = st();
        s.sched.last_intent = Some(intent(0, 0, DEADLINE_CAP_SECS));
        let o = bump_floor_or_count(&mut s, TerminationReason::DeadlineExceeded, &CEIL);
        assert!(!o.promoted && o.at_cap, "got {o:?}");
        assert_eq!(s.sched.resource_floor.deadline_secs, DEADLINE_CAP_SECS);
    }

    #[test]
    fn cold_start_zero_base_is_noop_not_promote() {
        // The COLD-START CORNER (live_040 re-scope): pre-first-mint is
        // still a real state — recovery hydrates nodes without
        // last_intent, and an OOM arriving before any mint has no
        // baseline to double from. last_intent=None, floor=0 → base=0
        // → next=0 → unchanged; {promoted:false, at_cap:false} →
        // caller's retry budget bounds it instead of looping at
        // floor=0. (This is the corner, NOT the steady state: the
        // mint stamps last_intent, so every post-mint OOM doubles —
        // see oom_floor_doubles_from_minted_intent.)
        let mut s = st();
        let o = bump_floor_or_count(&mut s, TerminationReason::OomKilled, &CEIL);
        assert!(!o.promoted && !o.at_cap);
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
            let p = ClampedFloor::of(&f, &ceil);
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
    /// live cap takes `bump_dim`'s at-cap arm (base ≥ cap → floor
    /// catches DOWN to cap, counted not promoted) — the in-memory heal
    /// the M_044 persist then writes (a no-op against the GREATEST
    /// ratchet, by design — see [`ClampedFloor`]).
    #[test]
    fn stale_floor_above_live_cap_heals_down_via_at_cap_arm() {
        let mut s = st();
        s.sched.resource_floor.mem_bytes = CEIL.max_mem * 4; // 383-era row
        let o = bump_floor_or_count(&mut s, TerminationReason::OomKilled, &CEIL);
        assert!(!o.promoted && o.at_cap);
        assert_eq!(
            s.sched.resource_floor.mem_bytes,
            mem_solve_cap(&CEIL),
            "the at-cap arm heals the in-memory floor down to the LIVE \
             cap — the SOLVE-domain cap since merged_bug_016 (a raw \
             global floor renders an unhostable container)"
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
    /// to at-cap, every step checked. STRAWMAN RED (order infeasible
    /// — commit 1 already moved the cap): with `cap = CEIL.max_mem`
    /// restored, the hostability assert fails at the at-cap step
    /// (`container(256 GiB) = 256 GiB + 256 MiB > 256 GiB`), and
    /// commit 1's floor-family re-derivations pinned the same
    /// boundary in their oracles.
    #[test]
    fn at_cap_terminal_reachable_in_bounded_steps_and_runnable() {
        let mut s = st();
        s.sched.last_intent = Some(intent(1 << 30, 0, 0));
        let mut steps = 0u32;
        loop {
            let o = bump_floor_or_count(&mut s, TerminationReason::OomKilled, &CEIL);
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
            if o.at_cap {
                break;
            }
            assert!(steps < 64, "doubling walk diverged");
        }
        // Hand-derived step oracle (not impl-derived): base starts at
        // the 1 GiB last_intent; doublings 1→2→4→…→128→clip at
        // cap' = 256 GiB − 256 MiB are 8 promoted steps, then step 9
        // reports at_cap. Bounded ⇒ the retry counter starts charging
        // at a known attempt number.
        assert_eq!(steps, 9, "bounded steps to the at-cap terminal");
        assert_eq!(
            s.sched.resource_floor.mem_bytes,
            mem_solve_cap(&CEIL),
            "the terminal state is the hostable solve-domain cap"
        );
    }

    // r[verify sched.sla.reactive-floor+5]
    /// **sh-012 D4 cores axis** — *proposition: an executor-variant
    /// exit≠0 with corroborated cpu_util ≥ threshold doubles
    /// floor.cores from the assigned shape; cpu_util ≪ threshold (the
    /// genuine compile-error exit shape) leaves it unchanged.* RED at
    /// base: `WitnessAxis::ComputeBound` does not exist.
    #[test]
    fn corroborated_compute_bound_band() {
        // cpu_util = 2280 / (600×4) = 0.95 ≥ 0.8 → corroborates.
        assert!(
            CorroborationWitness::corroborated_compute_bound(Some(2280.0), 4, 600, 0.8).is_some(),
            "cpu_util=0.95: a parallelism-exhausted build corroborates"
        );
        // cpu_util = 120 / (600×4) = 0.05 ≪ 0.8 → refuses (the
        // compile-error exit: died fast, near-zero cpu).
        assert!(
            CorroborationWitness::corroborated_compute_bound(Some(120.0), 4, 600, 0.8).is_none(),
            "cpu_util=0.05: a fast intrinsic exit≠0 refuses"
        );
        // Exactly at threshold: corroborates (closed lower bound).
        assert!(
            CorroborationWitness::corroborated_compute_bound(Some(1920.0), 4, 600, 0.8).is_some(),
            "cpu_util=0.80: the band is closed at threshold"
        );
        // None anchors refuse: missing telemetry / cold start.
        assert!(CorroborationWitness::corroborated_compute_bound(None, 4, 600, 0.8).is_none());
        assert!(
            CorroborationWitness::corroborated_compute_bound(Some(2280.0), 0, 600, 0.8).is_none()
        );
        assert!(
            CorroborationWitness::corroborated_compute_bound(Some(2280.0), 4, 0, 0.8).is_none()
        );
    }

    #[test]
    fn compute_bound_doubles_from_intent_then_floor() {
        let mut s = st();
        s.sched.last_intent = Some(crate::state::SolvedIntent {
            cores: 4,
            deadline_secs: 600,
            ..Default::default()
        });
        let o = bump_floor_or_count(&mut s, TerminationReason::ComputeBound, &CEIL);
        assert!(o.promoted && !o.at_cap);
        assert_eq!(s.sched.resource_floor.cores, 8);
        // Second bump: floor (8) > intent (4) → base=8 → 16.
        let o = bump_floor_or_count(&mut s, TerminationReason::ComputeBound, &CEIL);
        assert!(o.promoted && !o.at_cap);
        assert_eq!(s.sched.resource_floor.cores, 16);
        // At cap: max_cores=64.
        s.sched.resource_floor.cores = CEIL.max_cores as u32;
        let o = bump_floor_or_count(&mut s, TerminationReason::ComputeBound, &CEIL);
        assert!(!o.promoted && o.at_cap);
        assert_eq!(s.sched.resource_floor.cores, CEIL.max_cores as u32);
    }

    #[test]
    fn non_resource_reasons_noop() {
        let mut s = st();
        s.sched.last_intent = Some(intent(4 << 30, 0, 0));
        for r in [
            TerminationReason::Error,
            TerminationReason::Completed,
            TerminationReason::EvictedOther,
            TerminationReason::Unknown,
        ] {
            let o = bump_floor_or_count(&mut s, r, &CEIL);
            assert!(!o.promoted && !o.at_cap);
        }
        assert_eq!(s.sched.resource_floor, Default::default());
        assert_eq!(s.retry.infra_count, 0);
    }

    // r[verify sched.attempt.witnessed-terminal]
    /// live_058-b: the witnessed-reason x establish-disposition
    /// product census (the R25 proof obligation — the injectivity of
    /// the promotion set is COUNTED from the generated table, never
    /// asserted in prose). Membership is pinned through an exhaustive
    /// index match (the `GcPhase3Outcome::ALL` form), so a new wire
    /// letter cannot ship without joining `WITNESSED_LETTERS`, taking
    /// a `witnessed_disposition` row (rustc exhaustiveness), and
    /// filing its oracle row here.
    #[test]
    fn witnessed_disposition_product_census() {
        use WitnessedDisposition as D;
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

        // The product table — hand-written oracle rows, one per
        // letter, NOT derived from the impl's own match.
        let table = [
            (R::Unspecified, D::ClassifyOnly),
            (R::OomKilled, D::PromoteMemFloor),
            (R::EvictedDiskPressure, D::ClassifyOnly),
            (R::EvictedOther, D::ClassifyOnly),
            (R::Completed, D::ClassifyOnly),
            (R::Error, D::ClassifyOnly),
            (R::DeadlineExceeded, D::ClassifyOnly),
            (R::Cancelled, D::ClassifyOnly),
            (R::Preempted, D::ClassifyOnly),
            (R::Reaped, D::ClassifyOnly),
            (R::NoEligibleSource, D::ClassifyOnly),
        ];
        assert_eq!(table.len(), WITNESSED_LETTERS.len());
        for (letter, want) in table {
            assert_eq!(witnessed_disposition(letter), want, "letter={letter:?}");
        }

        // EXACTLY ONE promoting row — the quantifier, counted from
        // the generated set.
        assert_eq!(
            WITNESSED_LETTERS
                .iter()
                .filter(|r| witnessed_disposition(**r) == D::PromoteMemFloor)
                .count(),
            1,
            "witnessed-OomKilled is the ONLY promoting letter"
        );
    }
}
