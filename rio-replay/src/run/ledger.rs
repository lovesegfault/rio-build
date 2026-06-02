//! The job ledger: the single owner of job scheduling-state transitions.
//!
//! The campaign engine tracks per-job scheduling state in two structures —
//! the [`SubmitTracker`] (in-flight reservations, engine resubmission
//! counts, post-settlement cool-downs) and the [`Watchdog`] (per-job stall
//! clocks keyed by phase). Historically the watchdog's view was
//! reconstructed every poll tick from set membership (`results ∪
//! in_flight`), which structurally cannot cover jobs that are in neither
//! set: a job re-queued from a settled batch kept accruing `Active` time
//! while it actually sat in the pending pool, eventually burning its retry
//! budget and retiring with a spurious stalled-active record.
//!
//! The ledger replaces reconstruction with transition-site observation:
//! every job-state transition goes through a ledger method that updates the
//! tracker AND emits the matching watchdog phase observation, so a job
//! cannot change scheduling state without the watchdog hearing it —
//!
//! - [`JobLedger::commit_batch`]: a batch is committed for submission — its
//!   members become in-flight and are observed `Active`. This is the ONLY
//!   place a watchdog clock is created, so a clock can never start before
//!   the job's first offer (a never-submitted job is simply not tracked,
//!   and queued time before first submission is never charged).
//! - [`JobLedger::requeue_collected`]: collect re-offers a settled batch's
//!   member — the resubmission is counted and the job is observed `Queued`
//!   (the phase change resets its stall clock, routing the wait to the
//!   queued-watchdog path designed for it).
//! - [`JobLedger::requeue_stalled`]: the active-stall auto-retry — the
//!   in-flight reservation is released, the resubmission counted, and the
//!   job observed `Queued`.
//! - [`JobLedger::retire`]: the job reached a terminal record — it leaves
//!   the watchdog and the in-flight set. The record append itself stays
//!   with the caller (collect / the stall writer), which holds the context
//!   a record needs.
//!
//! Settlement (`release_after_settle`) is deliberately NOT a phase
//! transition: a settled-but-unclassified member stays `Active` for the one
//! collect-poll window until [`process_settled_batch`] decides requeue vs
//! terminal — a bounded sliver against multi-hour stall thresholds.
//!
//! Restart durability: both requeue transitions journal a
//! [`RequeueRecord`] to requeues.jsonl BEFORE moving the in-memory counter,
//! and [`JobLedger::from_journals`] rebuilds the counters as a pure fold of
//! that stream. The resubmission counts back documented convergence bounds
//! (the infra auto-retry budget, fail-fast singleton isolation, the stall
//! auto-retry gate); the user-facing `attempts`/`flaky` measurement is a
//! SEPARATE projection of the same journal ([`measured_attempt_requeues`]),
//! folding only cluster-attempt reasons — one substrate, two consumers
//! with explicitly different reason semantics. Deriving any of it from
//! volatile state would zero every consumed budget at each pod restart —
//! the exact spin the budget exists to prevent, reopened at the restart
//! edge.
//! Journal-then-increment keeps the invariant `in-memory counters ==
//! fold(requeues.jsonl)` under append failures and batch re-processing
//! alike; the equivalence test in `super::tests` pins it at every batch
//! boundary. One residual asymmetry is deliberate: a crash after the
//! journal append but before the batch lands in collected.json re-processes
//! that settle on resume and may journal the same decision again — a
//! one-unit budget over-charge in the conservative direction (a job can be
//! retired one retry early, never granted an extra retry or spun), and it
//! converges because the fold and the in-memory counters inflate together.
//!
//! Lock discipline: every method takes its locks strictly sequentially
//! (acquire, update, release — never two at once), so the ledger can be
//! called from the submit loop, the collect pass, and the poller without
//! ordering constraints against the poller's own watchdog lock.
//!
//! [`process_settled_batch`]: super::collect::process_settled_batch

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;

use super::model::{
    REQUEUE_SOURCE_COLLECT, REQUEUE_SOURCE_STALL, RequeueReason, RequeueRecord, now_rfc3339,
};
use super::state::{StateDir, StateFile};
use super::submit::SubmitTracker;
use super::watchdog::{JobPhase, Watchdog};

