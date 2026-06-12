//! The bilateral log-ingest liveness contract (merged_bug_335 / §5-S
//! Q1, bughunt-4).
//!
//! One const pair, two sides of one law:
//!
//! - **Producer** (rio-builder): an uploader whose AppendLog session
//!   is open with an empty buffer emits an empty keepalive batch every
//!   [`UPLOADER_KEEPALIVE_PERIOD`]. Empty batches are sanctioned by the
//!   ingest layer as non-cut-masking (`ingest.rs::accept` returns the
//!   real size trigger for them), so the keepalive carries liveness
//!   and nothing else.
//! - **Enforcement** (rio-store): the AppendLog driver aborts a
//!   session whose buffer is empty and whose inbound stream has been
//!   silent for [`INBOUND_IDLE_ABORT`] — a vanished builder cannot
//!   renew an ingest lease forever; a conformant one is never aborted,
//!   because the producer side guarantees inbound traffic well inside
//!   the bound.
//!
//! The conformance test below is the machine witness binding the two
//! sides: `period × margin < abort`. A change that breaks the
//! relation — shortening the abort, lengthening the period — fails
//! the workspace test suite rather than shipping a contract the two
//! crates no longer agree on.
//!
//! Any AppendLog client that parks an open session with an empty
//! buffer owes keepalives within the bound — including test writers
//! that bypass the builder uploader (the round-3 dashboard scenario's
//! grpcurl writer is its own producer under this contract; its ~5 s
//! cadence satisfies the law by a wide margin).

use std::time::Duration;

/// Enforcement bound: the store aborts an empty-buffer AppendLog
/// session after this much inbound silence (counted,
/// `reason="inbound_idle"`). The store additionally pins this to
/// 4 × its PG heartbeat interval; see the asserting test at the
/// abort site.
pub const INBOUND_IDLE_ABORT: Duration = Duration::from_secs(60);

/// Producer cadence: an open, empty-buffer uploader session emits an
/// empty keepalive batch this often.
pub const UPLOADER_KEEPALIVE_PERIOD: Duration = Duration::from_secs(20);

/// Safety factor between the producer cadence and the enforcement
/// bound: one whole keepalive may be lost (or arbitrarily delayed by
/// scheduling) and the session still shows inbound traffic inside the
/// abort window.
pub const KEEPALIVE_SAFETY_MARGIN: u32 = 2;

/// The conformance predicate — exposed (not just tested) so a future
/// third participant in the contract can assert it against its own
/// cadence.
#[must_use]
pub const fn keepalive_conforms(period: Duration, margin: u32, abort: Duration) -> bool {
    // const-friendly: compare in nanoseconds via u128.
    period.as_nanos() * (margin as u128) < abort.as_nanos()
}

// ────────────────────────────────────────────────────────────────────
// The bilateral VerifyChunks progress-cadence contract
// (merged_bug_023, bughunt-4). Same shape as the AppendLog pair above:
// a producer-side emission guarantee, a client-side bound that CITES
// it, and a conformance test binding the two so neither crate can
// drift the contract alone.
// ────────────────────────────────────────────────────────────────────

/// Producer cadence (rio-store `VerifyChunks`): at most this many
/// chunk probes (S3 `HeadObject`s) run between two progress frames.
/// The store slices each PG batch into emission sub-batches of this
/// size — a max batch (5000 chunks) can no longer go frame-silent for
/// its whole sequential HeadObject sweep.
pub const ADMIN_VERIFY_EMIT_EVERY: usize = 256;

/// Probe concurrency inside one HeadObject wave (the S3 backend's
/// bounded fan-out). The backend asserts its local constant equals
/// this one; the worst-case emission gap below divides by it.
pub const ADMIN_VERIFY_HEAD_CONCURRENCY: usize = 16;

/// Engineering worst case for ONE 16-wide HeadObject wave against a
/// degraded S3 (SDK retries included). Deliberately pessimistic: the
/// p99 healthy wave is tens of milliseconds.
pub const ADMIN_VERIFY_WORST_WAVE: Duration = Duration::from_secs(5);

