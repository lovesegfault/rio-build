//! Commit-on-Ack evidence buffer for scheduler-bound kube
//! observations (merged_bug_007/bug_082), with per-cell latest-wins
//! supersession across the mark/clear planes (merged_bug_005).
//!
//! The two cell planes carry OPPOSITE polarities for the scheduler's
//! ICE backoff ladder: `unfulfillable_cells` marks, `registered_cells`
//! clears. They used to be independent `extend`-only sets, so an Ack
//! retained across failed ticks could accumulate BOTH polarities for
//! one cell with the temporal order destroyed — and the scheduler's
//! fixed apply order (clears first, marks second) then resurrected a
//! stale buffered mark over a strictly newer registration. The buffer
//! is the only party that knows production order, so the buffer owns
//! the law: **per cell, the newest polarity wins** — buffering a mark
//! evicts a buffered clear for that cell and vice versa, and a request
//! built from the buffer can never carry one cell in both planes.
//!
//! The planes are PRIVATE to this module: every mutation flows through
//! [`PendingSchedulerEvidence::buffer_marks`],
//! [`PendingSchedulerEvidence::buffer_clears`],
//! [`PendingSchedulerEvidence::buffer_observed_types`], or
//! [`PendingSchedulerEvidence::merge`] — the supersession law is
//! enforced at the only writable seam, so a call site that bypasses it
//! does not compile (the machine witness for "every buffered mark and
//! clear is superseded-or-superseding": field privacy + the
//! compiler-generated closure of this module's public mutators).

use std::collections::BTreeSet;

use super::sketch::Cell;

/// Scheduler-bound evidence from kube-only observation
/// (merged_bug_007). The producer edges are consume-once, so a value
/// of this type must never be dropped — `#[must_use]` turns the old
/// `let _ =` discard into a deny-warnings error; ticks that cannot
/// ship it merge it into the reconciler's buffer instead.
#[must_use = "kube-only evidence is consume-once: merge it into pending_evidence (shipped from the buffer, cleared only on Ack-Ok)"]
#[derive(Debug, Default)]
pub(crate) struct PendingSchedulerEvidence {
    /// Cells whose NodeClaim reached `Registered=True` (the ICE-clear
    /// signal). `BTreeSet`: dedup across merged ticks + deterministic
    /// wire order.
    registered_cells: BTreeSet<Cell>,
    /// Per-cell instance types Karpenter resolved (CostTable feed).
    /// Deduped by full tuple on merge. No polarity conflict with the
    /// cell planes — scheduler-side application is an idempotent
    /// upsert.
    observed_types: Vec<rio_proto::types::ObservedInstanceType>,
    /// Cells whose NodeClaim was reaped for ICE (the ICE-mark signal,
    /// bug_082). The producers are consume-once edges — `record_reap`
    /// fires at the instant the claim is deleted, `detect_vanished`
    /// removes its tracking entry in the same retain that emits — so a
    /// mark that misses its Ack is gone forever unless it lives here.
    /// Commit-on-Ack like the sibling planes; `BTreeSet` dedups within
    /// a request. Redelivery after a successful-but-unobserved Ack is
    /// idempotent end-to-end: the scheduler's `IceBackoff::mark`
    /// refreshes the masked window without stepping the ladder while
    /// the mask is unexpired (merged_bug_005).
    ice_cells: BTreeSet<Cell>,
}

impl PendingSchedulerEvidence {
    /// Buffer ICE marks (reap/vanish production sites). Latest-wins:
    /// a newer mark evicts a buffered clear for the same cell — the
    /// failure post-dates the registration the clear recorded.
    pub(crate) fn buffer_marks(&mut self, cells: impl IntoIterator<Item = Cell>) {
        for c in cells {
            self.registered_cells.remove(&c);
            self.ice_cells.insert(c);
        }
    }

    /// Buffer ICE clears (`Registered=True` edges). Latest-wins: a
    /// newer registration evicts a buffered mark for the same cell —
    /// the cell provably delivered capacity after the failure the
    /// mark recorded, and shipping the stale mark would re-mask a
    /// healthy cell (the scheduler cannot reconstruct the order; only
    /// this buffer can).
    pub(crate) fn buffer_clears(&mut self, cells: impl IntoIterator<Item = Cell>) {
        for c in cells {
            self.ice_cells.remove(&c);
            self.registered_cells.insert(c);
        }
    }