/// Per-job CLUSTER-ATTEMPT requeues: the measurement projection of
/// requeues.jsonl, folding only the entries whose reason
/// [`RequeueReason::counts_as_cluster_attempt`] admits. This — never the
/// tracker's resubmission counter — feeds the `attempts`/`flaky` stamped on
/// results.jsonl records: the budget counter deliberately counts every
/// reason (engine-cancelled batches and engine-side submission failures
/// included), which is correct for bounding re-offers but marks
/// first-real-attempt successes flaky when reused as the measurement.
///
/// An entry whose `why` is outside the vocabulary (a journal written by a
/// different engine version) counts, preserving the historical
/// every-requeue semantics for foreign entries; a state dir without the
/// journal folds to empty counts.
pub fn measured_attempt_requeues(state: &StateDir) -> Result<HashMap<String, u32>> {
    let entries: Vec<RequeueRecord> = state.load_jsonl(StateFile::Requeues)?;
    let mut counts: HashMap<String, u32> = HashMap::new();
    for entry in &entries {
        if RequeueReason::from_wire(&entry.why).is_none_or(RequeueReason::counts_as_cluster_attempt)
        {
            *counts.entry(entry.job.clone()).or_default() += 1;
        }
    }
    Ok(counts)
}

/// The `attempts` value stamped on a results.jsonl record — the ONE
/// stamping convention for every record writer (collect terminals and the
/// stall writer alike, so the two cannot diverge on +1 conventions again):
/// the job's prior cluster-attempt requeues
/// ([`measured_attempt_requeues`]) plus the current attempt when the job
/// is, or just was, committed to a batch. Collect terminals always count
/// the current attempt (the record is about the submission that just
/// settled — for the submission-failure exhaustion arm, the final failed
/// submission itself); a stalled-active terminal counts its committed,
/// stalled attempt; a stalled-queued terminal has no current attempt (the
/// job sat in the queue since its last requeue).
pub fn stamped_attempts(prior_cluster_requeues: u32, currently_committed: bool) -> u32 {
    prior_cluster_requeues + u32::from(currently_committed)
}

/// Owner of job scheduling-state transitions. See the module docs.
pub struct JobLedger {
    /// Journal substrate: every requeue transition appends its
    /// [`RequeueRecord`] here before the in-memory counter moves.
    state: StateDir,
    tracker: Arc<SubmitTracker>,
    watchdog: Arc<tokio::sync::Mutex<Watchdog>>,
}

impl JobLedger {
    /// Construct over an explicit tracker (tests and call sites that build
    /// their own scheduling state). The production resume path MUST use
    /// [`JobLedger::from_journals`] instead, so the counters are seeded
    /// from the durable journal rather than starting empty.
    pub fn new(
        state: StateDir,
        tracker: Arc<SubmitTracker>,
        watchdog: Arc<tokio::sync::Mutex<Watchdog>>,
    ) -> Self {
        Self {
            state,
            tracker,
            watchdog,
        }
    }

    /// The production constructor: rebuild the resubmission counters as a
    /// fold of requeues.jsonl, so every bound they back (the infra
    /// auto-retry budget, fail-fast singleton isolation, `attempts`
    /// accounting) survives a pod restart with its consumed budget intact.
    /// Returns the ledger plus the per-job stall auto-retry counts (the
    /// `REQUEUE_SOURCE_STALL` slice of the same fold) for the poller's
    /// stall gate — one journal rehydrates both counter families.
    ///
    /// The fold covers every batch kind by construction: increments are
    /// journaled at the transition site, not re-derived per batch, so
    /// timeless collect re-offers and timed-mode stall retries land in the
    /// same stream. Deliberately NOT rehydrated: in-flight reservations
    /// (nothing is in flight in a fresh process), watchdog clocks (builds
    /// restart, so stall baselines must too), and post-settlement
    /// cool-downs (sub-minute re-offer dampers; at worst one early
    /// re-offer races the first collect pass, the same window the live
    /// cool-down already permits at expiry). A state dir from before this
    /// journal existed folds to empty counters — the pre-journal resume
    /// behavior, degraded but never worse.
    pub fn from_journals(
        state: StateDir,
        watchdog: Arc<tokio::sync::Mutex<Watchdog>>,
    ) -> Result<(Self, HashMap<String, u32>)> {
        let entries: Vec<RequeueRecord> = state.load_jsonl(StateFile::Requeues)?;
        let mut resubmissions: HashMap<String, u32> = HashMap::new();
        let mut stall_retries: HashMap<String, u32> = HashMap::new();
        for entry in &entries {
            *resubmissions.entry(entry.job.clone()).or_default() += 1;
            if entry.source == REQUEUE_SOURCE_STALL {
                *stall_retries.entry(entry.job.clone()).or_default() += 1;
            }
        }
        let tracker = Arc::new(SubmitTracker {
            resubmissions: tokio::sync::Mutex::new(resubmissions),
            ..SubmitTracker::default()
        });
        Ok((
            Self {
                state,
                tracker,
                watchdog,
            },
            stall_retries,
        ))
    }

