//! The retry/poison decision surface, as the scheduler consumes it.
//!
//! The decision logic itself — the reference fold over a derivation's
//! observed failure history, the Phase-1b decision functions
//! `decide()` / `classify()` / `placeable()`, and their CBMC contracts
//! and proof harnesses — lives in the dependency-free `rio-retry-kernel`
//! crate (see its crate docs for the full fold/divergence/Phase-1 scope
//! documentation that used to live here). It was extracted so the kani
//! goto model for the proof harnesses closes over the kernel crate only:
//! inside rio-scheduler's artifact context the harnesses inherited the
//! crate's full reachable code, Arc-backed identifiers and f64 timestamp
//! conversions, and CBMC did not converge inside a merge-gate budget.
//!
//! This module is the projection shim and the scheduler-facing surface:
//!
//! - it re-exports the kernel vocabulary under the names the actor and
//!   db layers have always used (`Budget`, `Verdict`, `PoisonReason`,
//!   `Placement`, `Counters`, …), pinning the
//!   executor-identity parameter to the fold's `String` vocabulary;
//! - [`decide`] projects the scheduler's [`AttemptRecord`] ledger rows
//!   (UUIDs, error messages, f64 epoch timestamps, [`ExecutorId`]s) onto
//!   the kernel's [`rio_retry_kernel::LedgerRow`] and maps the kernel's
//!   decision back into the actor's [`ExecutorId`] exclusion vocabulary;
//! - [`classify`] maps the kernel's outcome-class verdict back onto the
//!   sqlx-backed [`OutcomeClass`] db enum.
//!
//! The projection is deliberately mechanical: every enum bridge below is
//! an exhaustive `match`, so a variant added on either side fails to
//! compile until the bridge (and the kernel's row vocabulary) is updated.
//! The hand-computed-history test battery at the bottom of this file
//! exercises the kernel through this shim — it is the equivalence oracle
//! that pins the extraction to the pre-extraction behavior.

use std::collections::BTreeSet;

use crate::state::{
    AttemptEventKind, AttemptKind, AttemptRecord, ExecutorId, OutcomeClass, ReportingParty,
};

pub(crate) use rio_retry_kernel::{
    AbsTime, Budget, FloorOutcomeView, ObservedFailure, PoisonReason, Verdict, sweep_horizon_secs,
};
// The placement partition is consumed by the AD2 spawn gate
// (controller-side) and the retry-kernel Kani contracts; in this crate
// only the equivalence-oracle tests still exercise it since the
// completion-time fleet-exhaust arm retired with the executors map.
#[cfg(test)]
pub(crate) use rio_retry_kernel::{Placement, placeable};

/// The ten `RetryState` counters as the fold computes them, in the
/// fold's `String` executor-identity vocabulary
/// (`rio_retry_kernel::Counters` pinned to the scheduler's
/// instantiation).
pub(crate) type Counters = rio_retry_kernel::Counters<String>;

// The reference fold and its event/fleet vocabulary are consumed by the
// hand-computed-history battery in `mod tests` below (production call
// sites go through `decide()`, and the CBMC harnesses live next to the
// kernel in rio-retry-kernel). Scoped to cfg(test) so the re-exports
// don't sit unused in the production build.
#[cfg(test)]
pub(crate) use rio_retry_kernel::reference_fold;

/// The fold's observed-event alphabet, `String`-keyed (test battery
/// vocabulary).
#[cfg(test)]
pub(crate) type AttemptEvent = rio_retry_kernel::AttemptEvent<String>;

/// The eligible-fleet snapshot consumed by the fold's fleet-exhaust arm,
/// `String`-keyed (test battery vocabulary).
#[cfg(test)]
pub(crate) type FleetView = rio_retry_kernel::FleetView<String>;

/// The decision-surface output for one appending-transaction read: the
/// budget verdict plus the derived views the call sites consume.
///
/// This is the kernel's `Decision` translated into the actor's
/// identifier vocabulary: the exclusion set is keyed by [`ExecutorId`]
/// (what `placeable()` and the spawn-intent exclusion consume), while the full
/// counter view keeps the fold's `String` keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decision {
    /// The budget verdict as of the end of the history.
    pub verdict: Verdict,
    /// The per-executor exclusion set (the fold's `failed_builders`) in
    /// the actor's identifier vocabulary. E1's fleet-exhaust arm and the
    /// E9 dispatch backstop intersect it with the live eligible fleet via
    /// `placeable`; the spawn-intent exclusion consumes the same set
    /// through the fold-refreshed cached view (`RetryState::failed_builders`).
    pub exclusion: BTreeSet<ExecutorId>,
    /// The deterministic backoff deadline (no jitter — the dispatch site
    /// applies the production jitter exactly as today).
    pub backoff_until: Option<AbsTime>,
    /// The full fold-derived counter view.
    pub counters: Counters,
}

/// Phase-1b decision function: fold a derivation's attempt-ledger suffix
/// into the budget verdict and the derived counter/exclusion views.
///
/// `history` is the post-reset suffix in ledger order (what
/// `load_attempt_suffix` returns, or the in-memory mirror of it),
/// INCLUDING the row the calling site just appended — the verdict is the
/// disposition produced by the last decision-bearing event. `now` is
/// epoch seconds (the same clock as `AttemptRecord::occurred_at_epoch_secs`)
/// and is consulted only for the poison-TTL downgrade. See
/// [`rio_retry_kernel::decide`] for the contract the kernel carries for
/// the whole decision.
///
/// This shim only projects: [`AttemptRecord`] → [`rio_retry_kernel::LedgerRow`]
/// (drop the fields the fold ignores, convert the f64 epoch timestamp to
/// the abstract clock), delegate to the kernel, and lift the exclusion
/// set back into [`ExecutorId`]s.
pub(crate) fn decide(history: &[AttemptRecord], budget: &Budget, now: AbsTime) -> Decision {
    let rows: Vec<rio_retry_kernel::LedgerRow<String>> =
        history.iter().map(record_to_row).collect();
    let decision = rio_retry_kernel::decide(&rows, budget, now);
    Decision {
        verdict: decision.verdict,
        exclusion: decision
            .exclusion
            .iter()
            .map(|s| ExecutorId::from(s.clone()))
            .collect(),
        backoff_until: decision.backoff_until,
        counters: decision.counters,
    }
}

// r[impl sched.attempt.worker-abort-bounded+2]
/// Admit one worker-abort report (`BuildResultStatus::Cancelled` for a
/// still-wanted open build attempt) against the in-memory attempt
/// history: the kernel counts the worker-abort closures within the
/// trailing bounded-uncharged UNION run (bug_098 — sibling uncharged
/// rows extend the run, never reset it) and admits the charge-free
/// close only below [`rio_retry_kernel::WORKER_ABORT_FREE_CLOSES`].
/// Same projection shim shape as [`decide`].
pub(crate) fn admit_worker_abort(
    history: &[AttemptRecord],
) -> rio_retry_kernel::WorkerAbortAdmission {
    let rows: Vec<rio_retry_kernel::LedgerRow<String>> =
        history.iter().map(record_to_row).collect();
    rio_retry_kernel::admit_worker_abort(&rows, rio_retry_kernel::WORKER_ABORT_FREE_CLOSES)
}

// r[impl sched.retry.store-degraded-uncharged+3]
/// Admit one corroborated store-degraded report against the in-memory
/// attempt history: the kernel counts the store-degraded pacing rows
/// within the trailing bounded-uncharged UNION run (bug_098 — sibling
/// uncharged rows extend the run, never reset it) and admits the
/// uncharged paced requeue only below
/// [`rio_retry_kernel::STORE_DEGRADED_FREE_RUN`]; at the bound the
/// report falls through to the CHARGED infra path (merged_bug_032).
/// Same projection shim shape as [`decide`].
pub(crate) fn admit_store_degraded(
    history: &[AttemptRecord],
) -> rio_retry_kernel::WorkerAbortAdmission {
    let rows: Vec<rio_retry_kernel::LedgerRow<String>> =
        history.iter().map(record_to_row).collect();
    rio_retry_kernel::admit_store_degraded(&rows, rio_retry_kernel::STORE_DEGRADED_FREE_RUN)
}

