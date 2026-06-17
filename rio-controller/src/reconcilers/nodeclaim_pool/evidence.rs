//! Commit-on-Ack evidence buffer for scheduler-bound kube
//! observations (merged_bug_007/bug_082), holding per-cell ORDERED
//! evidence with producer-minted epochs (merged_bug_003/008 — this
//! supersedes the merged_bug_005 two-set latest-wins model).
//!
//! The two wire planes carry OPPOSITE polarities for the scheduler's
//! ICE backoff ladder: `unfulfillable_cells` marks (climb from the
//! RETAINED step), `registered_cells` clears (the ONLY ladder reset).
//! The planes are NOT symmetric scheduler-side, so supersession as
//! set arithmetic was lossy in exactly one direction: a newer mark
//! evicting a buffered clear destroyed the consume-once
//! `Registered=True` reset, and the cell climbed from its stale rung
//! instead of step 0 (merged_bug_003 — same-tick clear-then-mark is
//! real production order: the kube-only observation block buffers
//! clears before the reap paths buffer marks). The buffer is the only
//! party that knows production order, so the buffer owns the law:
//!
//! - **per cell, evidence is ORDERED** ([`CellEvidence`], a closed
//!   enum): a newer clear still evicts a buffered mark (the
//!   merged_bug_005 direction, unchanged — the cell provably
//!   delivered capacity after the failure); a newer mark RETAINS an
//!   older buffered clear as [`CellEvidence::ClearThenMark`] — the
//!   request then carries the cell in BOTH planes and the scheduler's
//!   fixed clears-then-marks apply order realizes the chronology as
//!   reset-then-step-0.
//! - **every buffered event carries a minted epoch** (the
//!   `"h:cap@epoch"` suffix of the shared `rio_common::cell_wire`
//!   grammar): epochs identify BUFFER EVENTS — the unit of redelivery
//!   — so the scheduler no-ops redelivery (`==`) and reorder (`<`) by
//!   construction (merged_bug_008).
//!
//! The planes are PRIVATE to this module: every mutation flows through
//! [`PendingSchedulerEvidence::buffer_marks`] (the reap production
//! sites), [`PendingSchedulerEvidence::buffer_observed_types`], or
//! [`PendingSchedulerEvidence::merge`] (clears enter ONLY through a
//! tick's [`TickEvidence`] — mirroring production) — the
//! ordered-evidence law is
//! enforced at the only writable seam, so a call site that bypasses
//! it does not compile (the machine witness: field privacy + the
//! compiler-generated closure of this module's public mutators, and
//! the closed [`CellEvidence`] alphabet whose every transition is an
//! exhaustive match).

use std::collections::{BTreeMap, BTreeSet};

use rio_common::cell_wire::EvidenceEpoch;

use super::sketch::Cell;

/// Un-epoch'd kube-only evidence produced by ONE tick
/// (`kube_only_observations`): the clear plane + observed types —
/// the ICE-mark plane is produced by the REAP paths, which buffer
/// directly at the production site via
/// [`PendingSchedulerEvidence::buffer_marks`] (consume-once
/// producers; bug_082). The producer edges here are consume-once
/// too, so a value of this type must never be dropped un-merged.
/// bug_127: the REAL mechanisms, stated honestly — `#[must_use]`
/// warns on an UNUSED result only (`let _ =` is exactly the idiom
/// that silences it, and a bound-then-dropped value never lints), so
/// the machine enforcement is the debug-assertions [`Drop`] guard
/// below: a non-empty `TickEvidence` reaching `Drop` panics in every
/// dev/test build, naming the lost planes. The retired doc claimed
/// the attribute made the discard a deny-warnings error — folklore
/// that demoted the consume-once law to review enforcement; claims
/// about compiler semantics are themselves bind-or-demote surfaces.
/// Ticks that cannot ship the value merge it into the reconciler's
/// buffer instead. Epochs are NOT minted here: they identify buffer
/// events (the unit of redelivery), so
/// [`PendingSchedulerEvidence::merge`] mints at buffer entry.
#[must_use = "kube-only evidence is consume-once: merge it into pending_evidence (shipped from the buffer, cleared only on Ack-Ok); a non-empty value reaching Drop panics under debug_assertions (bug_127)"]
#[derive(Debug, Default)]
pub(crate) struct TickEvidence {
    /// Cells whose NodeClaim reached `Registered=True` (the ICE-clear
    /// signal). `BTreeSet`: dedup within the tick + deterministic
    /// merge order.
    registered_cells: BTreeSet<Cell>,
    /// Per-cell instance types Karpenter resolved (CostTable feed).
    /// Deduped by full tuple on merge. No polarity conflict with the
    /// cell planes — scheduler-side application is an idempotent
    /// upsert.
    observed_types: Vec<rio_proto::types::ObservedInstanceType>,
}

