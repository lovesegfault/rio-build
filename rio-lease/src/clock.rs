//! The self-fence blind-time clock: suspend-aware time for the one
//! measurement in this crate that must keep counting while the host
//! sleeps.
//!
//! Property: the self-fence blind-time is measured on a clock that
//! ADVANCES ACROSS HOST SUSPEND, and the window is anchored at the
//! renew attempt's START (`RenewAnchor`, minted before the await), so
//! the property holds unconditionally — wherever the suspend lands.
//! With CLOCK_MONOTONIC (`Instant::now()`) a suspend straddling
//! [`SELF_FENCE_AFTER`](crate::SELF_FENCE_AFTER) resumed with the blind
//! window looking fresh — a zombie leader until the next failed
//! apiserver round-trip — while a standby legitimately stole at
//! STEAL_AFTER of real time mid-suspend. With CLOCK_BOOTTIME the first
//! post-resume tick-time fence check sees the full blind interval and
//! fences immediately; a suspend that straddles an IN-FLIGHT renew is
//! covered too, because the post-resume response stamps the attempt's
//! pre-suspend anchor, never its own arrival time (the in-flight
//! straddle that response anchoring left open is closed by
//! construction — `BlindClock` has no API for a post-response stamp).
//!
//! Residuals (deliberately NOT chased here; the generation fence
//! r\[sched.lease.generation-fence+3\] is the backstop): hypervisor-level
//! VM pause is invisible to CLOCK_BOOTTIME too; long stop-the-world
//! stalls pause the loop, not the clock; and gRPC handlers can run in
//! the resume-to-first-tick gap before the fence check fires.
//!
//! Scope: ONLY the fence blind-time moves to this clock. The standby
//! steal clock (`Observed.at`, election.rs) stays monotonic — under
//! standby suspend it errs toward stealing LATER, which preserves
//! NeverDual (the proof needs steal strictly after fence; delaying steal
//! widens the separation). `became_leader_at` stays monotonic — suspend
//! makes `leader_for()` UNDER-read, which keeps the controller's
//! `orphan_reap_gate` fail-closed longer. Both err conservative.

use std::time::Duration;

/// Duration since an arbitrary fixed epoch on a clock that ADVANCES
/// ACROSS HOST SUSPEND. Linux: `clock_gettime(CLOCK_BOOTTIME)`.
///
/// Returns a `Duration` (not an `Instant`-like wrapper) deliberately:
/// the lease loop only ever subtracts two readings, so the epoch is
/// irrelevant and no ordering API is needed.
#[cfg(target_os = "linux")]
pub(crate) fn suspend_aware_now() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid exclusively-owned timespec; CLOCK_BOOTTIME
    // is a valid clockid on every supported kernel (>= 2.6.39). The only
    // documented failures are EFAULT (impossible: valid pointer) and
    // EINVAL (impossible: valid constant clockid).
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    // assert!, not debug_assert!: a silent rc != 0 would zero the reading
    // and silently disarm the fence — worse than the (unreachable) panic.
    assert_eq!(
        rc, 0,
        "clock_gettime(CLOCK_BOOTTIME) cannot fail on Linux >= 2.6.39"
    );
    #[allow(clippy::cast_sign_loss)] // tv_sec clamped non-negative; tv_nsec < 1e9 by contract
    Duration::new(ts.tv_sec.max(0) as u64, ts.tv_nsec as u32)
}

/// Best-effort fallback: process-anchored std monotonic time. Suspend is
/// NOT counted here — the pre-change behavior. Production deploys are
/// Linux-only (k8s nodes, NixOS VM tests); this arm exists so the crate
/// compiles on dev macOS. The first call returns ~zero; only differences
/// are ever consumed, so the epoch is irrelevant.
#[cfg(not(target_os = "linux"))]
pub(crate) fn suspend_aware_now() -> Duration {
    use std::sync::OnceLock;
    static ANCHOR: OnceLock<std::time::Instant> = OnceLock::new();
    ANCHOR.get_or_init(std::time::Instant::now).elapsed()
}

#[cfg(test)]
mod tests {
    use super::suspend_aware_now;

    /// The clock strictly advances across a real sleep. No upper bound
    /// asserted — immune to the "wall-clock gate under load" flake class
    /// (.claude/rules/ci-failure-patterns.md).
    #[test]
    fn suspend_aware_now_advances() {
        let b0 = suspend_aware_now();
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(
            suspend_aware_now() > b0,
            "suspend-aware clock must strictly advance across a 10ms sleep"
        );
    }

    /// BOOTTIME counts a superset of MONOTONIC's time. Sandwich ordering
    /// (boot sample before mono at the start, mono before boot at the
    /// end) makes the boot interval a strict superset of the mono
    /// interval — structurally exact, no epsilon, true under arbitrary
    /// load or even a real suspend mid-test.
    #[test]
    fn suspend_aware_at_least_monotonic_over_sandwiched_interval() {
        let b0 = suspend_aware_now();
        let m0 = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let m_el = m0.elapsed();
        let b_el = suspend_aware_now() - b0;
        assert!(
            b_el >= m_el,
            "boot-clock interval ({b_el:?}) must cover the sandwiched monotonic interval ({m_el:?})"
        );
    }
}