/// Enforcement side (rio-cli): the per-message inactivity bound the
/// CLI's drain law applies to admin audit streams. A producer that
/// honors [`ADMIN_VERIFY_EMIT_EVERY`] emits well inside this window
/// (conformance test below); a stream silent past it is presumed
/// half-open (peer died without FIN/RST on the CLI's keepalive-free
/// channel) and the drain exits PARTIAL instead of hanging on the
/// kernel's ~2 h TCP keepalive.
pub const ADMIN_STREAM_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(120);

/// The emission-gap arithmetic, parameterized (bug_151): the waves in
/// one emission sub-batch × the worst-case wave. One formula serves
/// the production const fold below AND the conformance tests'
/// non-integral counter-examples — a second hand-derivation of the
/// gap is the R33 shape this split avoids.
///
/// LOSSLESS in the finest unit any input const can legally carry
/// (R29′, the in-file [`RetryEnvelope::worst_case`] house idiom):
/// const millis end-to-end. The pre-fix form truncated the wave to
/// whole seconds BEFORE the multiply ("Duration × usize is not
/// const" — avoidable, as `worst_case` in this same file already
/// demonstrated), under-measuring a non-integral wave by up to
/// `waves` seconds, lossy exactly toward hiding a violation of the
/// bound the output is compared against.
#[must_use]
pub const fn worst_emission_gap(
    worst_wave: Duration,
    emit_every: usize,
    concurrency: usize,
) -> Duration {
    let waves = emit_every.div_ceil(concurrency);
    Duration::from_millis(worst_wave.as_millis() as u64 * waves as u64)
}

/// Worst-case wall time between two `VerifyChunks` progress frames
/// under the producer cadence. Exposed so the conformance test (and
/// any future bound consumer) derives it instead of re-computing it.
#[must_use]
pub const fn admin_verify_worst_emission_gap() -> Duration {
    worst_emission_gap(
        ADMIN_VERIFY_WORST_WAVE,
        ADMIN_VERIFY_EMIT_EVERY,
        ADMIN_VERIFY_HEAD_CONCURRENCY,
    )
}

/// A liveness wave budget — the `BoundedOp` half of the cadence
/// contract (bug_108). Mintable ONLY from the liveness consts (the
/// constructors below are the sole public mints), so the cadence
/// claim's const and its runtime enforcement site are one value: an
/// I/O wave that ignores the budget has no bare future left to await
/// at a conforming call site, and a budget minted from an unrelated
/// literal cannot typecheck as `WaveBudget`.
///
/// `ADMIN_VERIFY_WORST_WAVE` was previously an UNENFORCED engineering
/// estimate consumed only by const arithmetic
/// ([`admin_verify_worst_emission_gap`]): with no `TimeoutConfig` on
/// the shared S3 client (see `rio_common::s3::default_client`'s doc),
/// an established-then-black-holed connection awaited response
/// headers forever, the never-completing FIRST attempt defeated the
/// SDK retry layer, and one dead HEAD false-failed a healthy audit at
/// the client's 120 s inactivity bound with no resume cursor. The
/// combinator upgrades the const from estimate to enforced bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaveBudget(Duration);

/// The admin-verify wave budget: one `ADMIN_VERIFY_HEAD_CONCURRENCY`-
/// wide `HeadObject` wave must complete inside
/// [`ADMIN_VERIFY_WORST_WAVE`] — the same const the emission-gap
/// arithmetic above already prices.
#[must_use]
pub const fn admin_verify_wave_budget() -> WaveBudget {
    WaveBudget(ADMIN_VERIFY_WORST_WAVE)
}

/// Typed elapse of a [`WaveBudget`]-bounded operation. Carries the
/// budget so callers (and tests) can assert WHICH liveness const was
/// enforced; flows through `anyhow` into the existing admin
/// classification (the non-auth arm → `Status::unavailable`).
#[derive(Debug, thiserror::Error)]
#[error(
    "operation exceeded its liveness wave budget ({budget:?}) — the peer is \
     presumed black-holed (established but silent); see \
     rio_common::liveness::WaveBudget"
)]
pub struct WaveBudgetExceeded {
    /// The enforced budget (the liveness const the mint wrapped).
    pub budget: Duration,
}