impl TickEvidence {
    /// Collect ICE clears (`Registered=True` edges) for this tick.
    pub(crate) fn buffer_clears(&mut self, cells: impl IntoIterator<Item = Cell>) {
        self.registered_cells.extend(cells);
    }

    /// Collect observed instance types for this tick.
    pub(crate) fn buffer_observed_types(
        &mut self,
        types: impl IntoIterator<Item = rio_proto::types::ObservedInstanceType>,
    ) {
        self.observed_types.extend(types);
    }

    /// Consume the tick's planes for the merge (bug_127): takes every
    /// field and leaves `self` EMPTY, so the [`Drop`] guard sees a
    /// consumed value. The by-ref destructure is the compile pin a
    /// `Drop` type cannot express by-value: a plane added to
    /// [`TickEvidence`] without a take here does not compile.
    fn into_parts(mut self) -> (BTreeSet<Cell>, Vec<rio_proto::types::ObservedInstanceType>) {
        let Self {
            registered_cells,
            observed_types,
        } = &mut self;
        (
            std::mem::take(registered_cells),
            std::mem::take(observed_types),
        )
    }
}

/// bug_127: the consume-once law, MACHINE-ENFORCED — a non-empty
/// `TickEvidence` reaching `Drop` is a producer that observed
/// evidence and lost it (the edges never re-fire), and panics in
/// every debug/test build naming the lost planes. `#[must_use]`
/// cannot do this (`let _ =` silences it; a bound-then-dropped value
/// never lints); review cannot do it reliably. Release builds keep
/// the warn so a production discard is loud, never fatal.
impl Drop for TickEvidence {
    fn drop(&mut self) {
        if !self.registered_cells.is_empty() || !self.observed_types.is_empty() {
            // bug_168 (T4 — enforcement machinery is itself a
            // correctness surface): a machine witness firing via
            // panic-in-Drop inherits Drop's never-panic-while-
            // panicking contract. Without this gate, any panic
            // unwinding through a populated guard DOUBLE-PANICS into
            // SIGABRT in exactly the dev/test builds the guard arms
            // — destroying the original diagnostics the guard exists
            // to improve. Unwinding drops take the warn fallback;
            // the clean-drop panic face is unchanged.
            #[cfg(debug_assertions)]
            if !std::thread::panicking() {
                panic!(
                    "TickEvidence dropped un-merged: {} clear(s), {} observed type(s) lost \
                     (consume-once: merge into pending_evidence — bug_127)",
                    self.registered_cells.len(),
                    self.observed_types.len(),
                );
            }
            tracing::warn!(
                clears = self.registered_cells.len(),
                observed = self.observed_types.len(),
                "TickEvidence dropped un-merged; kube-only evidence lost this tick \
                 (consume-once violation — bug_127)"
            );
        }
    }
}

/// Ordered per-cell evidence — the closed alphabet of what one cell's
/// buffered history can look like between Acks. Every transition is
/// an exhaustive match in this module; a new variant fails to compile
/// until every transition, the [`Self::planes`] projection, and the
/// wire encode handle it (the banner closure-set discipline).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellEvidence {
    /// Newest evidence is a `Registered=True` clear.
    Clear { epoch: EvidenceEpoch },
    /// Newest (and only retained) evidence is an ICE mark.
    Mark { epoch: EvidenceEpoch },
    /// A clear followed by a newer mark — BOTH retained
    /// (merged_bug_003): the request ships the cell in both planes
    /// and the scheduler's fixed clears-then-marks order + epoch gate
    /// realize reset-then-step-0. Constructor-enforced
    /// `clear_epoch < mark_epoch`.
    ClearThenMark {
        clear_epoch: EvidenceEpoch,
        mark_epoch: EvidenceEpoch,
    },
}

impl CellEvidence {
    /// The only `ClearThenMark` constructor — enforces the chronology
    /// the variant encodes. The mint's strict per-buffer monotonicity
    /// makes a violation unreachable from the public mutators; the
    /// assert is the constructor's machine witness.
    fn clear_then_mark(clear_epoch: EvidenceEpoch, mark_epoch: EvidenceEpoch) -> Self {
        assert!(
            clear_epoch < mark_epoch,
            "ClearThenMark chronology violated: clear {clear_epoch} >= mark {mark_epoch}"
        );
        Self::ClearThenMark {
            clear_epoch,
            mark_epoch,
        }
    }

