//! The read path's anomaly ledger: BOTH read-path anomaly counters —
//! data-loss and divergence — count per ANOMALY IDENTITY
//! `(family, exec_id, s3_key)`, and this module is the only place
//! either is touched.
//!
//! merged_bug_164: `rio_store_log_read_data_loss_total` is an
//! alert-on-ANY-increment counter (its PrometheusRule pages), but the
//! read path used to increment it per VISIT — every re-read of one
//! already-known missing object re-paged, and recoverable
//! divergence-grade events (an overlong object whose claimed lines all
//! served) fed the same pager.
//!
//! bug_206: the divergence family then repeated the same defect one
//! alert over — `note_divergence` counted per reader visit while the
//! wave-2 `RioLogReadDivergence` alert (`sum(increase(..[1h])) > 10`)
//! reads it as incidence, so ONE benign divergent chunk watched
//! ≥11×/hour kept the warning firing for the chunk's TTL, and the
//! counter could not distinguish 1 chunk × 1000 visits from 1000
//! chunks × 1 visit. The chokepoint is now ONE identity-keyed ledger
//! for every read-path anomaly counter (the kind/reason stays a metric
//! label; the FAMILY is part of the ledger key, so a hole and a
//! divergence on the same object are distinct anomalies):
//!
//! - [`note_hole`] — the SOLE increment site for the loss counter:
//!   one hole = one page, no matter how many readers hit it.
//! - [`note_divergence`] — the SOLE increment site for the
//!   warn-severity `rio_store_log_read_divergence_total{kind}` family
//!   (`overlong`, `short_object_covered`, `missing_object_covered`),
//!   excluded from the critical alert by NAME: one divergent object =
//!   one trend tick.
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

/// The anomaly family — part of the ledger KEY (bug_206): a hole and
/// a divergence on the same object are distinct anomalies; deduping
/// one must never suppress the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum AnomalyFamily {
    Loss,
    Divergence,
}

#[derive(Default)]
struct Ledger {
    seen: HashSet<(AnomalyFamily, Uuid, String)>,
    order: VecDeque<(AnomalyFamily, Uuid, String)>,
}

impl Ledger {
    /// Insert; true iff this is the key's first sighting.
    fn first_sighting(&mut self, family: AnomalyFamily, exec_id: Uuid, s3_key: &str) -> bool {
        if self.seen.contains(&(family, exec_id, s3_key.to_owned())) {
            return false;
        }
        if self.order.len() >= LEDGER_CAP
            && let Some(evicted) = self.order.pop_front()
        {
            self.seen.remove(&evicted);
        }
        self.seen.insert((family, exec_id, s3_key.to_owned()));
        self.order.push_back((family, exec_id, s3_key.to_owned()));
        true
    }
}

// r[impl store.log.loss-event-identity+1]
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
        .first_sighting(AnomalyFamily::Loss, exec_id, s3_key);
    if first {
        metrics::counter!(
            "rio_store_log_read_data_loss_total",
            "reason" => reason
        )
        .increment(1);
    }
    first
}

// r[impl store.log.loss-event-identity+1]
/// Record a served-anyway divergence between a chunk's manifest row
/// and its object: warn-severity, ONCE per divergent-object identity
/// `(exec_id, s3_key)` per process (bug_206 — the trend alert reads
/// this counter as incidence; a persistent divergent chunk re-visited
/// by every traversal must not re-tick it), excluded from the
/// critical loss alert by name. The kind stays a label; the ledger
/// key carries the family, so a later HOLE on the same object still
/// pages. Returns whether this call was the first sighting.
/// `kind` MUST be a member of [`crate::LOG_READ_DIVERGENCE_KINDS`]
/// (`overlong`, `short_object_covered`, `missing_object_covered`).
pub(super) fn note_divergence(exec_id: Uuid, s3_key: &str, kind: &'static str) -> bool {
    let first = ledger()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .first_sighting(AnomalyFamily::Divergence, exec_id, s3_key);
    if first {
        metrics::counter!(
            "rio_store_log_read_divergence_total",
            "kind" => kind
        )
        .increment(1);
    }
    first
}

/// The typed unservable kinds — the closed vocabulary of
/// identically-forever refusals (`LOG_UNSERVABLE_METADATA_KEY`
/// values). One enum, so an arm cannot invent an untyped refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UnservableKind {
    /// Manifest row claims more lines than any decodable chunk holds.
    OversizedChunk,
    /// Manifest row stands but the chunk object is gone, and no
    /// cursor/row coverage serves the span.
    MissingObject,
    /// Chunk object holds fewer lines than its row claims, and no
    /// cursor/row coverage serves the missing span.
    ShortObject,
    /// Chunk object exists but does not decode (corruption).
    UndecodableChunk,
}

impl UnservableKind {
    pub(super) const fn as_static(self) -> &'static str {
        match self {
            UnservableKind::OversizedChunk => "oversized_chunk",
            UnservableKind::MissingObject => "missing_object",
            UnservableKind::ShortObject => "short_object",
            UnservableKind::UndecodableChunk => "undecodable_chunk",
        }
    }
}

