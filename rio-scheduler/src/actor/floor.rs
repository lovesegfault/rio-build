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

// r[impl sched.sla.reactive-floor+4]
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
        R::OomKilled => bump_dim(
            &mut floor.mem_bytes,
            last.map_or(0, |i| i.mem_bytes),
            ceil.max_mem,
        ),
        // LIVE with exactly ONE producer (live_057-b): the worker
        // quota-attributed DiskFull lane — completion.rs matches
        // `rio_proto::DISK_FULL_MSG` on an InfrastructureFailure
        // report and calls `bump_resource_floor(EvictedDiskPressure,
        // "disk_full")`; precise by construction at the prjquota seam
        // (`apply_disk_override` — the prjquota-vs-statvfs
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
        // Non-resource reasons (pod-kill, node failure, expected
        // one-shot exit, unclassified) are not sizing signals.
        R::EvictedOther | R::Completed | R::Error | R::Unknown => FloorOutcome::default(),
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
        // Classify-only BY RULING: the controller folds node-condition
        // eviction shapes ("DiskPressure", "ephemeral-storage") AND
        // pod-attributed shapes ("ephemeral local storage", "EmptyDir
        // volume") into this ONE letter (pool/job.rs
        // pod_termination_reason) — promotion here would re-create
        // the I-199 ambient over-fire on the disk axis. The disk
        // floor's designed producer is the worker-side
        // quota-attributed lane (see the parked arm above).
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
}

impl ClampedFloor {
    /// Project `floor` under the live `ceil` — the ONLY constructor,
    /// so a consumer holding a `ClampedFloor` holds clamped values by
    /// type (the read-consume sites take this projection, never the
    /// raw floor; the census in sla_contract.rs pins the membership).
    pub(super) fn of(floor: &crate::state::ResourceFloor, ceil: &Ceilings) -> Self {
        Self {
            mem_bytes: floor.mem_bytes.min(ceil.max_mem),
            disk_bytes: floor.disk_bytes.min(ceil.max_disk),
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
    f.mem_bytes = f.mem_bytes.min(ceil.max_mem);
    f.disk_bytes = f.disk_bytes.min(ceil.max_disk);
    f.deadline_secs = f.deadline_secs.min(DEADLINE_CAP_SECS);
}

/// Per-dimension doubling shared by the three resource arms above.
///
/// `base = max(floor, last)` is what was actually dispatched
/// (`snapshot.rs` clamps `solve` output at `floor.max(...).min(ceil)`).
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
        let mut s = st();
        s.sched.resource_floor.mem_bytes = CEIL.max_mem;
        let o = bump_floor_or_count(&mut s, TerminationReason::OomKilled, &CEIL);
        assert!(!o.promoted && o.at_cap);
        // Helper does NOT mutate retry counters; caller owns that.
        assert_eq!(s.retry.infra_count, 0);
        assert_eq!(s.sched.resource_floor.mem_bytes, CEIL.max_mem);
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
        s.sched.last_intent = Some(intent(CEIL.max_mem, 0, 0));
        let o = bump_floor_or_count(&mut s, TerminationReason::OomKilled, &CEIL);
        assert!(
            !o.promoted && o.at_cap,
            "dispatched at ceiling ⇒ no growth possible; got {o:?}"
        );
        assert_eq!(
            s.sched.resource_floor.mem_bytes, CEIL.max_mem,
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
        // (input, live-cap, expected) per dimension — hand-written
        // oracle rows, NOT derived from the impl's own min().
        let rows: &[(u64, u64, u64)] = &[
            (0, 100, 0),             // zero stays zero
            (50, 100, 50),           // fresh below: untouched
            (100, 100, 100),         // at live cap: untouched
            (3_072 << 30, 100, 100), // stale above (383-era): clamped
        ];
        for &(input, cap, want) in rows {
            // Read half: the projection.
            let f = crate::state::ResourceFloor {
                mem_bytes: input,
                disk_bytes: input,
                deadline_secs: 60,
            };
            let ceil = Ceilings {
                max_cores: 64.0,
                max_mem: cap,
                max_disk: cap,
                default_disk: 1,
            };
            let p = ClampedFloor::of(&f, &ceil);
            assert_eq!(p.mem_bytes, want, "mem projection of {input} at cap {cap}");
            assert_eq!(
                p.disk_bytes, want,
                "disk projection of {input} at cap {cap}"
            );
            // Hydrate half: the in-place clamp.
            let mut g = f;
            clamp_floor_to_live(&mut g, &ceil);
            assert_eq!(
                g.mem_bytes, want,
                "mem hydrate-clamp of {input} at cap {cap}"
            );
            assert_eq!(
                g.disk_bytes, want,
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
        };
        clamp_floor_to_live(&mut g, &CEIL);
        assert_eq!(g.deadline_secs, DEADLINE_CAP_SECS);
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
            s.sched.resource_floor.mem_bytes, CEIL.max_mem,
            "the at-cap arm heals the in-memory floor down to the LIVE cap"
        );
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