    /// Per-variant plane projection — the effects record deciding
    /// which wire planes this cell ships, with which epoch. Total
    /// over the closed alphabet; the request builder and the local
    /// cover-mask read both consume THIS, so a variant cannot ship
    /// inconsistently between them.
    pub(crate) fn planes(&self) -> CellPlanes {
        match *self {
            Self::Clear { epoch } => CellPlanes {
                clear: Some(epoch),
                mark: None,
            },
            Self::Mark { epoch } => CellPlanes {
                clear: None,
                mark: Some(epoch),
            },
            Self::ClearThenMark {
                clear_epoch,
                mark_epoch,
            } => CellPlanes {
                clear: Some(clear_epoch),
                mark: Some(mark_epoch),
            },
        }
    }
}

/// Which wire planes a buffered cell ships, with which epochs — the
/// [`CellEvidence::planes`] effects record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CellPlanes {
    /// `registered_cells` plane entry (ICE clear).
    pub(crate) clear: Option<EvidenceEpoch>,
    /// `unfulfillable_cells` plane entry (ICE mark).
    pub(crate) mark: Option<EvidenceEpoch>,
}

/// Producer evidence-epoch mint — one per reconciler-owned buffer,
/// minting once per buffered evidence event.
///
/// EPOCH SEMANTICS (Q-S5-4, RULED ADJ-1 — accepted posture):
///
/// - Epochs are SINGLE-LINEAGE producer-identity ORDERING TOKENS:
///   `SystemTime`-millis seeded with the in-process
///   `max(now, prev + 1)` floor, strictly monotone within this
///   buffer's lifetime. They are never compared against any other
///   clock — the scheduler's gate compares controller-minted token
///   against controller-minted token (`epoch > last_applied[cell]`),
///   so the DB-single-frame clock discipline does not apply (that
///   discipline governs two-frame comparisons).
/// - LEASE-HANDOFF RESIDUAL (accepted): a successor replica starts a
///   fresh mint (`prev = 0`, seeded from its own clock). If its clock
///   is BEHIND the old leader's last mint, fresh events mint epochs
///   `<=` the scheduler's `last_applied` and no-op until the
///   successor's clock passes the old leader's last mint — bounded by
///   the inter-replica clock skew. Self-healing: clock catch-up, any
///   later event minting past the old watermark, or the
///   scheduler-side `last_applied` wipe on ITS restart/handoff. This
///   is symmetric with — and the same magnitude as — the consumer
///   side's recorded handoff posture (`IceBackoff` in rio-scheduler
///   `sla/cost.rs`: in-memory, lease-holder-only; a handoff wipes the
///   ladder and `last_applied` at the cost of at most one spurious
///   round per cell).
/// - The mint SURVIVES Ack-Ok ([`PendingSchedulerEvidence::
///   drain_delivered`] resets the planes, not the mint): a
///   Default-reset would re-seed `prev = 0` and could recycle an
///   epoch within the same millisecond, making the scheduler no-op a
///   genuine post-Ack event.
#[derive(Debug, Default)]
struct EpochMint {
    /// Highest epoch minted by THIS buffer instance.
    prev: u64,
}

impl EpochMint {
    fn next(&mut self) -> EvidenceEpoch {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        let e = now.max(self.prev.saturating_add(1));
        self.prev = e;
        EvidenceEpoch(e)
    }
}

/// Scheduler-bound evidence buffered between Acks: per-cell ordered
/// [`CellEvidence`] + the observed-types plane + the epoch mint.
/// Commit-on-Ack: shipped BY READ, planes cleared only on Ack-Ok
/// ([`Self::drain_delivered`] — mint retained).
#[derive(Debug, Default)]
pub(crate) struct PendingSchedulerEvidence {
    /// Per-cell ordered evidence (the closed-alphabet law above).
    cells: BTreeMap<Cell, CellEvidence>,
    /// Observed-instance-type plane, deduped by full tuple.
    observed_types: Vec<rio_proto::types::ObservedInstanceType>,
    /// The per-buffer epoch mint (see its ADJ-1 doc).
    mint: EpochMint,
}