impl WaveBudget {
    /// The bounded-op combinator: run `fut` under this budget, mapping
    /// elapse to the typed [`WaveBudgetExceeded`]. The future is
    /// dropped on elapse (in-flight SDK attempts are cancelled).
    ///
    /// # Errors
    /// [`WaveBudgetExceeded`] when `fut` does not complete inside the
    /// budget.
    pub async fn run<T>(
        self,
        fut: impl std::future::Future<Output = T>,
    ) -> Result<T, WaveBudgetExceeded> {
        tokio::time::timeout(self.0, fut)
            .await
            .map_err(|_| WaveBudgetExceeded { budget: self.0 })
    }

    /// The wrapped duration (for logs/assertions; the mint sites are
    /// the liveness constructors only).
    #[must_use]
    pub const fn duration(self) -> Duration {
        self.0
    }
}

/// A typed per-operation retry envelope (merged_bug_006): the complete
/// retry-layer shape — attempts × per-attempt timeout plus capped
/// exponential backoffs — as ONE value, so the SDK wire config and the
/// conformance arithmetic derive from the same source and cannot
/// drift apart (the R14 shared-value shape).
///
/// The defect class this closes: [`WaveBudget`] brackets a whole I/O
/// wave, but the shared S3 client deliberately carries NO
/// `TimeoutConfig` and an operator-tunable `max_attempts` (see
/// `rio_common::s3::default_client`). With no per-attempt bound BELOW
/// the retry layer, the retry envelope on a slow-but-alive peer is
/// unbounded — no finite wave budget can bracket it, so the budget
/// CANCELS lawful churn-recovery ladders (the exact recoveries the
/// raised attempt count exists for). The law: an enforced whole-op
/// deadline must DOMINATE the retry envelope it brackets — per-attempt
/// timeouts below the retry layer, whole-op budgets above it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryEnvelope {
    /// Total attempts (first try + retries), ≥ 1 — the SDK's
    /// `max_attempts`.
    pub attempts: u32,
    /// Per-attempt timeout (the SDK's `operation_attempt_timeout`).
    pub attempt_timeout: Duration,
    /// First between-attempt backoff; doubles per retry (the SDK's
    /// `initial_backoff`; jitter only shrinks it).
    pub initial_backoff: Duration,
    /// Cap on any single between-attempt backoff (`max_backoff`).
    pub max_backoff: Duration,
}

impl RetryEnvelope {
    /// Worst-case wall time of the whole envelope: every attempt
    /// consumes its full per-attempt timeout and every between-attempt
    /// gap its capped backoff —
    /// `attempts × attempt_timeout + Σᵢ min(initial_backoff·2ⁱ, max_backoff)`
    /// over the `attempts − 1` gaps. (SDK jitter MULTIPLIES backoffs
    /// by a factor in [0, 1], so this is an upper bound on the
    /// backoff schedule.) The const-asserted ordering law
    /// (`worst_case() ≤ ADMIN_VERIFY_WORST_WAVE`, R17) is what makes
    /// the wave budget a true BACKSTOP: the envelope exhausts first,
    /// and the budget fires only on shapes the retry layer itself
    /// cannot bound.
    #[must_use]
    pub const fn worst_case(self) -> Duration {
        let mut total_ms = self.attempt_timeout.as_millis() as u64 * self.attempts as u64;
        let mut backoff_ms = self.initial_backoff.as_millis() as u64;
        let max_ms = self.max_backoff.as_millis() as u64;
        let mut gap = 1;
        while gap < self.attempts {
            total_ms += if backoff_ms < max_ms {
                backoff_ms
            } else {
                max_ms
            };
            backoff_ms = backoff_ms.saturating_mul(2);
            gap += 1;
        }
        Duration::from_millis(total_ms)
    }
}

/// The VerifyChunks HeadObject lane's retry envelope, sized so
/// `worst_case()` fits INSIDE [`ADMIN_VERIFY_WORST_WAVE`] with
/// recorded headroom: 3 × 1200 ms + (100 ms + 200 ms) = 3.9 s ≤ 5 s
/// (22% headroom; the conformance test below asserts the relation, a
/// unit pins the arithmetic). Derivation: 3 attempts cover the
/// rustfs/MinIO churn shape — a recycled pooled connection plus one
/// more black-holed retry — while a fresh connection succeeds; 1200 ms
/// per attempt is ~10× the p99 healthy HEAD; short capped backoffs
/// because the failure mode is connection churn, not throttling. The
/// envelope deliberately DOES NOT derive from the client-wide
/// `s3_max_attempts` knob: that knob keeps governing the ops it was
/// raised for (puts/gets under churn), while this chokepoint owns its
/// own bound — drift-immunity by construction, not by assertion.
pub const ADMIN_VERIFY_HEAD_ENVELOPE: RetryEnvelope = RetryEnvelope {
    attempts: 3,
    attempt_timeout: Duration::from_millis(1200),
    initial_backoff: Duration::from_millis(100),
    max_backoff: Duration::from_millis(200),
};