    /// Append one requeue transition to the journal. Called BEFORE the
    /// in-memory increment: an append failure leaves the counter untouched
    /// (the caller retries the whole transition), so the in-memory state
    /// can never run ahead of the fold that resume rebuilds it from.
    fn journal_requeue(&self, job: &str, source: &str, why: &str) -> Result<()> {
        self.state.append_jsonl(
            StateFile::Requeues,
            &RequeueRecord {
                job: job.to_string(),
                source: source.to_string(),
                why: why.to_string(),
                at: now_rfc3339(),
            },
        )
    }

    /// Read-side view of the scheduling state (wave snapshots, cool-downs,
    /// resubmission counts, settlement release). Mutating the tracker's
    /// sets directly instead of going through a transition method loses the
    /// watchdog observation — only [`super::submit::submit_one_batch`]'s
    /// settlement release legitimately bypasses the ledger (settlement is
    /// not a phase transition).
    pub fn tracker(&self) -> &SubmitTracker {
        &self.tracker
    }

    /// The shared tracker handle, for the call sites that need to hold one
    /// across a spawned task.
    pub fn tracker_arc(&self) -> Arc<SubmitTracker> {
        self.tracker.clone()
    }

    /// Commit a batch for submission: reserve its jobs in-flight and
    /// observe them `Active`. The first commit of a job is what creates its
    /// watchdog clock — queued time before the first offer is never
    /// charged.
    pub async fn commit_batch(&self, jobs: &[String]) {
        {
            let mut in_flight = self.tracker.in_flight.lock().await;
            for job in jobs {
                in_flight.insert(job.clone());
            }
        }
        let mut wd = self.watchdog.lock().await;
        for job in jobs {
            wd.observe_job(job, JobPhase::Active);
        }
    }

    /// Collect re-offers a settled batch's member to the pending pool:
    /// journal the requeue, count the engine resubmission, and observe the
    /// job `Queued` (the phase change resets its stall clock). `why` is
    /// the collect decision's requeue reason; the journaled string feeds
    /// the measurement projection ([`measured_attempt_requeues`]) on top
    /// of its archaeological value.
    pub async fn requeue_collected(&self, job: &str, why: RequeueReason) -> Result<()> {
        self.journal_requeue(job, REQUEUE_SOURCE_COLLECT, why.as_str())?;
        *self
            .tracker
            .resubmissions
            .lock()
            .await
            .entry(job.to_string())
            .or_default() += 1;
        self.watchdog
            .lock()
            .await
            .observe_job(job, JobPhase::Queued);
        Ok(())
    }

    /// A failed canary probe re-offers its member WITHOUT journaling or
    /// counting: the probe's infra-shaped failure is evidence about the
    /// outage being probed, not about the job, so no budget moves — and
    /// `fold(requeues.jsonl) == live counters` holds because neither side
    /// moves. The job is still observed `Queued` so its clock leaves the
    /// settled batch's `Active` phase (the probe ladder, not the stall
    /// ladder, owns the outage's convergence — and the backpressure pause
    /// freezes stall clocks while the latch holds anyway).
    pub async fn requeue_probe_exempt(&self, job: &str) {
        self.watchdog
            .lock()
            .await
            .observe_job(job, JobPhase::Queued);
    }

    /// The active-stall auto-retry: journal the requeue, release the
    /// in-flight reservation, count the resubmission, and observe the job
    /// `Queued` so its wait for the fresh batch is charged to the queued
    /// clock, not a stale `Active` one.
    pub async fn requeue_stalled(&self, job: &str) -> Result<()> {
        self.journal_requeue(
            job,
            REQUEUE_SOURCE_STALL,
            RequeueReason::ActiveStall.as_str(),
        )?;
        self.tracker.in_flight.lock().await.remove(job);
        *self
            .tracker
            .resubmissions
            .lock()
            .await
            .entry(job.to_string())
            .or_default() += 1;
        self.watchdog
            .lock()
            .await
            .observe_job(job, JobPhase::Queued);
        Ok(())
    }

