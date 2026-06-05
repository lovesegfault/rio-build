//! The read path's loss ledger: data-loss is counted per HOLE
//! IDENTITY, divergence per event — and this module is the only place
//! either counter is touched.
//!
//! merged_bug_164: `rio_store_log_read_data_loss_total` is an
//! alert-on-ANY-increment counter (its PrometheusRule pages), but the
//! read path used to increment it per VISIT — every re-read of one
//! already-known missing object re-paged, and recoverable
//! divergence-grade events (an overlong object whose claimed lines all
//! served) fed the same pager. The chokepoint split:
//!
//! - [`note_hole`] — the SOLE increment site for the loss counter,
//!   deduplicated by `(exec_id, s3_key)` in a bounded per-process
//!   ledger: one hole = one page, no matter how many readers hit it.
//! - [`note_divergence`] — the warn-severity
//!   `rio_store_log_read_divergence_total{kind}` family for
//!   served-anyway divergence (`overlong`, `short_object_covered`),
//!   excluded from the critical alert by NAME.
//!
//! The `log-cap-status-chokepoint` misc-check pins the loss counter's
//! name to this file (plus the describe/seed sites in `lib.rs`), so a
//! future read-path arm cannot re-introduce a per-visit increment
//! without failing CI.
//!
//! Per-process semantics, documented tradeoff: the ledger forgets on
//! replica restart, so one hole can page once per replica generation.
//! That is the acceptable corner of "page once" — the alternative (a
//! durable dedup table) adds a PG write to the read path's error lane.

use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::OnceLock;

use uuid::Uuid;

/// Bound on remembered hole identities. At 64 bytes per entry this is
/// ~512 KiB worst case; eviction is FIFO (oldest first sighting
/// forgotten first), so a pathological key churn degrades toward the
/// old per-visit behavior rather than growing without bound.
const LEDGER_CAP: usize = 8192;

/// The process-global hole ledger. A `OnceLock<Mutex<..>>` rather than
/// a field threaded through the serve stack: the dedup is a
/// process-wide observability property (N concurrent readers of one
/// missing object are ONE hole), not per-session state.
fn ledger() -> &'static Mutex<Ledger> {
    static LEDGER: OnceLock<Mutex<Ledger>> = OnceLock::new();
    LEDGER.get_or_init(|| Mutex::new(Ledger::default()))
}

#[derive(Default)]
struct Ledger {
    seen: HashSet<(Uuid, String)>,
    order: VecDeque<(Uuid, String)>,
}

impl Ledger {
    /// Insert; true iff this is the key's first sighting.
    fn first_sighting(&mut self, exec_id: Uuid, s3_key: &str) -> bool {
        if self.seen.contains(&(exec_id, s3_key.to_owned())) {
            return false;
        }
        if self.order.len() >= LEDGER_CAP
            && let Some(evicted) = self.order.pop_front()
        {
            self.seen.remove(&evicted);
        }
        self.seen.insert((exec_id, s3_key.to_owned()));
        self.order.push_back((exec_id, s3_key.to_owned()));
        true
    }
}

// r[impl store.log.loss-event-identity]
/// Record an unrecoverable hole in a stored build log. Increments
/// `rio_store_log_read_data_loss_total{reason}` exactly once per
/// `(exec_id, s3_key)` per process — the counter's contract is one
/// page per hole, not one page per visit. Returns whether this call
/// was the first sighting (tests pin the dedup through this).
///
/// `reason` MUST be a member of
/// [`crate::LOG_READ_LOSS_REASONS`] (`missing_object`, `short_object`)
/// — the parity test and the metric HELP enforce the alphabet.
pub(super) fn note_hole(exec_id: Uuid, s3_key: &str, reason: &'static str) -> bool {
    let first = ledger()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .first_sighting(exec_id, s3_key);
    if first {
        metrics::counter!(
            "rio_store_log_read_data_loss_total",
            "reason" => reason
        )
        .increment(1);
    }
    first
}

// r[impl store.log.loss-event-identity]
/// Record a served-anyway divergence between a chunk's manifest row
/// and its object: warn-severity, per EVENT (no dedup — the volume is
/// the signal), and excluded from the critical loss alert by name.
/// `kind` MUST be a member of [`crate::LOG_READ_DIVERGENCE_KINDS`]
/// (`overlong`, `short_object_covered`).
pub(super) fn note_divergence(kind: &'static str) {
    metrics::counter!(
        "rio_store_log_read_divergence_total",
        "kind" => kind
    )
    .increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hole-identity dedup (merged_bug_164's trio leg 3): N
    /// sightings of one `(exec_id, s3_key)` = one counter increment.
    /// `note_hole`'s return value is the structural pin — the metric
    /// increment is gated on it, and the misc-check pins the counter
    /// name to this module so no other increment path exists.
    #[test]
    fn hole_identity_counted_once() {
        let exec = Uuid::now_v7();
        assert!(note_hole(exec, "logs/k1", "missing_object"));
        assert!(!note_hole(exec, "logs/k1", "missing_object"));
        assert!(!note_hole(exec, "logs/k1", "missing_object"));
        // A different key under the same exec is a different hole.
        assert!(note_hole(exec, "logs/k2", "short_object"));
        // Same key under a different exec is a different hole.
        assert!(note_hole(Uuid::now_v7(), "logs/k1", "missing_object"));
    }

    /// The ledger bound: eviction is FIFO and bounded, so the dedup
    /// degrades (oldest forgotten) rather than growing without bound.
    #[test]
    fn ledger_bounded_fifo() {
        let exec = Uuid::now_v7();
        let first_key = "logs/bound-0".to_owned();
        assert!(note_hole(exec, &first_key, "missing_object"));
        for i in 1..=LEDGER_CAP {
            note_hole(exec, &format!("logs/bound-{i}"), "missing_object");
        }
        // first_key was evicted by the CAP+1'th insert: re-noting it is
        // a "first" sighting again.
        assert!(note_hole(exec, &first_key, "missing_object"));
    }
}