#[cfg(test)]
mod tests {
    use super::*;

    /// The bilateral contract's machine witness: period × margin
    /// strictly inside the abort bound.
    #[test]
    fn keepalive_period_times_margin_is_inside_the_abort_bound() {
        assert!(
            keepalive_conforms(
                UPLOADER_KEEPALIVE_PERIOD,
                KEEPALIVE_SAFETY_MARGIN,
                INBOUND_IDLE_ABORT
            ),
            "UPLOADER_KEEPALIVE_PERIOD ({:?}) x {} must be < INBOUND_IDLE_ABORT ({:?}) — \
             a parked conformant uploader would otherwise be aborted mid-contract",
            UPLOADER_KEEPALIVE_PERIOD,
            KEEPALIVE_SAFETY_MARGIN,
            INBOUND_IDLE_ABORT
        );
    }

    /// Negative control (the planted red): a period at or past the
    /// bound is rejected by the predicate — the conformance test
    /// would catch a const change that breaks the contract.
    #[test]
    fn conformance_rejects_a_period_outside_the_bound() {
        assert!(!keepalive_conforms(
            Duration::from_secs(61),
            KEEPALIVE_SAFETY_MARGIN,
            INBOUND_IDLE_ABORT
        ));
        assert!(!keepalive_conforms(
            Duration::from_secs(30),
            2,
            Duration::from_secs(60)
        ));
    }

    /// The VerifyChunks bilateral contract's machine witness
    /// (merged_bug_023): the producer's worst-case emission gap fits
    /// strictly inside the client's inactivity bound. A change that
    /// breaks the relation — a bigger emission sub-batch, a smaller
    /// client bound, a slower assumed wave — fails the workspace
    /// suite rather than shipping a contract the two crates no
    /// longer agree on. The arithmetic is still true AND now
    /// enforced: `ADMIN_VERIFY_WORST_WAVE` is no longer an estimate —
    /// the store's exists_batch runs every HeadObject wave under
    /// [`WaveBudget::run`] (bug_108), and the paused-clock behavioral
    /// test there (`black_holed_head_fails_typed_within_the_wave_budget`)
    /// is the wiring witness this const-math test cannot be.
    #[test]
    fn verify_emission_gap_is_inside_the_client_bound() {
        let gap = admin_verify_worst_emission_gap();
        assert!(
            keepalive_conforms(gap, 1, ADMIN_STREAM_INACTIVITY_TIMEOUT),
            "worst emission gap ({gap:?}) must be < ADMIN_STREAM_INACTIVITY_TIMEOUT \
             ({ADMIN_STREAM_INACTIVITY_TIMEOUT:?}) — the client would kill healthy \
             max-batch verifies as half-open"
        );
        // Current values: 16 waves × 5000 ms = 80 000 ms vs 120 s —
        // 1.5× headroom over the engineering worst case. The pin is
        // re-derived from the LOSSLESS millis product (bug_151); the
        // value is unchanged only because the production wave is
        // integral-seconds — the non-integral domain is pinned by
        // `emission_gap_witness_sees_subsecond_violations`.
        assert_eq!(
            gap,
            Duration::from_millis(5000 * 16),
            "recompute the headroom note"
        );
    }

    /// Negative control: a cadence that fills the whole client window
    /// is rejected by the predicate this contract relies on.
    #[test]
    fn verify_conformance_rejects_a_window_filling_gap() {
        assert!(!keepalive_conforms(
            ADMIN_STREAM_INACTIVITY_TIMEOUT,
            1,
            ADMIN_STREAM_INACTIVITY_TIMEOUT
        ));
    }