/// THE permanent-refusal constructor (merged_bug_066): every
/// identically-forever refusal in the read path routes here, so a
/// permanent refusal CANNOT ship without its typed marker (the
/// gateway's reader exit law keys on `LOG_UNSERVABLE_METADATA_KEY` —
/// a bare `Status::internal` classifies TransportErr and wedges the
/// reader at reopen cadence for the build's lifetime) nor without its
/// hole-ledger entry (one hole = one page, N readers or re-reads
/// notwithstanding).
pub(super) fn refuse_permanent(
    exec_id: Uuid,
    s3_key: &str,
    kind: UnservableKind,
    detail: String,
) -> tonic::Status {
    note_hole(exec_id, s3_key, kind.as_static());
    let mut status = tonic::Status::internal(detail);
    status.metadata_mut().insert(
        rio_proto::LOG_UNSERVABLE_METADATA_KEY,
        tonic::metadata::MetadataValue::from_static(kind.as_static()),
    );
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hole-identity dedup (merged_bug_164's trio leg 3): N
    /// sightings of one `(exec_id, s3_key)` = one counter increment.
    /// `note_hole`'s return value is the structural pin — the metric
    /// increment is gated on it, and the misc-check pins the counter
    /// name to this module so no other increment path exists.
    ///
    /// merged_bug_084: this is the ONE thin test allowed on the
    /// process-global wrapper; its keys are uuid-namespaced so no
    /// other test body can collide with them, and the heavy
    /// flood/eviction coverage lives on a LOCAL `Ledger` below —
    /// pre-fix, `ledger_bounded_fifo`'s 8193-key flood through the
    /// same global could evict these keys between adjacent asserts
    /// under same-process parallel runners (plain `cargo test`).
    #[test]
    fn hole_identity_counted_once() {
        let exec = Uuid::now_v7();
        let ns = Uuid::now_v7();
        let k1 = format!("logs/{ns}/k1");
        let k2 = format!("logs/{ns}/k2");
        assert!(note_hole(exec, &k1, "missing_object"));
        assert!(!note_hole(exec, &k1, "missing_object"));
        assert!(!note_hole(exec, &k1, "missing_object"));
        // A different key under the same exec is a different hole.
        assert!(note_hole(exec, &k2, "short_object"));
        // Same key under a different exec is a different hole.
        assert!(note_hole(Uuid::now_v7(), &k1, "missing_object"));
    }

    /// bug_206 RED-FIRST: a persistent divergent chunk re-visited by
    /// every TailLog traversal must count ONCE per divergent object
    /// identity, not once per visit — the RioLogReadDivergence alert
    /// (sum(increase(..[1h])) > 10) reads the counter as incidence.
    #[test]
    fn divergence_identity_counted_once() {
        let rec = rio_test_support::metrics::CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let exec = Uuid::now_v7();
        let ns = Uuid::now_v7();
        let k = format!("logs/{ns}/d1");
        note_divergence(exec, &k, "overlong");
        note_divergence(exec, &k, "overlong");
        assert_eq!(
            rec.get("rio_store_log_read_divergence_total{kind=overlong}"),
            1,
            "one divergent object, many visits, ONE increment"
        );
        // A different object under the same exec is a new sighting.
        note_divergence(exec, &format!("logs/{ns}/d2"), "overlong");
        assert_eq!(
            rec.get("rio_store_log_read_divergence_total{kind=overlong}"),
            2
        );
        // Family separation: a HOLE on the already-diverged object is
        // a DISTINCT anomaly and still pages.
        assert!(
            note_hole(exec, &k, "missing_object"),
            "hole and divergence on one object are different anomalies"
        );
    }

    /// The ledger bound: eviction is FIFO and bounded, so the dedup
    /// degrades (oldest forgotten) rather than growing without bound.
    ///
    /// merged_bug_084: runs on a LOCALLY constructed `Ledger`, not the
    /// process-global one — the 8193-key flood must not be able to
    /// evict a sibling test's keys mid-assert, and FIFO-order
    /// exactness must not depend on which test inserted first.
    /// Globals are never shared between parallel test bodies.
    #[test]
    fn ledger_bounded_fifo() {
        let mut ledger = Ledger::default();
        let exec = Uuid::now_v7();
        let first_key = "logs/bound-0".to_owned();
        assert!(ledger.first_sighting(AnomalyFamily::Loss, exec, &first_key));
        for i in 1..=LEDGER_CAP {
            ledger.first_sighting(AnomalyFamily::Loss, exec, &format!("logs/bound-{i}"));
        }
        // first_key was evicted by the CAP+1'th insert: re-noting it is
        // a "first" sighting again (and that re-insert evicts bound-1,
        // the then-oldest).
        assert!(ledger.first_sighting(AnomalyFamily::Loss, exec, &first_key));
        // Eviction is EXACT FIFO: bound-2 survived both evictions.
        assert!(!ledger.first_sighting(AnomalyFamily::Loss, exec, "logs/bound-2"));
    }
}