    /// The job reached a terminal record (already appended by the caller):
    /// stop tracking it. Removing an absent in-flight entry is a no-op, so
    /// retirement is uniform across collect terminals (reservation already
    /// released at settle) and stall terminals (reservation still held).
    pub async fn retire(&self, job: &str) {
        self.watchdog.lock().await.remove_job(job);
        self.tracker.in_flight.lock().await.remove(job);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::spec::Knobs;
    use crate::run::watchdog::{IcePoll, PollTick, StallKind};

    fn ledger() -> (
        tempfile::TempDir,
        JobLedger,
        Arc<SubmitTracker>,
        Arc<tokio::sync::Mutex<Watchdog>>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let tracker = Arc::new(SubmitTracker::default());
        let watchdog = Arc::new(tokio::sync::Mutex::new(Watchdog::new(Knobs {
            active_stall_hours: 6.0,
            queued_watchdog_hours: 2.0,
            max_queued_requeues: 2,
            ..Knobs::default()
        })));
        (
            dir,
            JobLedger::new(state, tracker.clone(), watchdog.clone()),
            tracker,
            watchdog,
        )
    }

    fn tick(at: i64) -> PollTick {
        PollTick {
            at_unix: at,
            cluster: None,
            ice: IcePoll::NotPolled,
            engine_paused: false,
        }
    }

    /// A queued clock never starts before the job's first offer: only
    /// commit_batch creates watchdog entries, so a job sitting in the
    /// pending pool from t0 produces no stall verdicts no matter how long
    /// the campaign runs.
    #[tokio::test]
    async fn never_submitted_jobs_are_not_tracked() {
        let (_dir, ledger, _tracker, watchdog) = ledger();
        // Another job IS committed, so the watchdog has work to do.
        ledger.commit_batch(&["active.x".to_string()]).await;
        let mut wd = watchdog.lock().await;
        wd.on_tick(&tick(0));
        // 100 unsuspended hours: enough to trip every threshold many times.
        let outcome = wd.on_tick(&tick(100 * 3600));
        assert!(
            outcome.stalled.iter().all(|stall| stall.job == "active.x"),
            "only the committed job may ever stall: {:?}",
            outcome.stalled
        );
    }

    /// commit_batch reserves in-flight and observes Active; retire drops
    /// both, whether or not the reservation was already released.
    #[tokio::test]
    async fn commit_and_retire_are_uniform() {
        let (_dir, ledger, tracker, watchdog) = ledger();
        ledger.commit_batch(&["a.x".to_string()]).await;
        assert!(tracker.in_flight.lock().await.contains("a.x"));
        assert_eq!(
            watchdog.lock().await.phase_of("a.x"),
            Some(JobPhase::Active)
        );
        ledger.retire("a.x").await;
        assert!(!tracker.in_flight.lock().await.contains("a.x"));
        assert_eq!(watchdog.lock().await.phase_of("a.x"), None);
        // Retiring a job whose reservation was already released at settle
        // is the collect-terminal shape: still a no-op-safe removal.
        ledger.commit_batch(&["b.x".to_string()]).await;
        tracker.in_flight.lock().await.remove("b.x");
        ledger.retire("b.x").await;
        assert_eq!(watchdog.lock().await.phase_of("b.x"), None);
    }

    /// The bug_111 shape end-to-end at the ledger level: a job committed
    /// (Active), settled, and re-queued by collect transitions to Queued
    /// with its clock reset — its pending wait is charged to the queued
    /// watchdog (bounded, designed for it), never to a stale Active clock
    /// (whose stall would burn the infra retry budget).
    #[tokio::test]
    async fn requeue_transitions_to_queued_and_resets_the_clock() {
        let (_dir, ledger, tracker, watchdog) = ledger();
        ledger.commit_batch(&["job.x".to_string()]).await;
        {
            let mut wd = watchdog.lock().await;
            wd.on_tick(&tick(0));
            // 5h Active: under the 6h active threshold, nothing fires.
            assert!(wd.on_tick(&tick(5 * 3600)).stalled.is_empty());
        }
        // The batch settles and collect decides to re-offer the job.
        tracker
            .release_after_settle(&["job.x".to_string()], std::time::Duration::ZERO)
            .await;
        ledger
            .requeue_collected("job.x", RequeueReason::InfraAutoRetry)
            .await
            .unwrap();
        assert_eq!(ledger.tracker().resubmission_count("job.x").await, 1);
        {
            let mut wd = watchdog.lock().await;
            assert_eq!(wd.phase_of("job.x"), Some(JobPhase::Queued));
            // 1.5h after the requeue: had the Active clock survived the
            // requeue it would read 6.5h and fire ActiveStall; the reset
            // queued clock reads 1.5h of its 2h and stays quiet.
            assert!(wd.on_tick(&tick(5 * 3600 + 5400)).stalled.is_empty());
            // Another hour crosses the 2h queued threshold: the verdict is
            // a non-terminal QueuedRequeue (clock reset), not a stall that
            // burns the auto-retry.
            let outcome = wd.on_tick(&tick(5 * 3600 + 9000));
            assert_eq!(outcome.stalled.len(), 1, "{:?}", outcome.stalled);
            assert_eq!(outcome.stalled[0].kind, StallKind::QueuedRequeue);
        }
        // Resubmission flips the job back to Active.
        ledger.commit_batch(&["job.x".to_string()]).await;
        assert_eq!(
            watchdog.lock().await.phase_of("job.x"),
            Some(JobPhase::Active)
        );
    }

    /// The stall auto-retry transition: reservation released, resubmission
    /// counted, phase Queued.
    #[tokio::test]
    async fn stall_requeue_releases_and_observes_queued() {
        let (_dir, ledger, tracker, watchdog) = ledger();
        ledger.commit_batch(&["stuck.x".to_string()]).await;
        ledger.requeue_stalled("stuck.x").await.unwrap();
        assert!(!tracker.in_flight.lock().await.contains("stuck.x"));
        assert_eq!(ledger.tracker().resubmission_count("stuck.x").await, 1);
        assert_eq!(
            watchdog.lock().await.phase_of("stuck.x"),
            Some(JobPhase::Queued)
        );
    }

    /// The (requeue reason × counter consumer) lattice over the WHOLE
    /// vocabulary: every reason consumes retry BUDGET (decide()'s
    /// conservative-budget contract — re-offers can never multiply across
    /// reasons), while only cluster-attempt reasons enter the MEASUREMENT
    /// projection, so the flakiness of a one-requeue-then-success history
    /// follows the documented per-reason table. The expected rows derive
    /// from the carve-outs' own contracts: an engine-cancelled batch is
    /// "the engine's own act, not evidence about the job"
    /// (collect::RequeueBudget::engine_cancelled_carveout) and an
    /// engine-side submission failure never reached the cluster; every
    /// other reason describes an attempt that ran — or was denied a fair
    /// run — on the cluster. A new reason fails the row check until both
    /// consumers' semantics are decided.
    #[tokio::test]
    async fn budget_counts_every_reason_but_measurement_only_cluster_attempts() {
        let expected: &[(RequeueReason, bool)] = &[
            (RequeueReason::EngineCancelled, false),
            (RequeueReason::EngineSubmissionFailure, false),
            (RequeueReason::NoInbandResult, true),
            (RequeueReason::InfraAutoRetry, true),
            // The probe carve-out: evidence about the outage, not the job.
            // Normally never journaled (the decision consumer routes
            // probe-exempt re-offers around the journal), but a journaled
            // line must read back as not-a-cluster-attempt.
            (RequeueReason::InfraProbe, false),
            (RequeueReason::FailfastBatchMate, true),
            (RequeueReason::DependencyFailedNoTrigger, true),
            (RequeueReason::ActiveStall, true),
        ];
        assert_eq!(
            expected.len(),
            RequeueReason::ALL.len(),
            "every requeue reason needs a measurement-semantics row"
        );
        for reason in RequeueReason::ALL {
            let counted = expected
                .iter()
                .find(|(r, _)| *r == reason)
                .unwrap_or_else(|| panic!("no measurement row for {reason:?}"))
                .1;
            assert_eq!(reason.counts_as_cluster_attempt(), counted, "{reason:?}");
            // The journal string round-trips, so the projection reads back
            // exactly what the transitions wrote.
            assert_eq!(RequeueReason::from_wire(reason.as_str()), Some(reason));

            // One requeue of this reason, journaled through the ledger.
            let (dir, ledger, _tracker, _watchdog) = ledger();
            ledger.requeue_collected("job.x", reason).await.unwrap();
            // Budget consumer: every reason counts.
            assert_eq!(
                ledger.tracker().resubmission_count("job.x").await,
                1,
                "{reason:?} must consume budget"
            );
            // Measurement consumer: only cluster-attempt reasons count, so
            // a subsequent success is flaky (attempts > 1) exactly per the
            // table.
            let state = StateDir::new(dir.path()).unwrap();
            let measured = measured_attempt_requeues(&state)
                .unwrap()
                .get("job.x")
                .copied()
                .unwrap_or(0);
            assert_eq!(measured, u32::from(counted), "{reason:?}");
            assert_eq!(
                stamped_attempts(measured, true) > 1,
                counted,
                "{reason:?}: a success after this requeue must be flaky iff the reason is a \
                 cluster attempt"
            );
        }
    }

    /// A journal entry whose reason string is outside the vocabulary (a
    /// requeues.jsonl written by a different engine version) still parses
    /// and counts in the measurement: foreign entries keep the historical
    /// every-requeue-counts semantics instead of silently vanishing from
    /// the attempts accounting.
    #[tokio::test]
    async fn foreign_journal_reasons_parse_and_count_in_the_measurement() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        state
            .append_jsonl(
                StateFile::Requeues,
                &RequeueRecord {
                    job: "old.x".to_string(),
                    source: REQUEUE_SOURCE_COLLECT.to_string(),
                    why: "some-future-reason".to_string(),
                    at: "2026-01-01T00:00:00Z".to_string(),
                },
            )
            .unwrap();
        assert_eq!(RequeueReason::from_wire("some-future-reason"), None);
        let measured = measured_attempt_requeues(&state).unwrap();
        assert_eq!(measured.get("old.x"), Some(&1));
    }