    /// W12-AX (bug_151, R29′): witness arithmetic is LOSSLESS in the
    /// finest unit any input const can legally carry. The gap formula
    /// pre-fix truncated the wave to whole seconds BEFORE multiplying
    /// by the wave count — under-measuring up to ~16 s in exactly the
    /// direction that hides a contract violation. The counter-example
    /// the downcast hid: a 7900 ms wave computes a truncated
    /// 7 s × 16 = 112 s "gap" that passes the 120 s client bound,
    /// while the true worst gap is 126.4 s — a violating cadence
    /// certified green by the witness sworn to catch it. Latent today
    /// (the production 5 s wave is integral); the proposition is over
    /// the NON-INTEGRAL domain the formula's inputs can legally
    /// carry, driven through the same parameterized formula the
    /// production fold uses.
    ///
    /// Pre-fix red, verbatim (against the extracted-but-still-lossy
    /// formula):
    ///   a 7900 ms wave's true gap (126.4 s) violates the 120 s
    ///   bound — the witness must SEE it (lossy-toward-green
    ///   downcast hid it; computed 112 s)
    #[test]
    fn emission_gap_witness_sees_subsecond_violations() {
        let wave = Duration::from_millis(7900);
        let gap = worst_emission_gap(wave, ADMIN_VERIFY_EMIT_EVERY, ADMIN_VERIFY_HEAD_CONCURRENCY);
        assert!(
            !keepalive_conforms(gap, 1, ADMIN_STREAM_INACTIVITY_TIMEOUT),
            "a 7900 ms wave's true gap (126.4 s) violates the 120 s bound — \
             the witness must SEE it (lossy-toward-green downcast hid it; \
             computed {gap:?})"
        );
        // And the computed gap is the exact lossless product.
        assert_eq!(
            gap,
            Duration::from_millis(7900 * 16),
            "lossless in the finest unit the input carries"
        );
    }

    /// The NEW first link of the three-link chain (merged_bug_006,
    /// R17): `ADMIN_VERIFY_HEAD_ENVELOPE.worst_case()` ≤
    /// `ADMIN_VERIFY_WORST_WAVE` ≤ emission gap ≤ the 120 s client
    /// bound (links 2-3 asserted by
    /// `verify_emission_gap_is_inside_the_client_bound` above).
    /// Strawman disclosure (R16): this test cannot be red pre-fix —
    /// the envelope type did not exist; it is the STANDING drift
    /// gate, the same role the emission-gap test plays one link up.
    /// The behavioral red lives in rio-store
    /// (`churn_recovery_ladder_completes_inside_the_wave_budget`).
    #[test]
    fn head_retry_envelope_fits_inside_the_wave_budget() {
        let worst = ADMIN_VERIFY_HEAD_ENVELOPE.worst_case();
        assert!(
            worst <= ADMIN_VERIFY_WORST_WAVE,
            "the HEAD retry envelope's worst case ({worst:?}) must fit inside \
             ADMIN_VERIFY_WORST_WAVE ({ADMIN_VERIFY_WORST_WAVE:?}) — a budget \
             below its own retry envelope cancels lawful churn recoveries"
        );
    }

    /// Pins the worst-case arithmetic (and the headroom note in the
    /// const doc): 3 × 1200 ms + (100 + 200) ms = 3.9 s.
    #[test]
    fn head_retry_envelope_worst_case_arithmetic() {
        assert_eq!(
            ADMIN_VERIFY_HEAD_ENVELOPE.worst_case(),
            Duration::from_millis(3900),
            "recompute the headroom note in the const doc"
        );
        // The backoff cap binds: a 2-gap ladder pays initial then cap.
        let uncapped = RetryEnvelope {
            attempts: 3,
            attempt_timeout: Duration::from_millis(1000),
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(10_000),
        };
        assert_eq!(uncapped.worst_case(), Duration::from_millis(3300));
    }

    /// The budget-elapse direction's OWN witness (re-homed from the
    /// rio-store black-holed test, which post-fix exercises the
    /// envelope instead): a future that outlives the envelope's worst
    /// case is killed by the wave-budget BACKSTOP with the typed
    /// error carrying the enforced const.
    #[tokio::test(start_paused = true)]
    async fn wave_budget_backstop_fires_beyond_the_envelope() {
        let budget = admin_verify_wave_budget();
        let err = budget
            .run(std::future::pending::<()>())
            .await
            .expect_err("a never-resolving op must elapse the backstop");
        assert_eq!(
            err.budget, ADMIN_VERIFY_WORST_WAVE,
            "the elapse must carry the liveness const it enforced"
        );
    }
}