/// The materialization lane's windowed counters (the kernel fold over
/// the in-memory history, projected through the same record→row
/// conversion `decide()` uses) — THE single budget/one-shot/strictness
/// counter for `actor/materialize.rs` (merged_bug_020: the flat
/// per-class history counts are deleted). A mat-lane job-creation
/// reset row (migration 085) re-windows all three counts exactly as it
/// re-windows [`rio_retry_kernel::materialization_decide`]'s own fold;
/// build-lane rows (reset or not) neither count nor cut.
pub(crate) fn materialization_counters(history: &[AttemptRecord]) -> rio_retry_kernel::MatCounters {
    let rows: Vec<rio_retry_kernel::LedgerRow<String>> =
        history.iter().map(record_to_row).collect();
    rio_retry_kernel::materialization_counters(&rows)
}

/// Index where the MATERIALIZATION lane's window of `history` begins
/// (the kernel's per-lane suffix cut projected through `record_to_row`
/// — see [`materialization_counters`], which budget consumers use
/// directly).
pub(crate) fn materialization_window_start(history: &[AttemptRecord]) -> usize {
    let rows: Vec<rio_retry_kernel::LedgerRow<String>> =
        history.iter().map(record_to_row).collect();
    rio_retry_kernel::ledger_suffix_start(&rows, rio_retry_kernel::AttemptKind::Materialization)
}

/// Project one in-memory ledger record onto the kernel's row vocabulary.
/// Field-for-field: the fields `decide()` ignores (UUIDs, error
/// messages, `recorded_at_epoch_secs`, the `exempt` convenience flag —
/// the class already carries the exemption) are dropped, and the
/// occurrence timestamp moves onto
/// the abstract whole-second clock with the same `as` cast the fold has
/// always used.
///
/// AD2 / decision P12: the exclusion/budget identity is the
/// controller-authoritative `source_node` ONLY. A row without one (a
/// pull attempt whose binding ack never landed, or a pre-pull legacy
/// row) folds with no identity: it charges its flat counters but
/// contributes no exclusion key and no distinct-source slot — the
/// executor id (the pod name / attested intent) is never used as a
/// budget key.
// r[impl sched.retry.per-executor-budget+4]
fn record_to_row(record: &AttemptRecord) -> rio_retry_kernel::LedgerRow<String> {
    rio_retry_kernel::LedgerRow {
        event_kind: kernel_event_kind(record.event_kind),
        outcome_class: kernel_outcome_class(record.outcome_class),
        executor: record.source_node.clone(),
        reporting_party: kernel_reporting_party(record.reporting_party),
        floor_promoted: record.floor_promoted,
        floor_at_cap: record.floor_at_cap,
        resubmit_cycle: record.resubmit_cycle,
        at: record.occurred_at_epoch_secs as AbsTime,
        kind: kernel_attempt_kind(record.attempt_kind),
    }
}

/// db enum → kernel row kind.
fn kernel_event_kind(kind: AttemptEventKind) -> rio_retry_kernel::AttemptEventKind {
    match kind {
        AttemptEventKind::Attempt => rio_retry_kernel::AttemptEventKind::Attempt,
        AttemptEventKind::Reset => rio_retry_kernel::AttemptEventKind::Reset,
    }
}

/// db enum → kernel work kind (the kind-partition key; substitution-
/// replacement design §2.5). Exhaustive so a variant added on either
/// side fails to compile until both move together.
fn kernel_attempt_kind(kind: AttemptKind) -> rio_retry_kernel::AttemptKind {
    match kind {
        AttemptKind::Build => rio_retry_kernel::AttemptKind::Build,
        AttemptKind::Materialization => rio_retry_kernel::AttemptKind::Materialization,
    }
}

/// db enum → kernel outcome class.
fn kernel_outcome_class(class: OutcomeClass) -> rio_retry_kernel::OutcomeClass {
    match class {
        OutcomeClass::Transient => rio_retry_kernel::OutcomeClass::Transient,
        OutcomeClass::Infra => rio_retry_kernel::OutcomeClass::Infra,
        OutcomeClass::ExemptInfra => rio_retry_kernel::OutcomeClass::ExemptInfra,
        OutcomeClass::Timeout => rio_retry_kernel::OutcomeClass::Timeout,
        OutcomeClass::Permanent => rio_retry_kernel::OutcomeClass::Permanent,
        OutcomeClass::Cascade => rio_retry_kernel::OutcomeClass::Cascade,
        OutcomeClass::Backstop => rio_retry_kernel::OutcomeClass::Backstop,
        OutcomeClass::Disconnected => rio_retry_kernel::OutcomeClass::Disconnected,
        OutcomeClass::ExecutorCrash => rio_retry_kernel::OutcomeClass::ExecutorCrash,
        OutcomeClass::FleetExhaust => rio_retry_kernel::OutcomeClass::FleetExhaust,
        OutcomeClass::ResubmitReset => rio_retry_kernel::OutcomeClass::ResubmitReset,
        OutcomeClass::StoreDegraded => rio_retry_kernel::OutcomeClass::StoreDegraded,
        OutcomeClass::CacheHitClear => rio_retry_kernel::OutcomeClass::CacheHitClear,
        OutcomeClass::PoisonCleared => rio_retry_kernel::OutcomeClass::PoisonCleared,
        OutcomeClass::MaterializationUnobtainable => {
            rio_retry_kernel::OutcomeClass::MaterializationUnobtainable
        }
        OutcomeClass::MaterializationInfra => rio_retry_kernel::OutcomeClass::MaterializationInfra,
        OutcomeClass::MaterializationReset => rio_retry_kernel::OutcomeClass::MaterializationReset,
    }
}

/// db enum → kernel reporting party.
fn kernel_reporting_party(party: ReportingParty) -> rio_retry_kernel::ReportingParty {
    match party {
        ReportingParty::Worker => rio_retry_kernel::ReportingParty::Worker,
        ReportingParty::Controller => rio_retry_kernel::ReportingParty::Controller,
        ReportingParty::Scheduler => rio_retry_kernel::ReportingParty::Scheduler,
        ReportingParty::Admin => rio_retry_kernel::ReportingParty::Admin,
    }
}

/// kernel outcome class → db enum (the [`classify`] return path).
fn db_outcome_class(class: rio_retry_kernel::OutcomeClass) -> OutcomeClass {
    match class {
        rio_retry_kernel::OutcomeClass::Transient => OutcomeClass::Transient,
        rio_retry_kernel::OutcomeClass::Infra => OutcomeClass::Infra,
        rio_retry_kernel::OutcomeClass::ExemptInfra => OutcomeClass::ExemptInfra,
        rio_retry_kernel::OutcomeClass::Timeout => OutcomeClass::Timeout,
        rio_retry_kernel::OutcomeClass::Permanent => OutcomeClass::Permanent,
        rio_retry_kernel::OutcomeClass::Cascade => OutcomeClass::Cascade,
        rio_retry_kernel::OutcomeClass::Backstop => OutcomeClass::Backstop,
        rio_retry_kernel::OutcomeClass::Disconnected => OutcomeClass::Disconnected,
        rio_retry_kernel::OutcomeClass::ExecutorCrash => OutcomeClass::ExecutorCrash,
        rio_retry_kernel::OutcomeClass::FleetExhaust => OutcomeClass::FleetExhaust,
        rio_retry_kernel::OutcomeClass::ResubmitReset => OutcomeClass::ResubmitReset,
        rio_retry_kernel::OutcomeClass::CacheHitClear => OutcomeClass::CacheHitClear,
        rio_retry_kernel::OutcomeClass::PoisonCleared => OutcomeClass::PoisonCleared,
        rio_retry_kernel::OutcomeClass::MaterializationUnobtainable => {
            OutcomeClass::MaterializationUnobtainable
        }
        rio_retry_kernel::OutcomeClass::MaterializationInfra => OutcomeClass::MaterializationInfra,
        rio_retry_kernel::OutcomeClass::MaterializationReset => OutcomeClass::MaterializationReset,
        rio_retry_kernel::OutcomeClass::StoreDegraded => OutcomeClass::StoreDegraded,
    }
}

