//! The idle clock: how long has the scheduler been *telling* this pod
//! "wanted but not deliverable"?
//!
//! The class this closes (merged_bug_209): measuring idleness as
//! wall-clock since the first `NotYetReady` counts scheduler-outage
//! time — a 5-minute leader failover between two `NotYetReady` answers
//! used to mature the whole spawned cohort past `idle_secs` at once,
//! and the first post-recovery answer triggered a cohort-wide exit 0
//! right as the queue became servable again.
//!
//! [`IdleClock`] makes unanswered time unrepresentable in the
//! accumulator by construction: the only place the accumulator
//! advances is [`IdleClock::on_answer`], and the increment is the gap
//! since the *previous answer*, capped at twice that answer's
//! suggested re-pull delay (covers the ±20 % pacing jitter plus RPC
//! latency). Transport errors and empty outcomes call
//! [`IdleClock::on_non_answer`], which pauses the clock — the next
//! answer re-seeds the pair without crediting the outage gap.

use std::time::Duration;

use tokio::time::Instant;

/// Accumulates only answer-adjacent "told not deliverable" intervals.
#[derive(Debug, Default)]
pub(super) struct IdleClock {
    accumulated: Duration,
    /// The previous `NotYetReady` answer: when it arrived and what
    /// re-pull delay it suggested. `None` until the first answer and
    /// after every non-answer (transport error / empty outcome).
    last: Option<(Instant, Duration)>,
}

impl IdleClock {
    /// A `NotYetReady` answer arrived at `now`, suggesting `suggested`
    /// as the next re-pull delay. Credits the gap since the previous
    /// answer (capped at 2× that answer's suggestion) and arms the
    /// next interval.
    pub(super) fn on_answer(&mut self, now: Instant, suggested: Duration) {
        if let Some((prev, prev_suggested)) = self.last {
            self.accumulated += now.duration_since(prev).min(prev_suggested * 2);
        }
        self.last = Some((now, suggested));
    }

    /// A non-answer (transport error, empty outcome): pause. The
    /// elapsed outage gap will not be credited by the next answer.
    pub(super) fn on_non_answer(&mut self) {
        self.last = None;
    }

    /// Total answered idle time.
    pub(super) fn idle_for(&self) -> Duration {
        self.accumulated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// The accumulator is exactly the sum of answer-adjacent gaps,
    /// each capped at 2× the previous answer's suggestion — and
    /// non-answers contribute nothing regardless of how long they
    /// take.
    #[test]
    fn idle_clock_sums_capped_answer_adjacent_gaps() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let mut clock = IdleClock::default();
            let suggested = Duration::from_secs(5);

            clock.on_answer(Instant::now(), suggested);
            assert_eq!(clock.idle_for(), Duration::ZERO, "first answer seeds only");

            // A paced answer 5s later credits the full 5s.
            tokio::time::advance(Duration::from_secs(5)).await;
            clock.on_answer(Instant::now(), suggested);
            assert_eq!(clock.idle_for(), Duration::from_secs(5));

            // A late answer (scheduler hiccup, 60s) is capped at 2×5s.
            tokio::time::advance(Duration::from_secs(60)).await;
            clock.on_answer(Instant::now(), suggested);
            assert_eq!(clock.idle_for(), Duration::from_secs(15));

            // An outage: errors pause; the post-outage answer re-seeds
            // without crediting the 300s gap.
            clock.on_non_answer();
            tokio::time::advance(Duration::from_secs(300)).await;
            clock.on_answer(Instant::now(), suggested);
            assert_eq!(clock.idle_for(), Duration::from_secs(15));
        });
    }

    proptest! {
        /// idle_for == Σ min(gap, 2×prev_suggested) over answer-adjacent
        /// pairs; intervals interrupted by a non-answer never count.
        /// (Kani none-sensible: rio-builder is a bin crate — kani.nix
        /// covers lib crates only; this is 30 lines of pure arithmetic
        /// and the proptest is the recorded coverage.)
        #[test]
        fn idle_for_equals_capped_pair_sum(
            // (gap_ms, suggested_ms, answered) script, bounded to keep
            // Instant arithmetic well clear of overflow.
            script in proptest::collection::vec(
                (1u64..600_000, 1u64..60_000, proptest::bool::ANY), 0..40)
        ) {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .start_paused(true)
                .build()
                .expect("runtime");
            runtime.block_on(async {
                let mut clock = IdleClock::default();
                let mut expected = Duration::ZERO;
                let mut prev: Option<(Instant, Duration)> = None;
                for (gap_ms, suggested_ms, answered) in script {
                    tokio::time::advance(Duration::from_millis(gap_ms)).await;
                    if answered {
                        let now = Instant::now();
                        let suggested = Duration::from_millis(suggested_ms);
                        if let Some((p, ps)) = prev {
                            expected += now.duration_since(p).min(ps * 2);
                        }
                        clock.on_answer(now, suggested);
                        prev = Some((now, suggested));
                    } else {
                        clock.on_non_answer();
                        prev = None;
                    }
                    prop_assert_eq!(clock.idle_for(), expected);
                }
                Ok(())
            })?;
        }
    }
}
