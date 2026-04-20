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

// r[impl sched.sla.reactive-floor+2]
// r[impl sched.retry.promotion-exempt+2]
/// Double the relevant `resource_floor` dimension on an explicit
/// resource-exhaustion signal, or — if already at the cap — report
/// `at_cap=true` so the caller's retry counter bounds it. See
/// [`FloorOutcome`]. No retry counters are mutated here; the CALLER
/// increments after its cap check so at-cap and non-floor failures
/// poison at the same attempt number.
///
/// The doubling base is `state.sched.last_intent` — snapshotted at
/// dispatch time. The `max(floor, last)` form means a stale floor
/// (lower than what was actually dispatched) doesn't under-double; if
/// both are zero (cold start, never dispatched), the helper returns
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

/// Metric-label form of `reason` for the cases that bump. `None` for
/// non-resource reasons (counter not emitted).
pub(super) fn reason_label(reason: TerminationReason) -> Option<&'static str> {
    use TerminationReason as R;
    match reason {
        R::OomKilled => Some("oom_killed"),
        R::EvictedDiskPressure => Some("disk_pressure"),
        R::DeadlineExceeded => Some("deadline_exceeded"),
        R::EvictedOther | R::Completed | R::Error | R::Unknown => None,
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
        // last_intent=None, floor=0 → base=0 → next=0 → unchanged.
        // {promoted:false, at_cap:false} → caller's retry budget
        // bounds it instead of looping at floor=0.
        let mut s = st();
        let o = bump_floor_or_count(&mut s, TerminationReason::OomKilled, &CEIL);
        assert!(!o.promoted && !o.at_cap);
        assert_eq!(s.sched.resource_floor.mem_bytes, 0);
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
}