    /// Resume rehydration is a pure fold of the requeue journal: a fresh
    /// ledger built over the same state dir reproduces the previous
    /// process's resubmission counters and the stall slice that gates the
    /// single stall auto-retry. Volatile state is deliberately NOT
    /// rehydrated (in-flight reservations, watchdog clocks, cool-downs),
    /// and a state dir recorded before the journal existed folds to empty
    /// counters — the pre-journal behavior, never an error.
    #[tokio::test]
    async fn from_journals_rebuilds_counters_and_stall_slice() {
        let (dir, ledger, _tracker, _watchdog) = ledger();
        ledger
            .requeue_collected("a.x", RequeueReason::InfraAutoRetry)
            .await
            .unwrap();
        ledger
            .requeue_collected("a.x", RequeueReason::EngineCancelled)
            .await
            .unwrap();
        ledger.commit_batch(&["b.x".to_string()]).await;
        ledger.requeue_stalled("b.x").await.unwrap();

        // "Pod restart": rebuild from the same state dir.
        let state = StateDir::new(dir.path()).unwrap();
        let watchdog = Arc::new(tokio::sync::Mutex::new(Watchdog::new(Knobs::default())));
        let (resumed, stall_retries) = JobLedger::from_journals(state, watchdog).unwrap();
        assert_eq!(resumed.tracker().resubmission_count("a.x").await, 2);
        assert_eq!(resumed.tracker().resubmission_count("b.x").await, 1);
        assert_eq!(
            stall_retries,
            HashMap::from([("b.x".to_string(), 1)]),
            "only the stall-source entries feed the stall auto-retry gate"
        );
        assert!(
            resumed.tracker().in_flight.lock().await.is_empty(),
            "in-flight reservations are process-local and never rehydrated"
        );

        // Pre-journal state dir (no requeues.jsonl): empty counters.
        let legacy_dir = tempfile::tempdir().unwrap();
        let legacy_state = StateDir::new(legacy_dir.path()).unwrap();
        let watchdog = Arc::new(tokio::sync::Mutex::new(Watchdog::new(Knobs::default())));
        let (legacy, legacy_stalls) = JobLedger::from_journals(legacy_state, watchdog).unwrap();
        assert_eq!(legacy.tracker().resubmission_count("a.x").await, 0);
        assert!(legacy_stalls.is_empty());
    }
}