impl PendingSchedulerEvidence {
    /// Buffer one ICE-clear event. Any prior state supersedes to
    /// `Clear` — the newest polarity wins in THIS direction
    /// unchanged (merged_bug_005: the cell provably delivered
    /// capacity after whatever failure a buffered mark recorded;
    /// shipping the stale mark would re-mask a healthy cell).
    fn buffer_clear_event(&mut self, cell: Cell) {
        let epoch = self.mint.next();
        self.cells.insert(cell, CellEvidence::Clear { epoch });
    }

    /// Buffer one ICE-mark event. The transition law (exhaustive over
    /// the closed alphabet — merged_bug_003: a newer mark RETAINS an
    /// older buffered clear instead of destroying the only ladder
    /// reset):
    ///
    /// | prior              | next                                  |
    /// |--------------------|---------------------------------------|
    /// | `None` / `Mark`    | `Mark{new e}`                         |
    /// | `Clear{c}`         | `ClearThenMark{c, new e}`             |
    /// | `ClearThenMark{c,_}` | `ClearThenMark{c, new e}`           |
    fn buffer_mark_event(&mut self, cell: Cell) {
        let epoch = self.mint.next();
        let next = match self.cells.remove(&cell) {
            None | Some(CellEvidence::Mark { .. }) => CellEvidence::Mark { epoch },
            Some(CellEvidence::Clear { epoch: c }) => CellEvidence::clear_then_mark(c, epoch),
            Some(CellEvidence::ClearThenMark { clear_epoch, .. }) => {
                CellEvidence::clear_then_mark(clear_epoch, epoch)
            }
        };
        self.cells.insert(cell, next);
    }

