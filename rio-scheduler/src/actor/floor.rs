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
/// (`r[sched.retry.promotion-exempt+2]`); `counted` lets callers that
/// ALSO own a fall-through increment (`handle_infrastructure_failure`
/// `:1906`) skip it — without this the at-cap cgroup-OOM path
/// double-counted (bump's at-cap increment + caller's generic-infra
/// increment) and poisoned at half the configured budget.
#[derive(Debug, Default, Clone, Copy)]
pub struct FloorOutcome {
    /// Floor changed; next dispatch will be larger. Promotion-exempt.
    pub promoted: bool,
    /// Floor was already at cap; the matching retry counter
    /// (`infra_count` for mem/disk, `timeout_count` for deadline) was
    /// incremented HERE. Caller MUST NOT increment again.
    pub counted: bool,
}

// r[impl sched.sla.reactive-floor]
// r[impl sched.retry.promotion-exempt+2]
/// Double the relevant `resource_floor` dimension on an explicit
/// resource-exhaustion signal, or — if already at the cap — increment
/// the matching retry counter instead. See [`FloorOutcome`].
///
/// The doubling base is `state.sched.last_intent` — snapshotted at
/// dispatch time. The `max(floor, last)` form means a stale floor
/// (lower than what was actually dispatched) doesn't under-double; if
/// both are zero (cold start, never dispatched), the helper returns
/// `{promoted:false, counted:false}` — the caller MUST increment its
/// own retry counter when `!counted` so cold-start is bounded (I-200:
/// `if promoted` here looped forever).
///
/// `infra_count` for OOM/DiskPressure (mem/disk under-provision are
/// infrastructure failures); `timeout_count` for DeadlineExceeded
/// (separate budget per I-213). Never `retry_count` /
/// `failure_count` — those are build-determinism budgets.
pub fn bump_floor_or_count(
    state: &mut DerivationState,
    reason: TerminationReason,
    ceil: &Ceilings,
) -> FloorOutcome {
    use TerminationReason as R;
    let floor = &mut state.sched.resource_floor;
    let last = state.sched.last_intent.as_ref();
    match reason {
        R::OomKilled => {
            if floor.mem_bytes >= ceil.max_mem {
                state.retry.infra_count += 1;
                FloorOutcome {
                    promoted: false,
                    counted: true,
                }
            } else {
                let base = floor.mem_bytes.max(last.map_or(0, |i| i.mem_bytes));
                let next = base.saturating_mul(2).min(ceil.max_mem);
                let changed = next > floor.mem_bytes;
                floor.mem_bytes = next;
                FloorOutcome {
                    promoted: changed,
                    counted: false,
                }
            }
        }
        R::EvictedDiskPressure => {
            if floor.disk_bytes >= ceil.max_disk {
                state.retry.infra_count += 1;
                FloorOutcome {
                    promoted: false,
                    counted: true,
                }
            } else {
                let base = floor.disk_bytes.max(last.map_or(0, |i| i.disk_bytes));
                let next = base.saturating_mul(2).min(ceil.max_disk);
                let changed = next > floor.disk_bytes;
                floor.disk_bytes = next;
                FloorOutcome {
                    promoted: changed,
                    counted: false,
                }
            }
        }
        R::DeadlineExceeded => {
            if floor.deadline_secs >= DEADLINE_CAP_SECS {
                state.retry.timeout_count += 1;
                FloorOutcome {
                    promoted: false,
                    counted: true,
                }
            } else {
                let base = floor.deadline_secs.max(last.map_or(0, |i| i.deadline_secs));
                let next = base.saturating_mul(2).min(DEADLINE_CAP_SECS);
                let changed = next > floor.deadline_secs;
                floor.deadline_secs = next;
                FloorOutcome {
                    promoted: changed,
                    counted: false,
                }
            }
        }
        // Non-resource reasons (pod-kill, node failure, expected
        // one-shot exit, unclassified) are not sizing signals.
        R::EvictedOther | R::Completed | R::Error | R::Unknown => FloorOutcome::default(),
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
        assert!(o.promoted && !o.counted);
        assert_eq!(s.sched.resource_floor.mem_bytes, 8 << 30);
        assert_eq!(s.retry.infra_count, 0);
        // Second bump: floor (8) > est (4) → base=8 → 16.
        let o = bump_floor_or_count(&mut s, TerminationReason::OomKilled, &CEIL);
        assert!(o.promoted && !o.counted);
        assert_eq!(s.sched.resource_floor.mem_bytes, 16 << 30);
    }

    #[test]
    fn at_ceiling_increments_infra_not_floor() {
        let mut s = st();
        s.sched.resource_floor.mem_bytes = CEIL.max_mem;
        let o = bump_floor_or_count(&mut s, TerminationReason::OomKilled, &CEIL);
        assert!(!o.promoted && o.counted);
        assert_eq!(s.retry.infra_count, 1);
        assert_eq!(s.sched.resource_floor.mem_bytes, CEIL.max_mem);
    }

    #[test]
    fn deadline_uses_timeout_count_and_24h_cap() {
        let mut s = st();
        s.sched.last_intent = Some(intent(0, 0, 3600));
        let o = bump_floor_or_count(&mut s, TerminationReason::DeadlineExceeded, &CEIL);
        assert!(o.promoted && !o.counted);
        assert_eq!(s.sched.resource_floor.deadline_secs, 7200);
        assert_eq!(s.retry.timeout_count, 0, "below cap → no count");
        // At cap: timeout_count++, not infra_count.
        s.sched.resource_floor.deadline_secs = DEADLINE_CAP_SECS;
        let o = bump_floor_or_count(&mut s, TerminationReason::DeadlineExceeded, &CEIL);
        assert!(!o.promoted && o.counted);
        assert_eq!(s.retry.timeout_count, 1);
        assert_eq!(
            s.retry.infra_count, 0,
            "deadline uses timeout_count, NOT infra"
        );
    }

    #[test]
    fn cold_start_zero_base_is_noop_not_promote() {
        // last_intent=None, floor=0 → base=0 → next=0 → unchanged.
        // {promoted:false, counted:false} → caller's retry budget
        // bounds it instead of looping at floor=0.
        let mut s = st();
        let o = bump_floor_or_count(&mut s, TerminationReason::OomKilled, &CEIL);
        assert!(!o.promoted && !o.counted);
        assert_eq!(s.sched.resource_floor.mem_bytes, 0);
        assert_eq!(s.retry.infra_count, 0, "below cap → not counted either");
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
            assert!(!o.promoted && !o.counted);
        }
        assert_eq!(s.sched.resource_floor, Default::default());
        assert_eq!(s.retry.infra_count, 0);
    }
}