/// Classify one observed failure event into the ledger's outcome-class
/// alphabet, consuming the floor outcome at append time so [`decide`]
/// never sees the floor. The classification partition (and the
/// CONCURRENT_PUTPATH / floor-promotion exemption predicate on both
/// channels) is [`rio_retry_kernel::classify`]'s contract; this shim
/// only lifts the verdict back onto the sqlx-backed [`OutcomeClass`]
/// db enum.
pub(crate) fn classify(event: &ObservedFailure<'_>, floor: FloorOutcomeView) -> OutcomeClass {
    db_outcome_class(rio_retry_kernel::classify(event, floor))
}

// r[verify sched.retry.transient-budget+2]
// r[verify sched.retry.attempts-bounded+5]
// r[verify sched.retry.counters-refine-history+2]
#[cfg(test)]
mod tests {
    use super::*;

    fn t(at: AbsTime, ex: &str) -> AttemptEvent {
        AttemptEvent::Transient {
            at,
            executor: Some(ex.into()),
        }
    }

    fn infra(at: AbsTime, ex: &str, exempt: bool, at_cap: bool) -> AttemptEvent {
        AttemptEvent::Infra {
            at,
            executor: Some(ex.into()),
            exempt,
            at_cap,
        }
    }

    fn fleet(workers: &[&str]) -> FleetView {
        FleetView {
            eligible: workers.iter().map(|w| w.to_string()).collect(),
        }
    }

    fn fold(history: &[AttemptEvent], now: AbsTime) -> (Counters, Verdict) {
        reference_fold(history, now, &Budget::default(), &FleetView::default())
    }

    /// Empty history: everything zero, eligible for dispatch.
    #[test]
    fn empty_history_is_requeue_with_default_counters() {
        let (c, v) = fold(&[], 0);
        assert_eq!(c, Counters::default());
        assert_eq!(v, Verdict::Requeue);
    }

    /// The transient budget: with `max_retries = 2` and same-worker
    /// failures (so the distinct-worker threshold of 3 never trips), the
    /// first two failures requeue with an exponential backoff and the
    /// third poisons via the count cap. Charges `count`,
    /// `failure_count`, `failed_builders`, and `backoff_until`.
    #[test]
    fn transient_budget_two_retries_then_poison() {
        let h = [t(100, "w1"), t(200, "w1"), t(300, "w1")];

        let (c, v) = fold(&h[..1], 100);
        assert_eq!(c.count, 1);
        assert_eq!(c.failure_count, 1);
        assert_eq!(c.failed_builders.len(), 1);
        // backoff(0) = base = 5 s, computed from the pre-increment count.
        assert_eq!(c.backoff_until, Some(105));
        assert_eq!(v, Verdict::Requeue);

        let (c, v) = fold(&h[..2], 200);
        assert_eq!(c.count, 2);
        assert_eq!(c.failure_count, 2);
        assert_eq!(c.failed_builders.len(), 1, "same worker dedups");
        // backoff(1) = 5 * 2 = 10 s.
        assert_eq!(c.backoff_until, Some(210));
        assert_eq!(v, Verdict::Requeue);

        let (c, v) = fold(&h, 300);
        assert_eq!(c.count, 2, "the poisoning failure does not increment");
        assert_eq!(c.failure_count, 3);
        assert_eq!(c.poisoned_at, Some(300));
        assert_eq!(v, Verdict::Poison(PoisonReason::TransientBudget));
    }

    /// The distinct-worker poison threshold fires on the third distinct
    /// executor, before the count cap gets a say, and a successful
    /// dispatch between failures clears the backoff defer.
    #[test]
    fn distinct_worker_threshold_poisons_on_third_executor() {
        let h = [
            t(100, "w1"),
            AttemptEvent::Dispatched {
                at: 110,
                executor: Some("w2".into()),
            },
            t(200, "w2"),
            t(300, "w3"),
        ];
        let (c, v) = fold(&h[..2], 110);
        assert_eq!(c.backoff_until, None, "dispatch clears the defer");
        assert_eq!(v, Verdict::Requeue);

        let (c, v) = fold(&h, 300);
        assert_eq!(c.failed_builders.len(), 3);
        assert_eq!(c.failure_count, 3);
        assert_eq!(v, Verdict::Poison(PoisonReason::Threshold));
        assert_eq!(c.poisoned_at, Some(300));
    }

    /// The fleet-exhaust arm: two distinct workers fail (below the
    /// threshold of 3) but the eligible fleet IS those two workers, so
    /// the second failure poisons. An empty fleet never exhausts.
    #[test]
    fn fleet_exhaust_poisons_below_the_threshold() {
        let h = [t(100, "w1"), t(200, "w2")];
        let two = fleet(&["w1", "w2"]);
        let (c, v) = reference_fold(&h, 200, &Budget::default(), &two);
        assert_eq!(c.failed_builders.len(), 2);
        assert_eq!(v, Verdict::Poison(PoisonReason::FleetExhausted));

        // The same history against a fleet with a fresh worker requeues.
        let three = fleet(&["w1", "w2", "w3"]);
        let (_, v) = reference_fold(&h, 200, &Budget::default(), &three);
        assert_eq!(v, Verdict::Requeue);

        // And against an empty fleet (no pool connected) it requeues.
        let (_, v) = reference_fold(&h, 200, &Budget::default(), &FleetView::default());
        assert_eq!(v, Verdict::Requeue);
    }

    /// The non-exempt infra budget: check-then-increment, so
    /// `max_infra_retries = 10` means ten requeues and a poison on the
    /// eleventh failure. Charges `infra_count` and the window anchor and
    /// nothing else.
    #[test]
    fn infra_budget_ten_requeues_then_poison_on_the_eleventh() {
        let mut h: Vec<AttemptEvent> = Vec::new();
        for i in 0..10 {
            h.push(infra(100 + i, "w1", false, false));
        }
        let (c, v) = fold(&h, 200);
        assert_eq!(c.infra_count, 10);
        assert_eq!(c.last_infra_failure_at, Some(109));
        assert_eq!(c.count, 0, "infra does not eat the transient budget");
        assert_eq!(c.failed_builders.len(), 0, "infra never joins the set");
        assert_eq!(c.backoff_until, None, "infra requeues immediately");
        assert_eq!(v, Verdict::Requeue);

        h.push(infra(110, "w1", false, false));
        let (c, v) = fold(&h, 200);
        assert_eq!(
            c.infra_count, 10,
            "the poisoning failure does not increment"
        );
        assert_eq!(v, Verdict::Poison(PoisonReason::InfraBudget));
    }

    /// The 300 s sliding window: two infra failures, then a third more
    /// than the window after the second — the counter resets to zero
    /// before charging, so the third failure leaves `infra_count = 1`. A
    /// fourth within the window accumulates normally. An at-cap failure
    /// is never forgiven by the window.
    #[test]
    fn infra_window_reset_forgives_sparse_failures() {
        let h = [
            infra(100, "w1", false, false),
            infra(150, "w1", false, false),
            // 301 s after the second — strictly greater than the window.
            infra(452, "w1", false, false),
            infra(500, "w1", false, false),
        ];
        let (c, _) = fold(&h[..2], 150);
        assert_eq!(c.infra_count, 2);

        let (c, _) = fold(&h[..3], 452);
        assert_eq!(c.infra_count, 1, "window elapsed: reset then charge");
        assert_eq!(c.last_infra_failure_at, Some(452));

        let (c, _) = fold(&h, 500);
        assert_eq!(c.infra_count, 2, "within the window: accumulate");

        // At-cap failures are exempt from the forgiveness: the same
        // sparse spacing keeps accumulating.
        let h_cap = [
            infra(100, "w1", false, true),
            infra(452, "w1", false, true),
            infra(900, "w1", false, true),
        ];
        let (c, _) = fold(&h_cap, 900);
        assert_eq!(c.infra_count, 3, "at-cap failures never reset");
    }

