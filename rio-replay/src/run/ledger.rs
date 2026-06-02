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
//! auto-retry gate, `attempts` accounting); deriving them from anything
//! volatile would zero every consumed budget at each pod restart — the
//! exact spin the budget exists to prevent, reopened at the restart edge.
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

use super::model::{REQUEUE_SOURCE_COLLECT, REQUEUE_SOURCE_STALL, RequeueRecord, now_rfc3339};
use super::state::{StateDir, StateFile};
use super::submit::SubmitTracker;
use super::watchdog::{JobPhase, Watchdog};

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
    /// the collect decision's requeue reason, recorded on the journal line
    /// for archaeology.
    pub async fn requeue_collected(&self, job: &str, why: &str) -> Result<()> {
        self.journal_requeue(job, REQUEUE_SOURCE_COLLECT, why)?;
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

    /// The active-stall auto-retry: journal the requeue, release the
    /// in-flight reservation, count the resubmission, and observe the job
    /// `Queued` so its wait for the fresh batch is charged to the queued
    /// clock, not a stale `Active` one.
    pub async fn requeue_stalled(&self, job: &str) -> Result<()> {
        self.journal_requeue(job, REQUEUE_SOURCE_STALL, "active-stall")?;
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
            .requeue_collected("job.x", "infra-auto-retry")
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
            .requeue_collected("a.x", "infra-auto-retry")
            .await
            .unwrap();
        ledger
            .requeue_collected("a.x", "engine-cancelled")
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