    /// Buffer observed instance types, deduped by full tuple.
    pub(crate) fn buffer_observed_types(
        &mut self,
        types: impl IntoIterator<Item = rio_proto::types::ObservedInstanceType>,
    ) {
        for o in types {
            if !self.observed_types.iter().any(|e| {
                e.cell == o.cell
                    && e.instance_type == o.instance_type
                    && e.cores == o.cores
                    && e.mem_bytes == o.mem_bytes
            }) {
                self.observed_types.push(o);
            }
        }
    }

    /// Merge another tick's evidence. `other` is the NEWER production
    /// (this tick's); within a tick the kube-only observation block
    /// emits clears before the reap paths emit marks, so applying
    /// clears-then-marks preserves chronology for every real call
    /// site — and per-cell supersession makes the outcome
    /// deterministic either way.
    pub(crate) fn merge(&mut self, other: PendingSchedulerEvidence) {
        self.buffer_clears(other.registered_cells);
        self.buffer_observed_types(other.observed_types);
        self.buffer_marks(other.ice_cells);
    }

    /// The buffered ICE-mark plane (request building + the local
    /// cover mask).
    pub(crate) fn ice_cells(&self) -> &BTreeSet<Cell> {
        &self.ice_cells
    }

    /// The buffered ICE-clear plane (request building).
    pub(crate) fn registered_cells(&self) -> &BTreeSet<Cell> {
        &self.registered_cells
    }

    /// The buffered observed-instance-type plane (request building).
    pub(crate) fn observed_types(&self) -> &[rio_proto::types::ObservedInstanceType] {
        &self.observed_types
    }
}

#[cfg(test)]
mod tests {
    use super::super::sketch::CapacityType;
    use super::*;

    fn cell() -> Cell {
        Cell("mid-ebs-x86".into(), CapacityType::Spot)
    }

    /// merged_bug_005 red (ordering-inversion axis): a mark buffered
    /// on tick N (Ack failed, mark retained) followed by a
    /// registration on tick N+1 must resolve to the CLEAR — the
    /// registration is strictly newer. Pre-fix both planes kept the
    /// cell (`left: mark AND clear buffered` — and the scheduler's
    /// fixed clears-then-marks apply order re-masked the healthy
    /// cell); post-fix the buffer holds exactly the newest polarity
    /// (`right: clear only`).
    #[test]
    fn newer_clear_supersedes_buffered_mark() {
        let mut ev = PendingSchedulerEvidence::default();
        ev.buffer_marks([cell()]);
        assert!(ev.ice_cells().contains(&cell()));

        let mut newer = PendingSchedulerEvidence::default();
        newer.buffer_clears([cell()]);
        ev.merge(newer);

        assert!(
            !ev.ice_cells().contains(&cell()),
            "stale mark evicted by the newer registration"
        );
        assert!(
            ev.registered_cells().contains(&cell()),
            "the newest polarity (clear) is what ships"
        );
    }

    /// The mirror direction: a failure observed after a buffered
    /// (undelivered) registration wins — the conservative polarity
    /// when the failure is genuinely newer.
    #[test]
    fn newer_mark_supersedes_buffered_clear() {
        let mut ev = PendingSchedulerEvidence::default();
        ev.buffer_clears([cell()]);

        ev.buffer_marks([cell()]);

        assert!(
            !ev.registered_cells().contains(&cell()),
            "stale clear evicted by the newer failure"
        );
        assert!(ev.ice_cells().contains(&cell()));
    }

    /// One cell can never occupy both planes, through any interleaving
    /// — the request built from this buffer is conflict-free by
    /// construction.
    #[test]
    fn planes_are_mutually_exclusive_per_cell() {
        let mut ev = PendingSchedulerEvidence::default();
        ev.buffer_marks([cell()]);
        ev.buffer_clears([cell()]);
        ev.buffer_marks([cell()]);
        let in_marks = ev.ice_cells().contains(&cell());
        let in_clears = ev.registered_cells().contains(&cell());
        assert!(
            in_marks ^ in_clears,
            "exactly one plane holds the cell (marks={in_marks}, clears={in_clears})"
        );
    }

    #[test]
    fn observed_types_dedup_by_full_tuple() {
        let o = |it: &str, cores: u32| rio_proto::types::ObservedInstanceType {
            cell: cell().to_string(),
            instance_type: it.into(),
            cores,
            mem_bytes: 1,
        };
        let mut ev = PendingSchedulerEvidence::default();
        ev.buffer_observed_types([o("m5.large", 2), o("m5.large", 2), o("m5.large", 4)]);
        assert_eq!(
            ev.observed_types().len(),
            2,
            "exact dup dropped, variant kept"
        );
    }
}