    /// The as-built E2 fall-through: the I-127 stale-window reset is
    /// gated only on the event's own at-cap outcome, not on the
    /// exemption, so an under-cap exempt infra failure
    /// (CONCURRENT_PUTPATH or floor-promoted) arriving more than the
    /// window after the last counted infra failure also resets
    /// `infra_count` — while itself charging only `exempt_infra_count`
    /// and leaving the window anchor where the last counted failure put
    /// it.
    #[test]
    fn exempt_infra_failure_past_the_window_resets_the_counted_budget() {
        // Counted infra failure (anchor stamped), then an exempt one
        // 350 s later — strictly past the 300 s window.
        let h = [
            infra(100, "w1", false, false),
            infra(450, "w1", true, false),
        ];
        let (c, v) = fold(&h, 450);
        assert_eq!(
            c.infra_count, 0,
            "stale-window reset fires on the exempt event"
        );
        assert_eq!(c.exempt_infra_count, 1);
        assert_eq!(
            c.last_infra_failure_at,
            Some(100),
            "the exempt event does not move the anchor"
        );
        assert_eq!(v, Verdict::Requeue);

        // Within the window the exempt event leaves the counted budget
        // untouched.
        let h = [
            infra(100, "w1", false, false),
            infra(200, "w1", true, false),
        ];
        let (c, _) = fold(&h, 200);
        assert_eq!(c.infra_count, 1, "within the window: no reset");
        assert_eq!(c.exempt_infra_count, 1);
    }

    /// The exempt arm: a CONCURRENT_PUTPATH or floor-promoted infra
    /// failure charges `exempt_infra_count` instead of `infra_count`,
    /// and the exemption's own terminal is increment-then-check — the
    /// cap fires ON the 50th exempt attempt.
    #[test]
    fn exempt_infra_charges_the_exemption_budget_and_terminates() {
        let mut h: Vec<AttemptEvent> = Vec::new();
        for i in 0..49 {
            h.push(infra(100 + i, "w1", true, false));
        }
        let (c, v) = fold(&h, 200);
        assert_eq!(c.exempt_infra_count, 49);
        assert_eq!(c.infra_count, 0, "exempt attempts skip infra_count");
        assert_eq!(
            c.last_infra_failure_at, None,
            "exempt attempts do not move the window anchor"
        );
        assert_eq!(v, Verdict::Requeue);

        h.push(infra(149, "w1", true, false));
        let (c, v) = fold(&h, 200);
        assert_eq!(c.exempt_infra_count, 50);
        assert_eq!(v, Verdict::Poison(PoisonReason::ExemptInfraBudget));
    }

    /// The timeout budget via the worker-reported path: four requeues
    /// (each charging `timeout_count`, no backoff, no exclusion), then
    /// terminal `Cancelled` — not `Poisoned` — on the fifth.
    #[test]
    fn worker_timeout_budget_four_retries_then_cancel() {
        let mk = |at| AttemptEvent::WorkerTimeout {
            at,
            executor: Some("w1".into()),
        };
        let h: Vec<AttemptEvent> = (0..5).map(|i| mk(100 + i)).collect();
        let (c, v) = fold(&h[..4], 200);
        assert_eq!(c.timeout_count, 4);
        assert_eq!(c.backoff_until, None);
        assert_eq!(v, Verdict::Requeue);

        let (c, v) = fold(&h, 200);
        assert_eq!(c.timeout_count, 4, "the terminal attempt does not charge");
        assert_eq!(v, Verdict::Cancel);
        assert_eq!(c.poisoned_at, None, "Cancelled is not Poisoned");
    }

    /// DIVERGENCE D1: the same exhausted timeout budget reached via the
    /// controller-reported backstop produces the same `Cancel` verdict
    /// from the fold — the as-built E7 poisons here, and the model's
    /// `VerdictIsChannelInvariant` check is expected to catch that
    /// deviation. The under-cap controller report charges the same
    /// counter as the worker report.
    #[test]
    fn controller_deadline_exceeded_matches_the_worker_timeout_verdict() {
        let wt = |at| AttemptEvent::WorkerTimeout {
            at,
            executor: Some("w1".into()),
        };
        let cd = |at| AttemptEvent::ControllerDeadlineExceeded {
            at,
            executor: Some("w1".into()),
        };
        // Four timeouts observed via either channel exhaust the budget;
        // the fifth observation produces Cancel regardless of channel.
        let mixed = [wt(100), cd(200), wt(300), cd(400), cd(500)];
        let (c, v) = fold(&mixed, 500);
        assert_eq!(c.timeout_count, 4);
        assert_eq!(v, Verdict::Cancel, "the fold's adjudicated D1 verdict");

        let all_worker = [wt(100), wt(200), wt(300), wt(400), wt(500)];
        let (c2, v2) = fold(&all_worker, 500);
        assert_eq!(
            (c2.timeout_count, v2),
            (c.timeout_count, v),
            "channel-invariant: same counters, same verdict"
        );
    }

    /// A poison TTL expiry: a permanent failure poisons immediately with
    /// the real `poisoned_at`; once `now` is more than the TTL past it,
    /// the verdict becomes `TtlExpire`. The permanent path records the
    /// executor for diagnostics but never charges `failure_count`.
    #[test]
    fn permanent_failure_poisons_and_the_ttl_expires_it() {
        let h = [AttemptEvent::Permanent {
            at: 1_000,
            executor: Some("w1".into()),
        }];
        let (c, v) = fold(&h, 1_000);
        assert_eq!(v, Verdict::Poison(PoisonReason::Permanent));
        assert_eq!(c.poisoned_at, Some(1_000));
        assert!(c.failed_builders.contains("w1"), "diagnostics-only insert");
        assert_eq!(c.failure_count, 0, "the permanent path never charges it");

        // Exactly at the TTL boundary the poison holds (strictly-greater
        // comparison, mirroring `duration_since(...) > POISON_TTL`).
        let ttl = Budget::default().poison_ttl_secs;
        let (_, v) = fold(&h, 1_000 + ttl);
        assert_eq!(v, Verdict::Poison(PoisonReason::Permanent));

        let (_, v) = fold(&h, 1_000 + ttl + 1);
        assert_eq!(v, Verdict::TtlExpire);
    }

    /// The per-executor exclusion set and the disconnect/backstop paths:
    /// a bare disconnect charges nothing and requeues; the backstop
    /// charges `failed_builders` + `failure_count` and its own
    /// post-record threshold re-check poisons once three distinct
    /// workers have wedged.
    #[test]
    fn disconnect_charges_nothing_and_backstop_bounds_the_wedge_loop() {
        let d = |at, ex: &str| AttemptEvent::Disconnect {
            at,
            executor: Some(ex.into()),
        };
        let b = |at, ex: &str| AttemptEvent::BackstopTimeout {
            at,
            executor: Some(ex.into()),
        };
        // Disconnects alone never accumulate anything (contradiction C2:
        // the spec says they count; the code and therefore the fold say
        // they do not).
        let (c, v) = fold(&[d(1, "w1"), d(2, "w2"), d(3, "w3"), d(4, "w4")], 10);
        assert_eq!(c, Counters::default());
        assert_eq!(v, Verdict::Requeue);

        // Backstop timeouts do accumulate, and the third distinct worker
        // trips the threshold at the backstop's own re-check.
        let (c, v) = fold(&[b(1, "w1"), b(2, "w2"), b(3, "w3")], 10);
        assert_eq!(c.failed_builders.len(), 3);
        assert_eq!(c.failure_count, 3);
        assert_eq!(v, Verdict::Poison(PoisonReason::Threshold));

        // Two backstops then a disconnect: the disconnect's re-check
        // sees only two recorded failures and requeues.
        let (c, v) = fold(&[b(1, "w1"), b(2, "w2"), d(3, "w3")], 10);
        assert_eq!(c.failed_builders.len(), 2);
        assert_eq!(v, Verdict::Requeue);
    }

