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
}
