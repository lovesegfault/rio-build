//! The idle clock: how long has the scheduler been *telling* this pod
//! "wanted but not deliverable"?
//!
//! Two failure polarities bound this design, one per bug:
//!
//! - **Over-count** (merged_bug_209): measuring idleness as wall-clock
//!   since the first `NotYetReady` counts scheduler-outage time — a
//!   5-minute leader failover between two answers used to mature the
//!   whole spawned cohort past `idle_secs` at once, and the first
//!   post-recovery answer triggered a cohort-wide exit 0 right as the
//!   queue became servable again. The CAP is the close: the only place
//!   the accumulator advances is [`IdleClock::on_answer`], and the
//!   increment is the gap since the *previous answer*, capped at twice
//!   that answer's suggested re-pull delay (covers the ±20 % pacing
//!   jitter plus RPC latency). A 300s failover between two answers
//!   credits at most 2× the previous suggestion (~10s) — the cap alone
//!   is the sufficient outage bound.
//! - **Starvation** (bug_296): the original design ALSO discarded the
//!   armed answer pair on every transport error or empty outcome — an
//!   over-correction that made legitimately-idle time uncountable
//!   under interleaved errors: a flaky-but-answering scheduler kept
//!   `idle_for` at zero forever, so `idle_timeout` was unreachable.
//!   That pair-discard API is DELETED (`builder.pull.idle-undroppable`):
//!   the armed pair survives everything except the next answer, so the
//!   starvation polarity is unrepresentable — no API can drop an armed
//!   pair, and the cap already bounds whatever gap the next answer
//!   credits.
//!
//! Threat model in one line: time only accumulates between CONSECUTIVE
//! answers, at most 2× the pace the scheduler itself suggested —
//! outages are bounded by the cap, errors are invisible, and only the
//! scheduler's own answers can mature a pod toward its idle exit.

use std::time::Duration;

use tokio::time::Instant;

/// Accumulates only answer-adjacent "told not deliverable" intervals.
#[derive(Debug, Default)]
pub(super) struct IdleClock {
    accumulated: Duration,
    /// The previous `NotYetReady` answer: when it arrived and what
    /// re-pull delay it suggested. `None` only until the first answer
    /// — the armed pair is undroppable thereafter (bug_296: a
    /// pair-discard on errors starved the accumulator); the cap in
    /// [`Self::on_answer`] bounds whatever gap the next answer
    /// credits.
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

            // An outage: a 300s gap (leader failover, transport
            // errors — the clock cannot tell and does not care) is
            // credited at the CAP, not at face value: the
            // merged_bug_209 over-count close, now carried by the cap
            // alone.
            tokio::time::advance(Duration::from_secs(300)).await;
            clock.on_answer(Instant::now(), suggested);
            assert_eq!(clock.idle_for(), Duration::from_secs(25));
        });
    }

    // r[verify builder.pull.idle-undroppable]
    /// bug_296 red: interleaved transport errors must not starve the
    /// accumulator. A scheduler that answers NotYetReady every 5s
    /// through a flaky transport (one error between every pair of
    /// answers) is TELLING this pod "wanted but not deliverable" the
    /// whole time — yet the pre-fix pair-discard zeroed the armed
    /// state on every error, so idle_for stayed 0 forever and
    /// idle_timeout became unreachable: the over-count fix inverted
    /// into a starvation polarity.
    #[test]
    fn error_interleaved_answers_still_accumulate_idle() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let mut clock = IdleClock::default();
            let suggested = Duration::from_secs(5);
            clock.on_answer(Instant::now(), suggested);
            for _ in 0..6 {
                // Pre-fix, a transport error here called the (now
                // deleted) pair-discard; the type no longer has any
                // operation an error path could call — the errors are
                // structurally invisible to the accumulator.
                tokio::time::advance(Duration::from_secs(5)).await;
                clock.on_answer(Instant::now(), suggested);
            }
            assert_eq!(
                clock.idle_for(),
                Duration::from_secs(30),
                "six paced 5s answer gaps (each under the 2x cap) must \
                 accumulate 30s of answered idle time regardless of the \
                 errors interleaved between them"
            );
        });
    }

    proptest! {
        // r[verify builder.pull.idle-undroppable]
        /// Independent oracle (bug_296 replaced the impl-mirroring
        /// pair-sum check, which restated the implementation line for
        /// line and could not have caught a polarity inversion):
        /// four properties each computed by a DIFFERENT route than the
        /// accumulator —
        /// 1. cap bound: idle_for ≤ Σ 2×suggestedᵢ over all answers
        ///    but the last (each pair credits at most the cap);
        /// 2. wall bound: idle_for ≤ elapsed between first and last
        ///    answer (no pair credits more than real time);
        /// 3. monotone: idle_for never decreases;
        /// 4. paced exactness: when EVERY inter-answer gap is within
        ///    its cap, idle_for == last−first exactly (computed from
        ///    the endpoints, not the pairs — a starved accumulator
        ///    fails this without the oracle re-deriving the sum).
        /// (Kani none-sensible: rio-builder is a bin crate — kani.nix
        /// covers lib crates only; this proptest is the recorded
        /// coverage.)
        #[test]
        fn idle_for_independent_oracle(
            script in proptest::collection::vec(
                (1u64..600_000, 1u64..60_000), 1..40)
        ) {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .start_paused(true)
                .build()
                .expect("runtime");
            runtime.block_on(async {
                let mut clock = IdleClock::default();
                let mut cap_budget = Duration::ZERO;
                let mut prev_suggested: Option<Duration> = None;
                let mut first_at: Option<Instant> = None;
                let mut last_at: Option<Instant> = None;
                let mut all_paced = true;
                let mut last_idle = Duration::ZERO;
                for (gap_ms, suggested_ms) in script {
                    tokio::time::advance(Duration::from_millis(gap_ms)).await;
                    let now = Instant::now();
                    let suggested = Duration::from_millis(suggested_ms);
                    if let (Some(prev), Some(ps)) = (last_at, prev_suggested) {
                        cap_budget += ps * 2;
                        if now.duration_since(prev) > ps * 2 {
                            all_paced = false;
                        }
                    }
                    clock.on_answer(now, suggested);
                    prop_assert!(
                        clock.idle_for() >= last_idle,
                        "idle_for must be monotone"
                    );
                    last_idle = clock.idle_for();
                    first_at.get_or_insert(now);
                    last_at = Some(now);
                    prev_suggested = Some(suggested);
                }
                let wall = last_at
                    .zip(first_at)
                    .map_or(Duration::ZERO, |(l, f)| l.duration_since(f));
                prop_assert!(clock.idle_for() <= cap_budget, "cap bound");
                prop_assert!(clock.idle_for() <= wall, "wall bound");
                if all_paced {
                    prop_assert_eq!(
                        clock.idle_for(), wall,
                        "paced scripts accumulate exactly the endpoint \
                         elapsed — a starved accumulator fails here"
                    );
                }
                Ok(())
            })?;
        }
    }
}