    /// The resubmit reset: a poisoned derivation resubmitted by the
    /// client gets a fresh per-cycle state with `resubmit_cycles`
    /// incremented; the second cycle's budget is the full `max_retries`
    /// again.
    #[test]
    fn resubmit_reset_restores_the_per_cycle_budget_and_counts_the_cycle() {
        let h = [
            t(100, "w1"),
            t(200, "w2"),
            t(300, "w3"), // poisoned via the threshold
            AttemptEvent::ResubmitReset { at: 400 },
            t(500, "w1"),
        ];
        let (c, v) = fold(&h[..4], 400);
        assert_eq!(c.resubmit_cycles, 1);
        assert_eq!(c.count, 0);
        assert_eq!(c.failed_builders.len(), 0);
        assert_eq!(c.poisoned_at, None);
        assert_eq!(v, Verdict::Requeue);

        let (c, v) = fold(&h, 500);
        assert_eq!(c.resubmit_cycles, 1);
        assert_eq!(c.count, 1, "fresh max_retries budget");
        assert_eq!(v, Verdict::Requeue);
    }

    /// The cache-hit clear wipes nine of the ten counters but preserves
    /// `backoff_until` exactly as the as-built in-place reset did; the
    /// admin clear / TTL removal wipes all ten.
    #[test]
    fn cache_hit_clear_preserves_backoff_and_poison_clear_does_not() {
        let h = [
            t(100, "w1"), // arms backoff_until = 105
            AttemptEvent::CacheHitClear { at: 200 },
        ];
        let (c, v) = fold(&h, 200);
        assert_eq!(c.count, 0);
        assert_eq!(c.failure_count, 0);
        assert_eq!(
            c.backoff_until,
            Some(105),
            "clear() touches 9 of the 10 fields"
        );
        assert_eq!(v, Verdict::Requeue);

        let h = [t(100, "w1"), AttemptEvent::PoisonCleared { at: 200 }];
        let (c, _) = fold(&h, 200);
        assert_eq!(c, Counters::default(), "the admin clear resets everything");
    }

    /// DIVERGENCE D2 + D3 (the controller-reported OOM path): an at-cap
    /// controller termination charges `infra_count` and — in the fold,
    /// deviating from the code — stamps the window anchor; a promoted
    /// one charges `exempt_infra_count` per `sched.retry.exempt-infra-cap`
    /// — also deviating from the code, which charges nothing. A
    /// neither-promoted-nor-at-cap one charges nothing in both.
    #[test]
    fn controller_termination_charges_per_floor_outcome() {
        let ct = |at, promoted, at_cap| AttemptEvent::ControllerTermination {
            at,
            executor: Some("w1".into()),
            promoted,
            at_cap,
        };
        let (c, v) = fold(&[ct(100, false, true)], 100);
        assert_eq!(c.infra_count, 1);
        assert_eq!(c.last_infra_failure_at, Some(100), "D2: the fold stamps");
        assert_eq!(v, Verdict::Requeue);

        let (c, v) = fold(&[ct(100, true, false)], 100);
        assert_eq!(c.exempt_infra_count, 1, "D3: the fold charges");
        assert_eq!(c.infra_count, 0);
        assert_eq!(v, Verdict::Requeue);

        let (c, v) = fold(&[ct(100, false, false)], 100);
        assert_eq!(c, Counters::default());
        assert_eq!(v, Verdict::Requeue);

        // The at-cap arm shares the infra budget with E2: ten at-cap
        // controller OOMs then an eleventh poisons.
        let h: Vec<AttemptEvent> = (0..11).map(|i| ct(100 + i, false, true)).collect();
        let (c, v) = fold(&h, 200);
        assert_eq!(c.infra_count, 10);
        assert_eq!(v, Verdict::Poison(PoisonReason::InfraBudget));
    }

    // -----------------------------------------------------------------
    // Phase-1b decision surface: decide() / classify() / placeable()
    // -----------------------------------------------------------------

    /// Test-side `AttemptRecord` builder: the fields the fold consumes,
    /// everything else defaulted. `at` is epoch seconds. The identity is
    /// stamped as BOTH the executor id and the `source_node` (the
    /// controller-authoritative binding a bound pull attempt carries) —
    /// the fold keys exclusion on `source_node` only (decision P12);
    /// tests that need an unbound row clear `source_node` explicitly.
    fn rec(class: OutcomeClass, party: ReportingParty, executor: &str, at: u64) -> AttemptRecord {
        AttemptRecord {
            attempt_id: uuid::Uuid::now_v7(),
            event_kind: AttemptEventKind::Attempt,
            outcome_class: class,
            exec_id: None,
            executor_id: (!executor.is_empty()).then(|| ExecutorId::from(executor)),
            attempt_kind: AttemptKind::Build,
            source_node: (!executor.is_empty()).then(|| executor.to_string()),
            termination_reason: None,
            reporting_party: party,
            exempt: class == OutcomeClass::ExemptInfra,
            floor_promoted: false,
            floor_at_cap: false,
            error_msg: None,
            final_line_count: None,
            resubmit_cycle: 0,
            occurred_at_epoch_secs: at as f64,
            recorded_at_epoch_secs: at as f64,
        }
    }

    /// An UNBOUND attempt record: an executor identity but no
    /// controller-authoritative `source_node` (the binding-ack race, or
    /// a legacy row). Charges flat counters only (decision P12).
    fn unbound_rec(class: OutcomeClass, executor: &str, at: u64) -> AttemptRecord {
        AttemptRecord {
            source_node: None,
            ..rec(class, ReportingParty::Worker, executor, at)
        }
    }

    /// A reset-event record (`event_kind = 'reset'`).
    fn reset_rec(class: OutcomeClass, resubmit_cycle: i32, at: u64) -> AttemptRecord {
        AttemptRecord {
            event_kind: AttemptEventKind::Reset,
            resubmit_cycle,
            ..rec(class, ReportingParty::Scheduler, "", at)
        }
    }

    fn worker_rec(class: OutcomeClass, executor: &str, at: u64) -> AttemptRecord {
        rec(class, ReportingParty::Worker, executor, at)
    }

    // r[verify sched.retry.store-degraded-uncharged+3]
    /// bug_408 fold battery: N store-degraded rows ⇒ Requeue, every
    /// count counter zero, exclusion empty, never Poison; the backoff
    /// deadline follows the curve over the store-degraded count
    /// (base·multᵃ capped), computed from the LAST row's timestamp.
    #[test]
    fn store_degraded_battery_uncharged_paced_requeue() {
        let b = Budget::default(); // base 5, mult 2, cap 300
        // 12 consecutive degraded rows, 10s apart — past every cap.
        let history: Vec<AttemptRecord> = (0..12)
            .map(|i| worker_rec(OutcomeClass::StoreDegraded, "w1", 1_000 + i * 10))
            .collect();
        let d = decide(&history, &b, 2_000);
        assert_eq!(d.verdict, Verdict::Requeue, "never poisons");
        assert!(d.exclusion.is_empty(), "no exclusion key");
        assert_eq!(d.counters.count, 0);
        assert_eq!(d.counters.infra_count, 0);
        assert_eq!(d.counters.timeout_count, 0);
        assert_eq!(d.counters.exempt_infra_count, 0);
        assert_eq!(d.counters.failure_count, 0);
        assert_eq!(d.counters.poisoned_at, None);
        // Run index 11 ⇒ 5·2¹¹ ≫ 300 ⇒ capped: last row at 1110 + 300.
        assert_eq!(d.backoff_until, Some(1_110 + 300));
    }

