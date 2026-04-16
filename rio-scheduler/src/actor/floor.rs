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

// r[impl sched.sla.reactive-floor]
// r[impl sched.retry.promotion-exempt+2]
/// Double the relevant `resource_floor` dimension on an explicit
/// resource-exhaustion signal, or — if already at the cap — increment
/// the matching retry counter instead. Returns `true` iff the floor
/// changed (i.e. the next dispatch will be larger). Callers treat
/// `true` as "promotion exempt": no `retry_count`/`failure_count`
/// increment, no `failed_builders` insert.
///
/// `last_intent` (the doubling base) is `state.sched.est_*` —
/// snapshotted at dispatch time alongside `est_memory_bytes`. The
/// `max(floor, est)` form means a stale floor (lower than what was
/// actually dispatched) doesn't under-double; if both are zero (cold
/// start with the SLA gate skipped), the helper returns `false` so
/// the caller's retry budget bounds it.
///
/// `infra_count` for OOM/DiskPressure (mem/disk under-provision are
/// infrastructure failures); `timeout_count` for DeadlineExceeded
/// (separate budget per I-213). Never `retry_count` /
/// `failure_count` — those are build-determinism budgets.
pub fn bump_floor_or_count(
    state: &mut DerivationState,
    reason: TerminationReason,
    ceil: &Ceilings,
) -> bool {
    use TerminationReason as R;
    let floor = &mut state.sched.resource_floor;
    match reason {
        R::OomKilled => {
            if floor.mem_bytes >= ceil.max_mem {
                state.retry.infra_count += 1;
                false
            } else {
                let base = floor
                    .mem_bytes
                    .max(state.sched.est_memory_bytes.unwrap_or(0));
                let next = base.saturating_mul(2).min(ceil.max_mem);
                let changed = next > floor.mem_bytes;
                floor.mem_bytes = next;
                changed
            }
        }
        R::EvictedDiskPressure => {
            if floor.disk_bytes >= ceil.max_disk {
                state.retry.infra_count += 1;
                false
            } else {
                let base = floor
                    .disk_bytes
                    .max(state.sched.est_disk_bytes.unwrap_or(0));
                let next = base.saturating_mul(2).min(ceil.max_disk);
                let changed = next > floor.disk_bytes;
                floor.disk_bytes = next;
                changed
            }
        }
        R::DeadlineExceeded => {
            if floor.deadline_secs >= DEADLINE_CAP_SECS {
                state.retry.timeout_count += 1;
                false
            } else {
                let base = floor
                    .deadline_secs
                    .max(state.sched.est_deadline_secs.unwrap_or(0));
                let next = base.saturating_mul(2).min(DEADLINE_CAP_SECS);
                let changed = next > floor.deadline_secs;
                floor.deadline_secs = next;
                changed
            }
        }
        // Non-resource reasons (pod-kill, node failure, expected
        // one-shot exit, unclassified) are not sizing signals.
        R::EvictedOther | R::Completed | R::Error | R::Unknown => false,
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
    use crate::sla::solve::DEFAULT_CEILINGS;

    fn st() -> DerivationState {
        let row = crate::db::RecoveryDerivationRow::test_default("floor-t", "x86_64-linux");
        DerivationState::from_recovery_row(row, crate::state::DerivationStatus::Ready).unwrap()
    }

    #[test]
    fn oom_doubles_from_est_then_floor() {
        let mut s = st();
        s.sched.est_memory_bytes = Some(4 << 30);
        assert!(bump_floor_or_count(
            &mut s,
            TerminationReason::OomKilled,
            &DEFAULT_CEILINGS
        ));
        assert_eq!(s.sched.resource_floor.mem_bytes, 8 << 30);
        assert_eq!(s.retry.infra_count, 0);
        // Second bump: floor (8) > est (4) → base=8 → 16.
        assert!(bump_floor_or_count(
            &mut s,
            TerminationReason::OomKilled,
            &DEFAULT_CEILINGS
        ));
        assert_eq!(s.sched.resource_floor.mem_bytes, 16 << 30);
    }

    #[test]
    fn at_ceiling_increments_infra_not_floor() {
        let mut s = st();
        s.sched.resource_floor.mem_bytes = DEFAULT_CEILINGS.max_mem;
        assert!(!bump_floor_or_count(
            &mut s,
            TerminationReason::OomKilled,
            &DEFAULT_CEILINGS
        ));
        assert_eq!(s.retry.infra_count, 1);
        assert_eq!(s.sched.resource_floor.mem_bytes, DEFAULT_CEILINGS.max_mem);
    }

    #[test]
    fn deadline_uses_timeout_count_and_24h_cap() {
        let mut s = st();
        s.sched.est_deadline_secs = Some(3600);
        assert!(bump_floor_or_count(
            &mut s,
            TerminationReason::DeadlineExceeded,
            &DEFAULT_CEILINGS
        ));
        assert_eq!(s.sched.resource_floor.deadline_secs, 7200);
        assert_eq!(s.retry.timeout_count, 0, "below cap → no count");
        // At cap: timeout_count++, not infra_count.
        s.sched.resource_floor.deadline_secs = DEADLINE_CAP_SECS;
        assert!(!bump_floor_or_count(
            &mut s,
            TerminationReason::DeadlineExceeded,
            &DEFAULT_CEILINGS
        ));
        assert_eq!(s.retry.timeout_count, 1);
        assert_eq!(
            s.retry.infra_count, 0,
            "deadline uses timeout_count, NOT infra"
        );
    }

    #[test]
    fn cold_start_zero_base_is_noop_not_promote() {
        // est=None, floor=0 → base=0 → next=0 → unchanged → false.
        // Caller's retry budget bounds it instead of looping at floor=0.
        let mut s = st();
        assert!(!bump_floor_or_count(
            &mut s,
            TerminationReason::OomKilled,
            &DEFAULT_CEILINGS
        ));
        assert_eq!(s.sched.resource_floor.mem_bytes, 0);
        assert_eq!(s.retry.infra_count, 0, "below cap → not counted either");
    }

    #[test]
    fn non_resource_reasons_noop() {
        let mut s = st();
        s.sched.est_memory_bytes = Some(4 << 30);
        for r in [
            TerminationReason::Error,
            TerminationReason::Completed,
            TerminationReason::EvictedOther,
            TerminationReason::Unknown,
        ] {
            assert!(!bump_floor_or_count(&mut s, r, &DEFAULT_CEILINGS));
        }
        assert_eq!(s.sched.resource_floor, Default::default());
        assert_eq!(s.retry.infra_count, 0);
    }
}