    /// Buffer ICE marks (reap/vanish production sites — they buffer
    /// at the production site because the producers are consume-once;
    /// bug_082).
    pub(crate) fn buffer_marks(&mut self, cells: impl IntoIterator<Item = Cell>) {
        for c in cells {
            self.buffer_mark_event(c);
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

    /// Merge one tick's evidence, minting epochs at buffer entry
    /// (epochs identify BUFFER EVENTS — the unit of redelivery).
    /// Within a tick the kube-only observation block produces clears
    /// BEFORE the reap paths buffer their marks, so the per-tick
    /// production order (merge, then `buffer_marks`) preserves
    /// chronology for every real call site — and the ordered per-cell
    /// law keeps BOTH polarities when the order is clear-then-mark,
    /// so no information is lost either way. The exhaustive
    /// by-ref destructure inside [`TickEvidence::into_parts`]
    /// consumes every tick plane: a plane added to
    /// [`TickEvidence`] without a take there does not compile
    /// (bug_127: `Drop` types cannot destructure by value, so the
    /// pin moved into the consuming constructor).
    pub(crate) fn merge(&mut self, tick: TickEvidence) {
        let (registered_cells, observed_types) = tick.into_parts();
        for c in registered_cells {
            self.buffer_clear_event(c);
        }
        self.buffer_observed_types(observed_types);
    }

    /// Ack-Ok: the planes provably reached the scheduler — reset
    /// them; the MINT survives (a Default-reset would recycle epochs
    /// after every Ack and the scheduler would no-op genuine events;
    /// see the [`EpochMint`] doc).
    pub(crate) fn drain_delivered(&mut self) {
        self.cells.clear();
        self.observed_types.clear();
    }

    /// Cells whose buffered evidence ships an ICE mark (`Mark` or
    /// `ClearThenMark`) — the local cover-mask read: a buffered mark
    /// must keep `cover_deficit` out of the cell until acked, and a
    /// RETAINED clear must never unmask local cover (the mark is the
    /// newest polarity).
    pub(crate) fn ice_cells(&self) -> impl Iterator<Item = &Cell> {
        self.cells
            .iter()
            .filter(|(_, ev)| ev.planes().mark.is_some())
            .map(|(c, _)| c)
    }

    /// Per-cell buffered evidence (request building) — deterministic
    /// `BTreeMap` order; each cell projects to wire planes via
    /// [`CellEvidence::planes`].
    pub(crate) fn cell_events(&self) -> impl Iterator<Item = (&Cell, &CellEvidence)> {
        self.cells.iter()
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

    fn planes_of(ev: &PendingSchedulerEvidence, c: &Cell) -> Option<CellPlanes> {
        ev.cell_events()
            .find(|(cc, _)| *cc == c)
            .map(|(_, e)| e.planes())
    }

    /// Clears enter the buffer the way production sends them: via a
    /// tick's `TickEvidence` merge.
    fn merge_clear(ev: &mut PendingSchedulerEvidence, c: Cell) {
        let mut tick = TickEvidence::default();
        tick.buffer_clears([c]);
        ev.merge(tick);
    }

    /// merged_bug_005 red (ordering-inversion axis), pinned through
    /// the re-typed buffer: a mark buffered on tick N (Ack failed,
    /// mark retained) followed by a registration on tick N+1 must
    /// resolve to the CLEAR — the registration is strictly newer, and
    /// shipping the stale mark would re-mask the healthy cell. This
    /// direction's eviction law is UNCHANGED by merged_bug_003.
    /// W12-AS (bug_168) — proposition: the guard never destroys the
    /// diagnostics it exists to improve; population: {clean drop,
    /// unwinding drop} — both arms pinned (the clean-drop
    /// #[should_panic] face is the sibling test below/existing).
    #[test]
    fn w12_as_unwind_through_populated_guard_preserves_the_panic() {
        // The live trigger surface: a failing assertion (any panic)
        // unwinding through a populated TickEvidence — exactly the
        // dev/test builds the guard arms. Pre-fix the Drop panic had
        // no thread::panicking() gate: double-panic → SIGABRT, and
        // the guard destroyed the very diagnostics it exists to
        // improve. Post-fix the warn fallback fires and the ORIGINAL
        // panic propagates intact.
        let r = std::panic::catch_unwind(|| {
            let mut tick = TickEvidence::default();
            tick.buffer_clears([cell()]);
            panic!("the original diagnostic");
        });
        let e = r.expect_err("the original panic propagates");
        assert_eq!(
            e.downcast_ref::<&str>(),
            Some(&"the original diagnostic"),
            "the guard must never replace the original panic"
        );
    }

    // r[verify ctrl.nodeclaim.evidence-ack-latch+3]
    #[test]
    fn newer_clear_supersedes_buffered_mark() {
        let mut ev = PendingSchedulerEvidence::default();
        ev.buffer_marks([cell()]);
        assert!(ev.ice_cells().any(|c| *c == cell()));

        let mut newer = TickEvidence::default();
        newer.buffer_clears([cell()]);
        ev.merge(newer);

        assert!(
            !ev.ice_cells().any(|c| *c == cell()),
            "stale mark evicted by the newer registration"
        );
        let p = planes_of(&ev, &cell()).expect("clear buffered");
        assert!(p.clear.is_some() && p.mark.is_none(), "clear-only ships");
    }

    /// merged_bug_003 red: a failure observed after a buffered
    /// (undelivered) registration RETAINS the clear — the request
    /// ships the cell in BOTH planes with `clear_epoch < mark_epoch`,
    /// so the scheduler's fixed clears-then-marks order + epoch gate
    /// realize reset-then-step-0. `left (pre-fix): buffer_marks did
    /// registered_cells.remove(&c) — the consume-once reset was
    /// destroyed and the cell climbed from its stale rung` /
    /// `right: ClearThenMark, both planes, ordered epochs`.
    // r[verify ctrl.nodeclaim.evidence-ack-latch+3]
    #[test]
    fn newer_mark_retains_buffered_clear_as_clear_then_mark() {
        let mut ev = PendingSchedulerEvidence::default();
        merge_clear(&mut ev, cell());
        ev.buffer_marks([cell()]);

        let p = planes_of(&ev, &cell()).expect("cell buffered");
        let (clear_e, mark_e) = (
            p.clear.expect("retained clear ships"),
            p.mark.expect("newer mark ships"),
        );
        assert!(
            clear_e < mark_e,
            "chronology on the wire: clear {clear_e} before mark {mark_e}"
        );
        assert!(
            ev.ice_cells().any(|c| *c == cell()),
            "the retained clear must NOT unmask local cover — the mark is newest"
        );
    }

    /// REWRITTEN STRAWMAN (disclosed): this test previously asserted
    /// the LOSSY XOR law — "exactly one plane holds the cell
    /// (`in_marks ^ in_clears`)" — which is precisely what
    /// merged_bug_003 falsifies. The closure law it pins now:
    /// `mark → clear → mark` ends `ClearThenMark` shipping BOTH
    /// planes with `clear_epoch < mark_epoch` (the clear stays the
    /// one the scheduler resets on; the mark epoch advances with each
    /// newer failure).
    // r[verify ctrl.nodeclaim.evidence-ack-latch+3]
    // r[verify ctrl.nodeclaim.ice-mark-clear+6]
    #[test]
    fn mark_clear_mark_ends_clear_then_mark_shipping_both_planes() {
        let mut ev = PendingSchedulerEvidence::default();
        ev.buffer_marks([cell()]);
        merge_clear(&mut ev, cell());
        let p_after_clear = planes_of(&ev, &cell()).unwrap();
        assert!(
            p_after_clear.mark.is_none(),
            "clear evicted the older mark (merged_bug_005 direction)"
        );
        ev.buffer_marks([cell()]);

        let p = planes_of(&ev, &cell()).unwrap();
        let (c_e, m_e) = (p.clear.unwrap(), p.mark.unwrap());
        assert!(c_e < m_e, "both planes, ordered: clear {c_e} < mark {m_e}");

        // A further mark advances the mark epoch, keeps the SAME
        // retained clear.
        ev.buffer_marks([cell()]);
        let p2 = planes_of(&ev, &cell()).unwrap();
        assert_eq!(p2.clear, Some(c_e), "retained clear is stable");
        assert!(p2.mark.unwrap() > m_e, "mark epoch advances");
    }

    /// merged_bug_008 companion (controller half): epochs are minted
    /// once per buffer event and strictly increase across the whole
    /// buffer lifetime — including across Ack-Ok drains. `left:` a
    /// Default-reset on Ack-Ok recycles the mint and a same-millis
    /// post-Ack event can repeat an epoch the scheduler already
    /// applied (no-op'ing a genuine mark) / `right:` monotone.
    // r[verify ctrl.nodeclaim.evidence-ack-latch+3]
    #[test]
    fn drain_delivered_retains_epoch_mint() {
        let mut ev = PendingSchedulerEvidence::default();
        ev.buffer_marks([cell()]);
        let e1 = planes_of(&ev, &cell()).unwrap().mark.unwrap();

        // Ack-Ok: planes reset, mint retained.
        ev.drain_delivered();
        assert_eq!(ev.cell_events().count(), 0, "planes reset on Ack-Ok");
        assert_eq!(ev.observed_types().len(), 0);

        ev.buffer_marks([cell()]);
        let e2 = planes_of(&ev, &cell()).unwrap().mark.unwrap();
        assert!(
            e2 > e1,
            "post-drain epoch must be strictly newer ({e2} vs {e1})"
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

    /// The real per-tick production order — kube-only clears merged
    /// FIRST, reap-path marks buffered SECOND — yields the ordered
    /// ClearThenMark, not a lost reset.
    #[test]
    fn tick_merge_then_reap_marks_yields_clear_then_mark() {
        let mut tick = TickEvidence::default();
        tick.buffer_clears([cell()]);
        let mut ev = PendingSchedulerEvidence::default();
        ev.merge(tick);
        ev.buffer_marks([cell()]);
        let p = planes_of(&ev, &cell()).unwrap();
        assert!(
            p.clear.is_some() && p.mark.is_some(),
            "same-tick clear-then-mark ships both planes"
        );
        assert!(p.clear.unwrap() < p.mark.unwrap());
    }
    /// W11-BP (bug_127): the strawman non-merging producer — a tick
    /// that observed evidence and dropped it — trips the debug Drop
    /// guard. This is the machine enforcement the retired folklore
    /// doc ascribed to `#[must_use]` (`let _ =` silences that
    /// attribute; a bound-then-dropped value never lints at all).
    /// Stated per R16 at its real tier: debug-guard (every dev/test
    /// build), not compile — and the test is cfg-gated to that same
    /// tier: the CI nextest binaries build release
    /// (`debug_assertions` off, the guard's warn arm), where a
    /// should_panic on the compiled-out panic arm fails by
    /// construction. The dev-profile run (`nix develop -c cargo
    /// nextest run`, the per-commit boundary check) exercises it on
    /// every boundary; the release tier's warn arm is covered by the
    /// companion clean-drop test compiling/running in BOTH profiles.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "TickEvidence dropped un-merged")]
    fn unmerged_tick_evidence_trips_the_drop_guard() {
        let mut tick = TickEvidence::default();
        tick.buffer_clears([cell()]);
        // The silencing idiom the folklore claimed was an error:
        let _ = tick;
        // (the panic fires at the drop above)
    }

    /// W11-BP companion: the lawful paths stay silent — a merged tick
    /// and an empty tick both drop clean.
    #[test]
    fn merged_and_empty_ticks_drop_clean() {
        let mut pending = PendingSchedulerEvidence::default();
        let mut tick = TickEvidence::default();
        tick.buffer_clears([cell()]);
        pending.merge(tick); // consumed: into_parts empties before drop
        let empty = TickEvidence::default();
        drop(empty); // nothing observed, nothing owed
    }
}