    /// The run resets on any folded event OUTSIDE the
    /// bounded-uncharged union (a charged row): the
    /// degraded row AFTER a transient charge restarts the curve at
    /// `base` (the pacing is per-outage, not per-derivation-lifetime).
    #[test]
    fn store_degraded_run_resets_on_other_events() {
        let b = Budget::default();
        let history = vec![
            worker_rec(OutcomeClass::StoreDegraded, "w1", 1_000), // run 0: +5
            worker_rec(OutcomeClass::StoreDegraded, "w1", 1_010), // run 1: +10
            worker_rec(OutcomeClass::Transient, "w1", 1_020),     // breaks the run
            worker_rec(OutcomeClass::StoreDegraded, "w1", 1_030), // run 0 again: +5
        ];
        let d = decide(&history, &b, 1_031);
        // The transient charged normally...
        assert_eq!(d.counters.count, 1);
        assert_eq!(d.counters.failure_count, 1);
        // ...and the post-break degraded row paced from base again,
        // exceeding the transient's own (count-1=0 ⇒ base) deadline.
        assert_eq!(d.backoff_until, Some(1_030 + 5));
    }

    /// The pacing curve survives a SIBLING bounded-uncharged row
    /// (bug_098, the union law): a worker-abort free close between two
    /// store-degraded rows extends the bounded-uncharged run, so the
    /// second degraded row paces at run 1 (base·mult), not back at
    /// base — the curve escalates across the interleaving exactly as
    /// the admission counts across it. A CHARGED row still resets
    /// (the sibling test above).
    // r[verify sched.retry.store-degraded-uncharged+3]
    #[test]
    fn store_degraded_pacing_survives_uncharged_sibling() {
        let b = Budget::default(); // base 5, mult 2, cap 300
        let history = vec![
            worker_rec(OutcomeClass::StoreDegraded, "w1", 1_000), // run 0: +5
            worker_rec(OutcomeClass::Disconnected, "w1", 1_010),  // sibling: extends
            worker_rec(OutcomeClass::StoreDegraded, "w1", 1_020), // run 1: +10
        ];
        let d = decide(&history, &b, 1_021);
        // The sibling free close charged nothing...
        assert_eq!(d.counters.count, 0);
        assert_eq!(d.counters.infra_count, 0);
        // ...and the post-sibling degraded row paced at the ESCALATED
        // curve point (run 1 ⇒ +10 from its own timestamp), not reset
        // to base.
        assert_eq!(d.backoff_until, Some(1_020 + 10));
    }

    /// Classify maps the flagged event to the dedicated class with no
    /// floor sensitivity (the disposition IS the class).
    #[test]
    fn store_degraded_classifies_floor_blind() {
        for promoted in [false, true] {
            assert_eq!(
                rio_retry_kernel::classify(
                    &ObservedFailure::WorkerStoreDegraded,
                    FloorOutcomeView {
                        promoted,
                        at_cap: false
                    },
                ),
                rio_retry_kernel::OutcomeClass::StoreDegraded,
            );
        }
    }

    fn decide_default(history: &[AttemptRecord], now: AbsTime) -> Decision {
        decide(history, &Budget::default(), now)
    }

    /// An empty suffix is the default state: requeue,
    /// nothing excluded, no backoff.
    #[test]
    fn decide_empty_history_is_requeue() {
        let d = decide_default(&[], 0);
        assert_eq!(d.verdict, Verdict::Requeue);
        assert!(d.exclusion.is_empty());
        assert_eq!(d.backoff_until, None);
        assert_eq!(d.counters, Counters::default());
    }

    /// decide() over worker-reported ledger rows reproduces the
    /// reference fold's hand-computed transient-budget history: two
    /// same-worker retries with backoff, poison via the per-cycle cap on
    /// the third, and the executor lands in the exclusion set.
    #[test]
    fn decide_transient_budget_matches_the_reference_fold() {
        let h = [
            worker_rec(OutcomeClass::Transient, "w1", 100),
            worker_rec(OutcomeClass::Transient, "w1", 200),
            worker_rec(OutcomeClass::Transient, "w1", 300),
        ];
        let oracle_events = [t(100, "w1"), t(200, "w1"), t(300, "w1")];

        let d = decide_default(&h[..2], 200);
        let (oc, ov) = fold(&oracle_events[..2], 200);
        assert_eq!(d.counters, oc, "counters match the reference fold");
        assert_eq!(d.verdict, ov);
        assert_eq!(d.backoff_until, Some(210));
        assert!(d.exclusion.contains(&ExecutorId::from("w1")));

        let d = decide_default(&h, 300);
        let (oc, ov) = fold(&oracle_events, 300);
        assert_eq!(d.counters, oc);
        assert_eq!(d.verdict, ov);
        assert_eq!(d.verdict, Verdict::Poison(PoisonReason::TransientBudget));
    }

    /// decide() over the worker infra family: the non-exempt budget is
    /// check-then-increment with the 300 s window, exactly as the fold's
    /// infra history; exempt rows charge only the exemption budget.
    #[test]
    fn decide_infra_budget_and_window_match_the_reference_fold() {
        let mut h: Vec<AttemptRecord> = (0..10)
            .map(|i| worker_rec(OutcomeClass::Infra, "w1", 100 + i))
            .collect();
        let d = decide_default(&h, 200);
        assert_eq!(d.counters.infra_count, 10);
        assert_eq!(d.counters.last_infra_failure_at, Some(109));
        assert_eq!(d.verdict, Verdict::Requeue);
        assert!(
            d.exclusion.is_empty(),
            "infra failures never join the exclusion set"
        );

        h.push(worker_rec(OutcomeClass::Infra, "w1", 110));
        let d = decide_default(&h, 200);
        assert_eq!(d.verdict, Verdict::Poison(PoisonReason::InfraBudget));

        // Sparse failures past the window are forgiven before charging.
        let sparse = [
            worker_rec(OutcomeClass::Infra, "w1", 100),
            worker_rec(OutcomeClass::Infra, "w1", 452),
        ];
        let d = decide_default(&sparse, 452);
        assert_eq!(d.counters.infra_count, 1, "window reset then charge");
    }

    /// The worker timeout budget through decide(): four requeues then
    /// terminal Cancel, never Poisoned.
    #[test]
    fn decide_worker_timeout_cap_is_cancel() {
        let h: Vec<AttemptRecord> = (0..5)
            .map(|i| worker_rec(OutcomeClass::Timeout, "w1", 100 + i))
            .collect();
        let d = decide_default(&h[..4], 200);
        assert_eq!(d.counters.timeout_count, 4);
        assert_eq!(d.verdict, Verdict::Requeue);
        let d = decide_default(&h, 200);
        assert_eq!(d.verdict, Verdict::Cancel);
        assert_eq!(d.counters.poisoned_at, None);
    }

    /// A permanent failure poisons immediately and the TTL downgrade
    /// works through decide()'s `now`.
    #[test]
    fn decide_permanent_poisons_and_ttl_expires() {
        let h = [worker_rec(OutcomeClass::Permanent, "w1", 1_000)];
        let d = decide_default(&h, 1_000);
        assert_eq!(d.verdict, Verdict::Poison(PoisonReason::Permanent));
        let ttl = Budget::default().poison_ttl_secs;
        let d = decide_default(&h, 1_000 + ttl + 1);
        assert_eq!(d.verdict, Verdict::TtlExpire);
    }

    /// A promoted / CONCURRENT_PUTPATH exempt-infra history: classify()
    /// maps it to the exempt class, and decide() charges only
    /// `exempt_infra_count` — never the transient count, never the
    /// non-exempt infra count.
    #[test]
    fn decide_exempt_infra_history_charges_only_the_exemption_budget() {
        // classify(): both exemption vehicles map to ExemptInfra.
        assert_eq!(
            classify(
                &ObservedFailure::WorkerInfra { error_msg: "boom" },
                FloorOutcomeView {
                    promoted: true,
                    at_cap: false
                }
            ),
            OutcomeClass::ExemptInfra
        );
        let putpath_msg = format!("upload failed: {}", rio_proto::CONCURRENT_PUTPATH_MSG);
        assert_eq!(
            classify(
                &ObservedFailure::WorkerInfra {
                    error_msg: &putpath_msg
                },
                FloorOutcomeView::default()
            ),
            OutcomeClass::ExemptInfra
        );

        let h: Vec<AttemptRecord> = (0..3)
            .map(|i| worker_rec(OutcomeClass::ExemptInfra, "w1", 100 + i))
            .collect();
        let d = decide_default(&h, 200);
        assert_eq!(d.counters.exempt_infra_count, 3);
        assert_eq!(d.counters.infra_count, 0);
        assert_eq!(d.counters.count, 0, "never the transient budget");
        assert_eq!(d.verdict, Verdict::Requeue);
    }

