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

/// Worst-case wall time between two `VerifyChunks` progress frames
/// under the producer cadence: the waves in one emission sub-batch ×
/// the worst-case wave. Exposed so the conformance test (and any
/// future bound consumer) derives it instead of re-computing it.
#[must_use]
pub const fn admin_verify_worst_emission_gap() -> Duration {
    let waves = ADMIN_VERIFY_EMIT_EVERY.div_ceil(ADMIN_VERIFY_HEAD_CONCURRENCY);
    // Duration × usize is not const; build from secs.
    Duration::from_secs(ADMIN_VERIFY_WORST_WAVE.as_secs() * waves as u64)
}

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
    /// longer agree on.
    #[test]
    fn verify_emission_gap_is_inside_the_client_bound() {
        let gap = admin_verify_worst_emission_gap();
        assert!(
            keepalive_conforms(gap, 1, ADMIN_STREAM_INACTIVITY_TIMEOUT),
            "worst emission gap ({gap:?}) must be < ADMIN_STREAM_INACTIVITY_TIMEOUT \
             ({ADMIN_STREAM_INACTIVITY_TIMEOUT:?}) — the client would kill healthy \
             max-batch verifies as half-open"
        );
        // Current values: 16 waves × 5 s = 80 s vs 120 s — 1.5×
        // headroom over the engineering worst case.
        assert_eq!(gap, Duration::from_secs(80), "recompute the headroom note");
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
}