    /// The controller-classified rows: a promoted controller termination
    /// (class `exempt_infra`, scheduler/controller-reported) charges the
    /// exemption budget — divergence D3's adjudicated side, visible to
    /// any collapsed reader of the exempt cap; an unestablished
    /// `disconnected` row charges nothing.
    #[test]
    fn decide_controller_promoted_termination_charges_the_exemption_budget() {
        let h = [
            rec(
                OutcomeClass::ExemptInfra,
                ReportingParty::Scheduler,
                "w1",
                100,
            ),
            rec(
                OutcomeClass::Disconnected,
                ReportingParty::Scheduler,
                "w1",
                110,
            ),
        ];
        let d = decide_default(&h, 120);
        assert_eq!(d.counters.exempt_infra_count, 1, "D3: the fold charges");
        assert_eq!(d.counters.infra_count, 0);
        assert_eq!(d.counters.failure_count, 0);
        assert_eq!(d.verdict, Verdict::Requeue);
    }

    /// Backstop rows charge the threshold/exclusion budget and the third
    /// distinct wedged worker poisons; cascade and fleet-exhaust marker
    /// rows are no-ops for the fold.
    #[test]
    fn decide_backstop_rows_bound_the_wedge_loop_and_markers_are_noops() {
        let h = [
            rec(OutcomeClass::Backstop, ReportingParty::Scheduler, "w1", 10),
            rec(OutcomeClass::Cascade, ReportingParty::Scheduler, "", 11),
            rec(OutcomeClass::Backstop, ReportingParty::Scheduler, "w2", 20),
            rec(
                OutcomeClass::FleetExhaust,
                ReportingParty::Scheduler,
                "",
                21,
            ),
            rec(OutcomeClass::Backstop, ReportingParty::Scheduler, "w3", 30),
        ];
        let d = decide_default(&h[..4], 25);
        assert_eq!(d.counters.failed_builders.len(), 2);
        assert_eq!(d.verdict, Verdict::Requeue);
        let d = decide_default(&h, 35);
        assert_eq!(d.counters.failed_builders.len(), 3);
        assert_eq!(d.verdict, Verdict::Poison(PoisonReason::Threshold));
        assert_eq!(d.exclusion.len(), 3);
    }

    // r[verify sched.retry.per-executor-budget+4]
    /// C2 (T-1b.11): an `executor_crash` history charges the
    /// threshold/exclusion budget — each established crash joins
    /// `failed_builders` and increments `failure_count`, the placement
    /// exclusion picks the crashing executors up, and the third
    /// distinct establishment crosses the default threshold. A bare
    /// `disconnected` row (not yet established) still charges nothing.
    #[test]
    fn decide_executor_crash_history_charges_the_threshold_budget() {
        let ex = |ids: &[&str]| -> BTreeSet<ExecutorId> {
            ids.iter().map(|s| ExecutorId::from(*s)).collect()
        };
        let crash = |w: &str, at: u32| {
            rec(
                OutcomeClass::ExecutorCrash,
                ReportingParty::Scheduler,
                w,
                u64::from(at),
            )
        };
        // Two establishments: charged and excluded, still under the
        // threshold; the fold-view exclusion feeds placeable().
        let h = [crash("w0", 100), crash("w1", 101)];
        let d = decide_default(&h, 200);
        assert_eq!(d.counters.failure_count, 2);
        assert_eq!(d.counters.failed_builders.len(), 2);
        assert_eq!(d.verdict, Verdict::Requeue);
        assert!(d.exclusion.contains(&ExecutorId::from("w0")));
        assert!(d.exclusion.contains(&ExecutorId::from("w1")));
        assert_eq!(
            placeable(&d.exclusion, &ex(&["w0", "w1"])),
            Placement::FleetExhausted,
            "a fleet consisting only of crashed executors reads exhausted"
        );
        assert_eq!(
            placeable(&d.exclusion, &ex(&["w0", "w1", "w-fresh"])),
            Placement::Placeable,
            "a fresh executor keeps the derivation placeable"
        );

        // The third distinct establishment crosses the threshold (the
        // C2 boundedness the as-built code lacked).
        let h = [crash("w0", 100), crash("w1", 101), crash("w2", 102)];
        let d = decide_default(&h, 200);
        assert_eq!(d.verdict, Verdict::Poison(PoisonReason::Threshold));
        assert_eq!(d.counters.failed_builders.len(), 3);

        // Unestablished disconnects stay uncharged.
        let h = [
            rec(
                OutcomeClass::Disconnected,
                ReportingParty::Scheduler,
                "w0",
                100,
            ),
            rec(
                OutcomeClass::Disconnected,
                ReportingParty::Scheduler,
                "w1",
                101,
            ),
        ];
        let d = decide_default(&h, 200);
        assert_eq!(d.counters, Counters::default());
        assert!(d.exclusion.is_empty());
    }

    /// A suffix that begins with a resubmit-reset row seeds
    /// `resubmit_cycles` from the row's cycle index, and the per-cycle
    /// budget is fresh.
    #[test]
    fn decide_leading_resubmit_reset_seeds_the_cycle_index() {
        let h = [
            reset_rec(OutcomeClass::ResubmitReset, 2, 400),
            worker_rec(OutcomeClass::Transient, "w1", 500),
        ];
        let d = decide_default(&h, 500);
        assert_eq!(d.counters.resubmit_cycles, 2, "carried from the row");
        assert_eq!(d.counters.count, 1, "fresh per-cycle budget");
        assert_eq!(d.verdict, Verdict::Requeue);

        // Cache-hit and poison-clear reset rows behave as the fold's
        // clear events (no cycle carried).
        let h = [
            worker_rec(OutcomeClass::Transient, "w1", 100),
            reset_rec(OutcomeClass::CacheHitClear, 0, 200),
        ];
        let d = decide_default(&h, 200);
        assert_eq!(d.counters.count, 0);
        assert_eq!(d.counters.backoff_until, Some(105), "clear() keeps backoff");
    }

    /// classify() is total over the trigger alphabet and never lets a
    /// transient failure consult the floor (P4).
    #[test]
    fn classify_maps_every_trigger_and_transients_never_consult_the_floor() {
        let promoted = FloorOutcomeView {
            promoted: true,
            at_cap: false,
        };
        let none = FloorOutcomeView::default();
        assert_eq!(
            classify(&ObservedFailure::WorkerTransient, promoted),
            OutcomeClass::Transient,
            "P4: a promoted floor never reclassifies a transient failure"
        );
        assert_eq!(
            classify(&ObservedFailure::WorkerInfra { error_msg: "eio" }, none),
            OutcomeClass::Infra
        );
        assert_eq!(
            classify(&ObservedFailure::WorkerPermanent, promoted),
            OutcomeClass::Permanent
        );
        assert_eq!(
            classify(&ObservedFailure::WorkerTimeout, promoted),
            OutcomeClass::Timeout
        );
        assert_eq!(
            classify(&ObservedFailure::Disconnect, none),
            OutcomeClass::Disconnected
        );
        assert_eq!(
            classify(&ObservedFailure::ControllerResourceTermination, promoted),
            OutcomeClass::ExemptInfra
        );
        assert_eq!(
            classify(&ObservedFailure::ControllerResourceTermination, none),
            OutcomeClass::Infra
        );
        assert_eq!(
            classify(&ObservedFailure::ControllerDeadlineExceeded, none),
            OutcomeClass::Timeout
        );
        assert_eq!(
            classify(&ObservedFailure::BackstopTimeout, none),
            OutcomeClass::Backstop
        );
        assert_eq!(
            classify(&ObservedFailure::UnreportedCrash, none),
            OutcomeClass::ExecutorCrash
        );
    }

    /// placeable(): the three-way placement verdict mirrors the as-built
    /// fleet-exhaust predicate, including both empty-set carve-outs.
    #[test]
    fn placeable_mirrors_the_fleet_exhaust_predicate() {
        let ex = |ids: &[&str]| -> BTreeSet<ExecutorId> {
            ids.iter().map(|s| ExecutorId::from(*s)).collect()
        };
        assert_eq!(
            placeable(&ex(&["w1", "w2"]), &ex(&["w1", "w2"])),
            Placement::FleetExhausted
        );
        assert_eq!(
            placeable(&ex(&["w1"]), &ex(&["w1", "w2"])),
            Placement::Placeable
        );
        assert_eq!(
            placeable(&ex(&[]), &ex(&["w1"])),
            Placement::Placeable,
            "nothing failed yet: never exhausted"
        );
        assert_eq!(
            placeable(&ex(&["w1", "w2"]), &ex(&[])),
            Placement::NoEligibleWorkers,
            "empty pool is a provisioning transient, never a poison"
        );
        // The exclusion set may contain workers no longer in the fleet.
        assert_eq!(
            placeable(&ex(&["gone-1", "gone-2"]), &ex(&["w-fresh"])),
            Placement::Placeable
        );
    }

    // r[verify sched.retry.per-executor-budget+4]
    /// Decision P12: the exclusion/budget key is the
    /// controller-authoritative `source_node` ONLY. A row that carries
    /// one contributes that node; a row that carries only an executor
    /// identity (a legacy stream-era pod name, or a pull attempt whose
    /// binding ack has not landed) contributes NO exclusion key — it
    /// still charges the flat `failure_count`, but it cannot occupy a
    /// distinct-source slot or leak a non-schedulable key into the
    /// placement exclusion.
    #[test]
    fn exclusion_keys_are_source_nodes_only() {
        let unbound = unbound_rec(OutcomeClass::Transient, "pool-pod-1", 100);
        let mut bound = worker_rec(OutcomeClass::Transient, "drv-hash-x", 200);
        bound.source_node = Some("node-1".into());
        let d = decide_default(&[unbound.clone(), bound], 300);
        assert!(
            d.exclusion.contains(&ExecutorId::from("node-1")),
            "a bound row contributes its source node"
        );
        assert!(
            !d.exclusion.contains(&ExecutorId::from("pool-pod-1")),
            "an executor-only row contributes no exclusion key (P12: the \
             pod-name fallback is gone)"
        );
        assert!(
            !d.exclusion.contains(&ExecutorId::from("drv-hash-x")),
            "the pull identity (the intent) is never an exclusion key"
        );
        assert_eq!(
            d.counters.failed_builders.len(),
            1,
            "only the node key occupies a distinct-source slot"
        );
        assert_eq!(
            d.counters.failure_count, 2,
            "the identity-less failure still charges the flat counter"
        );

        // The old fallback must not influence the verdict either: three
        // unbound failures from three distinct pod names never cross the
        // distinct-source threshold (only the per-cycle transient cap
        // bounds them), where the pre-P12 fallback would have poisoned
        // via three distinct pod-name keys.
        let h = [
            unbound_rec(OutcomeClass::Transient, "pod-a", 100),
            unbound_rec(OutcomeClass::Transient, "pod-b", 200),
            unbound_rec(OutcomeClass::Transient, "pod-c", 300),
        ];
        let d = decide_default(&h, 300);
        assert!(
            d.exclusion.is_empty(),
            "unbound failures contribute no distinct-source keys"
        );
        assert_eq!(
            d.verdict,
            Verdict::Poison(PoisonReason::TransientBudget),
            "the per-cycle transient cap still bounds an identity-less loop"
        );
    }

    // r[verify sched.dispatch.fleet-exhaust+5]
    // r[verify sched.retry.per-executor-budget+4]
    /// AD2 small-fleet clause over the re-keyed inputs: with a single
    /// spawnable source the exhaustion verdict is reachable after that
    /// one source fails (min(threshold, |sources|) = 1), and the empty
    /// universe still defers rather than poisons.
    #[test]
    fn re_keyed_exhaustion_fires_with_single_source() {
        let ex = |ids: &[&str]| -> BTreeSet<ExecutorId> {
            ids.iter().map(|s| ExecutorId::from(*s)).collect()
        };
        // One pull-mode failure on the only node in the universe.
        let mut row = worker_rec(OutcomeClass::Transient, "drv-hash-y", 100);
        row.source_node = Some("node-only".into());
        let d = decide_default(&[row], 200);
        assert_eq!(
            placeable(&d.exclusion, &ex(&["node-only"])),
            Placement::FleetExhausted,
            "|sources| = 1: exhaustion fires once the single source has failed"
        );
        assert_eq!(
            placeable(&d.exclusion, &ex(&[])),
            Placement::NoEligibleWorkers,
            "an empty spawnable universe defers, never poisons"
        );
    }

    /// The kernel crate cannot depend on rio-proto, so it carries its
    /// own copy of the CONCURRENT_PUTPATH error-message marker that
    /// `classify()`'s exemption predicate greps for. Pin the two
    /// constants together: if the store's message (and therefore
    /// `rio_proto::CONCURRENT_PUTPATH_MSG`) ever changes, this test
    /// fails until the kernel's copy is updated in the same change.
    #[test]
    fn concurrent_putpath_marker_matches_rio_proto() {
        assert_eq!(
            rio_retry_kernel::CONCURRENT_PUTPATH_MSG,
            rio_proto::CONCURRENT_PUTPATH_MSG,
            "rio-retry-kernel's CONCURRENT_PUTPATH marker must stay in \
             lockstep with rio_proto::CONCURRENT_PUTPATH_MSG"
        );
    }

    /// merged_bug_020 differential: the in-memory `AttemptRecord`
    /// projection (`record_to_row`) and the kernel fold agree —
    /// [`materialization_counters`] over a mixed two-lane history
    /// counts exactly the mat-lane rows since the LAST mat-lane reset
    /// (party split included); build rows and build resets neither
    /// count nor cut, and the cut index matches
    /// [`materialization_window_start`].
    #[test]
    fn materialization_counters_projection_differential() {
        use crate::db::attempts::AttemptRow;
        use crate::state::{AttemptKind as K, OutcomeClass as C, ReportingParty as P};
        let d = uuid::Uuid::new_v4();
        let history: Vec<crate::state::AttemptRecord> = vec![
            // Pre-window: a mat infra charge, then a BUILD reset (must
            // not cut the mat lane), then a mat unobtainable.
            AttemptRow::new(d, C::MaterializationInfra, P::Worker, K::Materialization).to_record(),
            AttemptRow::new_reset(d, C::ResubmitReset, P::Scheduler, 1, K::Build).to_record(),
            AttemptRow::new(
                d,
                C::MaterializationUnobtainable,
                P::Worker,
                K::Materialization,
            )
            .to_record(),
            // THE mat-lane reset (migration 085 job creation): the cut.
            AttemptRow::new_reset(
                d,
                C::MaterializationReset,
                P::Scheduler,
                0,
                K::Materialization,
            )
            .to_record(),
            // Window: 1 worker infra + 1 scheduler (establishment)
            // infra + 1 unobtainable + build noise.
            AttemptRow::new(d, C::MaterializationInfra, P::Worker, K::Materialization).to_record(),
            AttemptRow::new(d, C::MaterializationInfra, P::Scheduler, K::Materialization)
                .to_record(),
            AttemptRow::new(
                d,
                C::MaterializationUnobtainable,
                P::Worker,
                K::Materialization,
            )
            .to_record(),
            AttemptRow::new(d, C::Infra, P::Worker, K::Build).to_record(),
        ];
        let c = materialization_counters(&history);
        assert_eq!(c.infra_since_reset, 2, "both parties charge the window");
        assert_eq!(c.worker_infra_since_reset, 1, "the Item-T worker subset");
        assert_eq!(
            c.unobtainable_since_reset, 1,
            "pre-window unobtainable cut away"
        );
        assert_eq!(
            materialization_window_start(&history),
            3,
            "the cut anchors at the last mat-lane reset row"
        );
    }
}
