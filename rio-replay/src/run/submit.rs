//! Submit stage: turns the attemptable, not-yet-terminal job set into
//! batched gateway submissions. Runs concurrently with collect; the
//! two stages communicate only through results.jsonl/batches.jsonl and
//! the in-memory scheduling state behind the
//! [`JobLedger`] (the [`SubmitTracker`] plus the watchdog's phase view —
//! batch commitment goes through the ledger so the watchdog observes
//! every submission). Per-job requeue decisions belong to
//! collect — this loop simply re-offers any job whose latest record is
//! non-terminal, that is not currently in flight, and whose
//! post-settlement cool-down has expired (the cool-down gives the collect
//! pass a chance to classify a settled batch before its jobs can be
//! offered again).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Mutex;

use super::batch::{Batch, PendingJob, assemble_batches};
use super::ledger::JobLedger;
use super::model::{BATCH_KIND_SUBMIT, BatchIntent, BatchRecord, PauseState, now_rfc3339};
use super::spec::Knobs;
use super::state::{StateDir, StateFile};
use super::submitter::{BatchDeadline, Submitter};
use super::supply::exec::PreSubmitSupply;

/// How long the loop sleeps between re-checks while submission is paused
/// (manual pause or backpressure). Short enough that an operator unpause
/// is picked up promptly; the per-tick work while paused is a cheap
/// in-memory wave computation.
const PAUSE_POLL: Duration = Duration::from_secs(1);

/// How long the loop sleeps when every pending job is already in flight
/// and there is nothing new to start, before re-checking for settled
/// workers.
const IN_FLIGHT_POLL: Duration = Duration::from_secs(1);

/// Shared view of what is currently in flight and how often each job has
/// been resubmitted by the engine. Resubmissions are campaign-level
/// re-offers (auto-retry, stall requeue) — collect folds them into each
/// job record's attempt count; they are distinct from the scheduler's own
/// internal retry cycles.
#[derive(Debug, Default)]
pub struct SubmitTracker {
    /// Jobs reserved by a batch that has been started and has not settled,
    /// keyed to the OWNING batch id. Ownership is what makes settlement
    /// releases safe against the deliberate live-overlap the stall
    /// auto-retry creates (the stuck batch keeps running while its member
    /// is re-offered into a fresh batch): a settle can only release the
    /// reservations its own batch id still owns, so a superseded batch's
    /// late settle cannot strip its successor's reservation and re-open
    /// the job to a third concurrent submission.
    pub in_flight: Mutex<HashMap<String, u64>>,
    /// Job → number of engine-initiated resubmissions so far. Incremented
    /// by the collect pass when it re-queues a job from a settled batch
    /// (and by the stall watchdog when it requeues a stalled job); the
    /// submit loop only reads it.
    pub resubmissions: Mutex<HashMap<String, u32>>,
    /// Job → engine-cancel carve-out cycles already granted (the
    /// engine-cancel why-slice of the resubmissions —
    /// `RequeueReason::is_engine_cancel_cycle`, both the announced and
    /// the fully cancelled variants): the explicit bound on the
    /// cancel-and-requeue loop, rehydrated from the same journal fold.
    pub cancel_cycles: Mutex<HashMap<String, u32>>,
    /// Job → earliest instant it may be offered again after its batch
    /// settled (the post-settlement cool-down; see
    /// [`Self::release_after_settle`]).
    pub cooldown_until: Mutex<HashMap<String, tokio::time::Instant>>,
}

impl SubmitTracker {
    pub async fn resubmission_count(&self, job: &str) -> u32 {
        *self.resubmissions.lock().await.get(job).unwrap_or(&0)
    }

    /// Release a settled batch's in-flight reservations and start the
    /// post-settlement cool-down for each of its jobs — but only the
    /// reservations `batch_id` still OWNS. A member re-reserved by a newer
    /// batch (the stall auto-retry released this batch's reservation and
    /// re-offered the job while this batch kept running) is left alone:
    /// the stale settle has no authority over the successor's reservation,
    /// and stripping it would re-open the job to a third concurrent
    /// submission on the next wave.
    ///
    /// Re-offer damper: a job whose batch settled without a terminal record
    /// stays un-offerable for `cooldown` (the submit loop passes one collect
    /// poll interval) so the concurrent collect pass gets a chance to
    /// classify the settled batch — write a terminal record or count a
    /// resubmission — before the job can be offered again. Without it a
    /// fast-failing batch (e.g. an ssh error that settles in milliseconds)
    /// would be resubmitted every wave without ever incrementing
    /// `resubmissions`, so fail-fast singleton isolation could never engage.
    /// The cool-down only delays the next offer; it does not change what
    /// counts as a resubmission. The cool-down entry is written before the
    /// in-flight reservation is dropped so there is no instant in which the
    /// job is offerable between the two updates.
    pub async fn release_after_settle(&self, batch_id: u64, jobs: &[String], cooldown: Duration) {
        let until = tokio::time::Instant::now() + cooldown;
        let owned: Vec<&String> = {
            let in_flight = self.in_flight.lock().await;
            jobs.iter()
                .filter(|job| in_flight.get(*job) == Some(&batch_id))
                .collect()
        };
        {
            let mut cooling = self.cooldown_until.lock().await;
            for job in &owned {
                cooling.insert((*job).clone(), until);
            }
        }
        let mut in_flight = self.in_flight.lock().await;
        for job in &owned {
            // Re-checked under the lock: ownership cannot have moved to
            // this batch id in between (ids are unique per submission),
            // so a non-matching entry simply stays.
            if in_flight.get(*job) == Some(&batch_id) {
                in_flight.remove(*job);
            }
        }
    }

    /// Jobs whose post-settlement cool-down has not yet expired. Expired
    /// entries are pruned as a side effect, so the map stays bounded by
    /// recently settled batches rather than growing with campaign size.
    pub async fn cooling_jobs(&self) -> HashSet<String> {
        let now = tokio::time::Instant::now();
        let mut cooling = self.cooldown_until.lock().await;
        cooling.retain(|_, until| *until > now);
        cooling.keys().cloned().collect()
    }
}

/// Build the batch list for one submission wave: attemptable jobs whose
/// latest record is non-terminal and that are not in `blocked` (currently
/// in flight, or still inside the post-settlement cool-down — the loop
/// passes the union of the two).
/// Jobs already resubmitted at least `failfast_singleton_after` times are
/// emitted one-per-batch so a repeatedly fail-fast-cancelled batch member
/// can no longer mask its batch-mates' progress. Returns the assembled
/// batches plus the number of pending (offerable) jobs.
pub fn pending_wave(
    attemptable: &[PendingJob],
    terminal: &HashSet<String>,
    blocked: &HashSet<String>,
    resubmissions: &HashMap<String, u32>,
    knobs: &Knobs,
) -> (Vec<Batch>, usize) {
    let mut normal = Vec::new();
    let mut singletons = Vec::new();
    for job in attemptable {
        if terminal.contains(&job.job) || blocked.contains(&job.job) {
            continue;
        }
        if resubmissions.get(&job.job).copied().unwrap_or(0) >= knobs.failfast_singleton_after {
            singletons.push(job.clone());
        } else {
            normal.push(job.clone());
        }
    }
    let pending = normal.len() + singletons.len();
    let mut batches = assemble_batches(&normal, knobs.batch_max_jobs, knobs.batch_max_nodes);
    for s in &singletons {
        batches.extend(assemble_batches(std::slice::from_ref(s), 1, usize::MAX));
    }
    (batches, pending)
}

/// One submission worker: submits the batch, releases the in-flight
/// reservation (starting the `cooldown` re-offer damper), and appends the
/// [`BatchRecord`]. Returns the recorded batch.
///
/// `interruption_drvs` names the root drvs for which the timed dispatcher
/// armed a recorded interruption; it is written verbatim onto the batch
/// record (timeless callers pass an empty vector). This chokepoint also
/// records WHICH deadline a cancellation came from: `engine_cancelled`
/// under a [`BatchDeadline::DisconnectReplay`] deadline is the recorded
/// interruption reproduced (`disconnect_deadline_fired`), while the same
/// cancellation under a [`BatchDeadline::Build`] deadline is the engine's
/// own budget cut — classification reads the bit, never re-derives it.
///
/// `topup_delivered` records whether this batch's pre-submission supply
/// top-up proved a COMPLETE delivery; the caller derives it from the
/// top-up's returned per-path outcome (`TopupOutcome::proves_delivery`,
/// fail-closed: false when no hook is wired, the call failed, or any owed
/// path went undelivered). It is written verbatim onto the record — the
/// inline-resume gate reads the bit as the batch's delivery proof, so
/// neither bare batch membership nor a top-up's bare Ok ever stands in
/// for delivery.
///
/// A submitter `Err` (ssh/spawn/import failure) is evidence, not a fatal
/// error: it is recorded on the batch record with no build id and the jobs
/// are re-offered on a later wave; only state-dir I/O failures propagate.
/// The reservations are released before the append so an append failure
/// can never leak in-flight markers.
///
/// `intent` is the writer's declared intent ([`BatchIntent`]), recorded
/// verbatim on the batch record — collect reads it to apply
/// writer-specific policy (probe budget exemption).
#[allow(clippy::too_many_arguments)]
pub async fn submit_one_batch(
    state: &StateDir,
    submitter: &dyn Submitter,
    tracker: &SubmitTracker,
    store_url: &str,
    kind: &str,
    batch_id: u64,
    batch: Batch,
    deadline: BatchDeadline,
    cooldown: Duration,
    interruption_drvs: Vec<String>,
    intent: BatchIntent,
) -> Result<BatchRecord> {
    let started_at = now_rfc3339();
    let outcome = submitter.submit_batch(store_url, &batch, deadline).await;
    let mut record = BatchRecord {
        batch_id,
        kind: kind.to_string(),
        jobs: batch.jobs.clone(),
        root_drvs: batch.root_drvs.clone(),
        est_nodes: batch.est_nodes,
        build_id: None,
        started_at,
        finished_at: Some(now_rfc3339()),
        results: Vec::new(),
        reasons: BTreeMap::new(),
        lost_terminals: BTreeSet::new(),
        stderr_tail: None,
        engine_cancelled: false,
        disconnect_deadline_fired: false,
        interruption_drvs,
        import_skipped_drvs: Vec::new(),
        import_skipped_by_root: BTreeMap::new(),
        probe: intent.probe,
        confirmation_attempt: intent.confirmation_attempt,
        topup_delivered: intent.topup_delivered,
    };
    match outcome {
        Ok(o) => {
            record.build_id = o.build_id;
            record.results = o.results;
            record.reasons = o.reasons;
            record.lost_terminals = o.lost_terminals;
            record.stderr_tail = Some(o.stderr_tail);
            record.engine_cancelled = o.engine_cancelled;
            record.import_skipped_drvs = o.import_skipped_drvs;
            record.import_skipped_by_root = o.import_skipped_by_root;
            // The submitter has exactly one cancellation source: the
            // deadline it was handed. Which logical deadline that was is
            // this call's knowledge, so the cause bit is derived here, at
            // the single point both halves are in scope.
            record.disconnect_deadline_fired =
                o.engine_cancelled && deadline.is_disconnect_replay();
            tracing::info!(
                batch_id,
                kind,
                jobs = record.jobs.len(),
                build_id = record.build_id.as_deref().unwrap_or(""),
                results = record.results.len(),
                engine_cancelled = record.engine_cancelled,
                disconnect_deadline_fired = record.disconnect_deadline_fired,
                reasons = record.reasons.len(),
                "batch settled"
            );
        }
        Err(e) => {
            // Submission infrastructure error (ssh failed, import failed, …):
            // recorded with no build_id; collect treats the jobs as
            // not-attempted and they are re-offered next wave.
            tracing::warn!(batch_id, kind, error = %format!("{e:#}"), "batch submission failed");
            record.stderr_tail = Some(format!("engine submission error: {e:#}"));
        }
    }
    tracker
        .release_after_settle(batch_id, &batch.jobs, cooldown)
        .await;
    state.append_jsonl(StateFile::Batches, &record)?;
    Ok(record)
}

/// The submit loop. Exits when every attemptable job is terminal (per
/// `terminal_jobs`) or `deadline_reached` reports true. While
/// `pause.paused()` is true no new batch is started; running children are
/// never killed by a pause — pausing only stops new submissions. Either
/// exit still drains the batches already in flight before returning, which
/// can take up to the batch timeout.
///
/// Canary-probe exception: a paused loop that finds a granted probe token
/// ([`PauseState::take_probe`]) releases exactly ONE one-job batch flagged
/// [`BatchIntent::probe`] — the poller's bounded way of refreshing the
/// infra-rate evidence the pause itself froze. The probe is never a wave:
/// one token, one singleton batch, reported back through
/// [`PauseState::set_probe_batch`] so the poller can score the cycle.
///
/// When `supply_topup` is present (the campaign's
/// [`LadderTopup`](super::supply::exec::LadderTopup) under inline delivery,
/// or as the prewarm-miss fallback), each batch gets a pre-submission supply
/// top-up over its root drvs — mirroring the timed dispatcher's per-request
/// call. A top-up failure degrades to a warning and the batch is submitted
/// regardless: a genuinely undelivered input then surfaces as that unit's
/// build failure, never as a silently skipped batch.
#[allow(clippy::too_many_arguments)]
pub async fn run_submit_loop(
    state: Arc<StateDir>,
    submitter: Arc<dyn Submitter>,
    ledger: Arc<JobLedger>,
    pause: Arc<PauseState>,
    attemptable: Vec<PendingJob>,
    terminal_jobs: impl Fn() -> HashSet<String> + Send + Sync + 'static,
    deadline_reached: impl Fn() -> bool + Send + Sync + 'static,
    store_url: String,
    knobs: Knobs,
    batch_seq: Arc<AtomicU64>,
    supply_topup: Option<Arc<dyn PreSubmitSupply>>,
) -> Result<()> {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(knobs.submit_concurrency.max(1)));
    // Floor the child deadline at one second: a zero/NaN batch_timeout_hours
    // is rejected by spec validation, but Knobs built outside a loaded spec
    // must still never become a 0-second deadline that kills every child the
    // moment it spawns.
    let timeout = Duration::from_secs((knobs.batch_timeout_hours * 3600.0).max(1.0) as u64);
    // Post-settlement cool-down (re-offer damper): one collect poll
    // interval, so the concurrent collect pass can classify a settled batch
    // before its non-terminal jobs become offerable again (see
    // [`SubmitTracker::release_after_settle`]).
    let cooldown = Duration::from_secs(knobs.collect_poll_secs.max(1));
    let mut join_set: tokio::task::JoinSet<Result<BatchRecord>> = tokio::task::JoinSet::new();
    tracing::info!(
        attemptable = attemptable.len(),
        submit_concurrency = knobs.submit_concurrency,
        batch_max_jobs = knobs.batch_max_jobs,
        batch_max_nodes = knobs.batch_max_nodes,
        "submit loop starting"
    );
    let mut was_paused = false;

    loop {
        // Drain finished workers (propagate hard errors, e.g. state-dir I/O).
        while let Some(res) = join_set.try_join_next() {
            res.map_err(|e| anyhow::anyhow!("submit worker panicked: {e}"))??;
        }
        if deadline_reached() {
            tracing::info!("deadline reached; not starting new batches");
            break;
        }
        let terminal = terminal_jobs();
        let (in_flight_snapshot, resubs) = {
            (
                ledger
                    .tracker()
                    .in_flight
                    .lock()
                    .await
                    .keys()
                    .cloned()
                    .collect::<HashSet<String>>(),
                ledger.tracker().resubmissions.lock().await.clone(),
            )
        };
        // Jobs still inside the post-settlement cool-down are withheld from
        // this wave; ones that already reached a terminal record are dropped
        // from the cooling view so they cannot keep the loop alive.
        let cooling: HashSet<String> = ledger
            .tracker()
            .cooling_jobs()
            .await
            .into_iter()
            .filter(|job| !terminal.contains(job))
            .collect();
        let blocked: HashSet<String> = in_flight_snapshot.union(&cooling).cloned().collect();
        let (batches, pending) = pending_wave(&attemptable, &terminal, &blocked, &resubs, &knobs);
        if pending == 0 && in_flight_snapshot.is_empty() && cooling.is_empty() {
            tracing::info!("submit loop drained: every attemptable job has a terminal record");
            break;
        }
        let paused = pause.paused();
        if paused != was_paused {
            // Log pause/resume once per transition, not per poll tick.
            if paused {
                tracing::info!(
                    manual = pause.manual(),
                    backpressure = pause.backpressure(),
                    "submission paused; running batches continue, no new batches start"
                );
            } else {
                tracing::info!("submission resumed");
            }
            was_paused = paused;
        }
        let (batch, intent) = if paused {
            // A paused loop submits nothing — except the single canary
            // probe the poller granted a token for. The probe is one job,
            // assembled fresh from the offerable set (never the full
            // wave), so a frozen infra-rate window is refreshed at the
            // cost of exactly one batch per probe cycle. A redeemed token
            // that finds no offerable job aborts the cycle so the poller
            // can grant again later.
            let probe = pause.take_probe().then(|| {
                attemptable
                    .iter()
                    .find(|job| !terminal.contains(&job.job) && !blocked.contains(&job.job))
                    .and_then(|job| {
                        assemble_batches(std::slice::from_ref(job), 1, usize::MAX)
                            .into_iter()
                            .next()
                    })
            });
            match probe {
                Some(Some(batch)) => (batch, BatchIntent::probe()),
                Some(None) => {
                    pause.abort_probe();
                    tokio::time::sleep(PAUSE_POLL).await;
                    continue;
                }
                None => {
                    tokio::time::sleep(PAUSE_POLL).await;
                    continue;
                }
            }
        } else {
            match batches.into_iter().next() {
                Some(batch) => (batch, BatchIntent::default()),
                None => {
                    // Everything pending is in flight or cooling down; wait
                    // for workers to settle or cool-downs to expire.
                    tokio::time::sleep(IN_FLIGHT_POLL).await;
                    continue;
                }
            }
        };
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore not closed");
        // The permit wait can be long (up to a full batch runtime when the
        // concurrency cap is saturated): re-check the pause flag and the
        // deadline before committing this batch, so a batch selected before
        // the wait cannot start hours after the operator paused or the
        // deadline passed. Dropping the permit and continuing also refreshes
        // the wave snapshot. A probe batch exists BECAUSE the engine pause
        // is on, so it re-checks only the operator's manual pause (which
        // always wins); a dropped probe aborts its cycle so the poller can
        // grant a fresh one.
        let pause_blocks = if intent.probe {
            pause.manual()
        } else {
            pause.paused()
        };
        if pause_blocks || deadline_reached() {
            if intent.probe {
                pause.abort_probe();
            }
            drop(permit);
            continue;
        }
        // The terminal set is just as stale across that wait: a member whose
        // earlier batch settled and was classified terminal while this batch
        // sat behind the semaphore must not be submitted again — the
        // duplicate would waste a concurrency slot and its eventual records
        // could displace the real verdict under latest-record-per-job
        // semantics. The view never spuriously shrinks (terminal_view
        // returns the last good snapshot under contention), so the re-check
        // can only drop batches that genuinely contain finished work; the
        // next wave re-packs the survivors.
        let terminal_now = terminal_jobs();
        if batch.jobs.iter().any(|job| terminal_now.contains(job)) {
            if intent.probe {
                pause.abort_probe();
            }
            drop(permit);
            continue;
        }
        let batch_id = batch_seq.fetch_add(1, Ordering::SeqCst);
        ledger.commit_batch(batch_id, &batch.jobs).await;
        if intent.probe {
            // Report the released probe so the poller can score the cycle
            // once collect classifies the batch.
            pause.set_probe_batch(batch_id, batch.jobs.clone());
        }
        tracing::info!(
            batch_id,
            jobs = batch.jobs.len(),
            est_nodes = batch.est_nodes,
            pending,
            probe = intent.probe,
            "starting batch"
        );
        let state = state.clone();
        let submitter = submitter.clone();
        let tracker = ledger.tracker_arc();
        let store_url = store_url.clone();
        let supply_topup = supply_topup.clone();
        join_set.spawn(async move {
            let _permit = permit;
            // Pre-submission gap top-up (inline delivery / prewarm-miss
            // fallback) over this batch's roots, exactly like the timed
            // dispatcher's per-request call: a failure degrades to a
            // warning so a supply hiccup can never wedge the batch — a
            // truly undelivered input surfaces as the unit's build failure
            // instead. The batch's delivery proof (the inline-resume
            // gate's evidence) is the returned per-path outcome over this
            // batch's own plan, collapsed FAIL-CLOSED: only a complete
            // delivery proves — a partial one (paths breaker-skipped,
            // refused, claim-held, or unsourceable) and an Err alike
            // submit the batch but prove nothing, because Ok-ness is not
            // delivery (a breaker-tripped top-up journals every remaining
            // path skipped and still returns Ok).
            let mut topup_delivered = false;
            if let Some(topup) = &supply_topup {
                match topup.topup(&batch.root_drvs).await {
                    Ok(outcome) => {
                        topup_delivered = outcome.proves_delivery();
                        if !topup_delivered {
                            tracing::warn!(
                                batch_id,
                                planned = outcome.planned,
                                delivered = outcome.delivered,
                                undelivered = outcome.undelivered,
                                "pre-submission supply top-up left paths undelivered; \
                                 submitting the batch without delivery proof"
                            );
                        }
                    }
                    Err(e) => tracing::warn!(
                        batch_id,
                        error = %format!("{e:#}"),
                        "pre-submission supply top-up failed; submitting the batch anyway"
                    ),
                }
            }
            // The batch deadline is anchored when the submission starts
            // (after the top-up): an absolute instant, so the budget covers
            // the whole submission and cannot stretch across the
            // submitter's import phase.
            submit_one_batch(
                &state,
                submitter.as_ref(),
                &tracker,
                &store_url,
                BATCH_KIND_SUBMIT,
                batch_id,
                batch,
                BatchDeadline::Build(tokio::time::Instant::now() + timeout),
                cooldown,
                Vec::new(),
                BatchIntent {
                    topup_delivered,
                    ..intent
                },
            )
            .await
        });
    }
    // Wait for in-flight batches to settle before returning (after a
    // deadline exit this can take up to the batch timeout).
    while let Some(res) = join_set.join_next().await {
        res.map_err(|e| anyhow::anyhow!("submit worker panicked: {e}"))??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::model::{PathOutcome, build_status_name};
    use crate::run::submitter::BatchOutcome;
    use crate::run::submitter::test_support::FakeSubmitter;
    use crate::run::supply::exec::TopupOutcome;
    use rio_nix::protocol::build::BuildStatus;

    /// Wrap a tracker in a fresh ledger (default-knob watchdog) over the
    /// test's state dir for the submit-loop tests; the tests keep their
    /// tracker handle for direct state assertions.
    fn test_ledger(state: &StateDir, tracker: Arc<SubmitTracker>) -> Arc<JobLedger> {
        Arc::new(JobLedger::new(
            state.clone(),
            tracker,
            Arc::new(Mutex::new(crate::run::watchdog::Watchdog::new(
                Knobs::default(),
            ))),
        ))
    }

    fn pj(name: &str, deps: usize) -> PendingJob {
        PendingJob {
            job: name.to_string(),
            drv_path: format!("/nix/store/{:0>32}-{name}.drv", deps),
            dep_drvs: (0..deps)
                .map(|i| format!("/nix/store/{i:0>32}-d{i}.drv"))
                .collect(),
        }
    }

    #[test]
    fn pending_wave_skips_terminal_inflight_and_singles_out_failfast_victims() {
        let attemptable = vec![pj("a", 1), pj("b", 1), pj("c", 1), pj("d", 1)];
        let terminal: HashSet<String> = ["a".to_string()].into();
        let in_flight: HashSet<String> = ["b".to_string()].into();
        let mut resubs = HashMap::new();
        resubs.insert("d".to_string(), 3);
        let knobs = Knobs::default();
        let (batches, pending) = pending_wave(&attemptable, &terminal, &in_flight, &resubs, &knobs);
        assert_eq!(pending, 2); // c (normal) + d (singleton)
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].jobs, vec!["c"]);
        assert_eq!(batches[1].jobs, vec!["d"]);
    }

    #[tokio::test]
    async fn submit_one_batch_records_outcome_and_releases_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let tracker = SubmitTracker::default();
        let submitter = FakeSubmitter::default();
        let scripted_result = PathOutcome {
            drv_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv".to_string(),
            status: build_status_name(BuildStatus::PermanentFailure).into(),
            error_msg: "builder failed with exit code 2".into(),
            start_time: 100,
            stop_time: 200,
        };
        submitter.outcomes.lock().unwrap().push(Ok(BatchOutcome {
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".into()),
            results: vec![scripted_result.clone()],
            reasons: BTreeMap::from([(
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv".to_string(),
                "failed on every eligible worker".to_string(),
            )]),
            lost_terminals: BTreeSet::from([
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv".to_string()
            ]),
            stderr_tail: "tail".into(),
            engine_cancelled: false,
            import_skipped_drvs: Vec::new(),
            import_skipped_by_root: BTreeMap::new(),
        }));
        let batch = Batch {
            jobs: vec!["x.x86_64-linux".into()],
            root_drvs: vec!["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv".into()],
            est_nodes: 1,
        };
        tracker
            .in_flight
            .lock()
            .await
            .insert("x.x86_64-linux".into(), 7);
        let rec = submit_one_batch(
            &state,
            &submitter,
            &tracker,
            "ssh-ng://x",
            BATCH_KIND_SUBMIT,
            7,
            batch,
            BatchDeadline::Build(tokio::time::Instant::now() + Duration::from_secs(60)),
            Duration::from_secs(60),
            Vec::new(),
            BatchIntent::default(),
        )
        .await
        .unwrap();
        assert_eq!(rec.batch_id, 7);
        assert_eq!(
            rec.build_id.as_deref(),
            Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a")
        );
        // The submitter's in-band per-root results ride the batch record.
        assert_eq!(rec.results, vec![scripted_result.clone()]);
        assert!(
            !tracker
                .in_flight
                .lock()
                .await
                .contains_key("x.x86_64-linux")
        );
        // The settled job enters the post-settlement cool-down.
        assert!(tracker.cooling_jobs().await.contains("x.x86_64-linux"));
        let on_disk: Vec<BatchRecord> = state.load_jsonl(StateFile::Batches).unwrap();
        assert_eq!(on_disk.len(), 1);
        assert_eq!(on_disk[0].reasons.len(), 1);
        assert_eq!(on_disk[0].results, vec![scripted_result]);
        // The captured lost-terminal markers ride the record like the
        // reasons do — collect's evidence-loss disambiguation reads them
        // from here, including across resume.
        assert_eq!(
            on_disk[0].lost_terminals,
            BTreeSet::from(["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv".to_string()])
        );
    }

    /// A submitter `Err` (engine-side submission failure) is evidence, not a
    /// loop-fatal error: the batch record carries it, no build id is set, the
    /// in-flight reservation is released, and the batch deadline the loop
    /// chose was passed through to the submitter.
    #[tokio::test]
    async fn submit_one_batch_records_engine_submission_errors() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let tracker = SubmitTracker::default();
        let submitter = FakeSubmitter::default();
        submitter
            .outcomes
            .lock()
            .unwrap()
            .push(Err(anyhow::anyhow!("ssh handshake failed")));
        let batch = Batch {
            jobs: vec!["x.x86_64-linux".into()],
            root_drvs: vec!["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv".into()],
            est_nodes: 1,
        };
        tracker
            .in_flight
            .lock()
            .await
            .insert("x.x86_64-linux".into(), 1);
        let deadline = BatchDeadline::Build(tokio::time::Instant::now() + Duration::from_secs(60));
        let rec = submit_one_batch(
            &state,
            &submitter,
            &tracker,
            "ssh-ng://x",
            BATCH_KIND_SUBMIT,
            1,
            batch,
            deadline,
            Duration::from_secs(60),
            Vec::new(),
            BatchIntent::default(),
        )
        .await
        .unwrap();
        assert_eq!(rec.build_id, None);
        assert!(rec.results.is_empty());
        assert!(
            rec.stderr_tail
                .as_deref()
                .unwrap_or_default()
                .contains("ssh handshake failed"),
            "{rec:?}"
        );
        assert!(
            !tracker
                .in_flight
                .lock()
                .await
                .contains_key("x.x86_64-linux")
        );
        let on_disk: Vec<BatchRecord> = state.load_jsonl(StateFile::Batches).unwrap();
        assert_eq!(on_disk.len(), 1);
        assert_eq!(submitter.submitted.lock().unwrap()[0].2, deadline);
    }

    /// The deadline-cause bit is derived at this chokepoint, from the typed
    /// deadline the caller handed in: a cancellation under a
    /// disconnect-replay deadline records `disconnect_deadline_fired`, the
    /// SAME scripted cancellation under a build deadline does not, and an
    /// uncancelled outcome never does — no submitter implementation can set
    /// or forget the bit on its own.
    #[tokio::test]
    async fn submit_one_batch_records_which_deadline_fired() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let tracker = SubmitTracker::default();
        let submitter = FakeSubmitter::default();
        let batch = || Batch {
            jobs: vec!["x.x86_64-linux".into()],
            root_drvs: vec!["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv".into()],
            est_nodes: 1,
        };
        let in_one_min = || tokio::time::Instant::now() + Duration::from_secs(60);
        let cancelled = || {
            Ok(BatchOutcome {
                engine_cancelled: true,
                ..BatchOutcome::default()
            })
        };

        // Scripts pop from the back: build-cut, then settled, then replay.
        submitter.outcomes.lock().unwrap().push(cancelled());
        submitter
            .outcomes
            .lock()
            .unwrap()
            .push(Ok(BatchOutcome::default()));
        submitter.outcomes.lock().unwrap().push(cancelled());

        let replay = submit_one_batch(
            &state,
            &submitter,
            &tracker,
            "ssh-ng://x",
            BATCH_KIND_SUBMIT,
            1,
            batch(),
            BatchDeadline::DisconnectReplay(in_one_min()),
            Duration::from_secs(60),
            Vec::new(),
            BatchIntent::default(),
        )
        .await
        .unwrap();
        assert!(replay.engine_cancelled);
        assert!(replay.disconnect_deadline_fired);

        let settled = submit_one_batch(
            &state,
            &submitter,
            &tracker,
            "ssh-ng://x",
            BATCH_KIND_SUBMIT,
            2,
            batch(),
            BatchDeadline::DisconnectReplay(in_one_min()),
            Duration::from_secs(60),
            Vec::new(),
            BatchIntent::default(),
        )
        .await
        .unwrap();
        assert!(!settled.engine_cancelled);
        assert!(!settled.disconnect_deadline_fired);

        let build_cut = submit_one_batch(
            &state,
            &submitter,
            &tracker,
            "ssh-ng://x",
            BATCH_KIND_SUBMIT,
            3,
            batch(),
            BatchDeadline::Build(in_one_min()),
            Duration::from_secs(60),
            Vec::new(),
            BatchIntent::default(),
        )
        .await
        .unwrap();
        assert!(build_cut.engine_cancelled);
        assert!(!build_cut.disconnect_deadline_fired);

        // The bits round-trip through batches.jsonl.
        let on_disk: Vec<BatchRecord> = state.load_jsonl(StateFile::Batches).unwrap();
        let fired: Vec<bool> = on_disk
            .iter()
            .map(|r| r.disconnect_deadline_fired)
            .collect();
        assert_eq!(fired, vec![true, false, false]);
    }

    #[tokio::test]
    async fn submit_loop_drains_and_respects_terminal_set() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateDir::new(dir.path()).unwrap());
        let tracker = Arc::new(SubmitTracker::default());
        let pause = Arc::new(PauseState::default());
        let submitter = Arc::new(FakeSubmitter::default());
        // Every batch "succeeds" with a build id; collect is not running, so
        // mark jobs terminal as soon as a batch for them has been submitted —
        // emulate that by treating any job recorded in the FakeSubmitter as
        // terminal.
        for _ in 0..4 {
            submitter.outcomes.lock().unwrap().push(Ok(BatchOutcome {
                build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".into()),
                ..BatchOutcome::default()
            }));
        }
        let attemptable = vec![pj("a", 0), pj("b", 0), pj("c", 0)];
        let submitted_view = submitter.clone();
        let knobs = Knobs {
            batch_max_jobs: 2,
            submit_concurrency: 2,
            ..Knobs::default()
        };
        run_submit_loop(
            state.clone(),
            submitter.clone(),
            test_ledger(&state, tracker),
            pause,
            attemptable,
            move || {
                submitted_view
                    .submitted
                    .lock()
                    .unwrap()
                    .iter()
                    .flat_map(|(_, b, _)| b.jobs.clone())
                    .collect()
            },
            || false,
            "ssh-ng://test".into(),
            knobs,
            Arc::new(AtomicU64::new(1)),
            None,
        )
        .await
        .unwrap();
        let records: Vec<BatchRecord> = state.load_jsonl(StateFile::Batches).unwrap();
        let all_jobs: HashSet<String> = records.iter().flat_map(|r| r.jobs.clone()).collect();
        assert_eq!(
            all_jobs,
            ["a", "b", "c"].iter().map(|s| s.to_string()).collect()
        );
        // No duplicates: the total job count across recorded batches equals
        // the input count, so no job was submitted twice.
        let total_jobs: usize = records.iter().map(|r| r.jobs.len()).sum();
        assert_eq!(total_jobs, 3);
        // Dual cap respected: no batch carries more than 2 jobs.
        assert!(records.iter().all(|r| r.jobs.len() <= 2));
        // No top-up hook wired: no record may claim a delivered top-up.
        assert!(records.iter().all(|r| !r.topup_delivered));
    }

    /// Scripted pre-submission supply hook: records each call's root drvs
    /// together with how many submissions the fake submitter had already
    /// received, then fails — proving both the before-submission ordering
    /// and that a top-up failure never blocks the batch.
    struct RecordingTopup {
        submitter: Arc<FakeSubmitter>,
        /// `(roots, submissions already made)` per call, in call order.
        calls: std::sync::Mutex<Vec<(Vec<String>, usize)>>,
    }

    #[async_trait::async_trait]
    impl PreSubmitSupply for RecordingTopup {
        async fn topup(&self, roots: &[String]) -> anyhow::Result<TopupOutcome> {
            let submitted = self.submitter.submitted.lock().unwrap().len();
            self.calls.lock().unwrap().push((roots.to_vec(), submitted));
            anyhow::bail!("scripted top-up failure")
        }
    }

    /// The pre-submission top-up runs once per batch with that batch's root
    /// drvs, before the batch's own submission — the timeless leg of inline
    /// delivery and of the prewarm-miss fallback; a failing top-up degrades
    /// to a warning and the batch is submitted anyway.
    #[tokio::test]
    async fn submit_loop_runs_the_topup_before_each_batch_and_failures_never_block() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateDir::new(dir.path()).unwrap());
        let tracker = Arc::new(SubmitTracker::default());
        let pause = Arc::new(PauseState::default());
        let submitter = Arc::new(FakeSubmitter::default());
        for _ in 0..2 {
            submitter.outcomes.lock().unwrap().push(Ok(BatchOutcome {
                build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".into()),
                ..BatchOutcome::default()
            }));
        }
        let topup = Arc::new(RecordingTopup {
            submitter: submitter.clone(),
            calls: std::sync::Mutex::new(Vec::new()),
        });
        // Serial submission (concurrency 1) makes the top-up/submission
        // interleaving deterministic; two jobs per batch over three jobs
        // yields two batches.
        let knobs = Knobs {
            batch_max_jobs: 2,
            submit_concurrency: 1,
            ..Knobs::default()
        };
        let submitted_view = submitter.clone();
        let ledger = test_ledger(&state, tracker);
        run_submit_loop(
            state.clone(),
            submitter.clone(),
            ledger,
            pause,
            vec![pj("a", 0), pj("b", 0), pj("c", 0)],
            move || {
                submitted_view
                    .submitted
                    .lock()
                    .unwrap()
                    .iter()
                    .flat_map(|(_, b, _)| b.jobs.clone())
                    .collect()
            },
            || false,
            "ssh-ng://test".into(),
            knobs,
            Arc::new(AtomicU64::new(1)),
            Some(topup.clone() as Arc<dyn PreSubmitSupply>),
        )
        .await
        .unwrap();
        let calls = topup.calls.lock().unwrap().clone();
        let submitted = submitter.submitted.lock().unwrap();
        assert_eq!(calls.len(), 2, "one top-up call per batch");
        assert_eq!(submitted.len(), 2, "failing top-ups never block submission");
        // Each call carried exactly its batch's root drvs and happened
        // before that batch's own submission: the first call saw zero prior
        // submissions, the second exactly one.
        for (index, ((roots, prior), (_, batch, _))) in
            calls.iter().zip(submitted.iter()).enumerate()
        {
            assert_eq!(roots, &batch.root_drvs);
            assert_eq!(*prior, index);
        }
        // A failed top-up submits the batch but proves no delivery: the
        // records must not claim it.
        let records: Vec<BatchRecord> = state.load_jsonl(StateFile::Batches).unwrap();
        assert!(
            records.iter().all(|r| !r.topup_delivered),
            "failed top-ups must not be recorded as delivered: {records:?}"
        );
    }

    /// Scripted [`PreSubmitSupply`] whose outcomes pop from a list (front
    /// first); calls beyond the script report a vacuous complete delivery.
    struct ScriptedTopup {
        outcomes: std::sync::Mutex<std::collections::VecDeque<anyhow::Result<TopupOutcome>>>,
    }

    #[async_trait::async_trait]
    impl PreSubmitSupply for ScriptedTopup {
        async fn topup(&self, _roots: &[String]) -> anyhow::Result<TopupOutcome> {
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(TopupOutcome::default()))
        }
    }

    /// The batch record's `topup_delivered` bit is the FAIL-CLOSED
    /// collapse of the top-up's returned per-path outcome, derived at this
    /// chokepoint from the call the loop actually made.
    ///
    /// Quantification domain: every value the producer can return — the
    /// full [`TopupDelivery`] tri-state (complete / partial / nothing
    /// delivered, the latter the breaker-tripped all-skipped shape that
    /// returns Ok after delivering nothing) plus the `Err` arm — and, in
    /// the sibling test above, the no-hook case. Only a COMPLETE delivery
    /// records true; partial is NOT delivered (the gate consumes the bit
    /// one-sidedly, so a 5-of-6 top-up must prove nothing for the job
    /// that needed the sixth path), and the Ok-with-zero-delivered shape
    /// that previously minted a false proof records false. The fixture
    /// returns producer-typed [`TopupOutcome`] values, so the violating
    /// state (Ok with undelivered > 0) is expressible — the prior unit
    /// `Result` fixture could not express it and pinned the flawed
    /// Ok→true mapping instead of the requirement.
    #[tokio::test]
    async fn submit_loop_records_topup_delivery_proof_per_batch() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateDir::new(dir.path()).unwrap());
        let submitter = Arc::new(FakeSubmitter::default());
        for _ in 0..4 {
            submitter.outcomes.lock().unwrap().push(Ok(BatchOutcome {
                build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".into()),
                ..BatchOutcome::default()
            }));
        }
        // One single-job batch per scripted outcome; serial submission
        // (concurrency 1) keeps the order deterministic.
        let topup = Arc::new(ScriptedTopup {
            outcomes: std::sync::Mutex::new(
                [
                    Err(anyhow::anyhow!("scripted top-up failure")),
                    // Complete: everything owed was delivered.
                    Ok(TopupOutcome {
                        planned: 2,
                        delivered: 2,
                        undelivered: 0,
                    }),
                    // Partial: one path delivered, one breaker-skipped.
                    Ok(TopupOutcome {
                        planned: 2,
                        delivered: 1,
                        undelivered: 1,
                    }),
                    // Nothing delivered: the breaker-tripped all-skipped
                    // run — Ok, but zero delivery.
                    Ok(TopupOutcome {
                        planned: 2,
                        delivered: 0,
                        undelivered: 2,
                    }),
                ]
                .into(),
            ),
        });
        let knobs = Knobs {
            batch_max_jobs: 1,
            submit_concurrency: 1,
            ..Knobs::default()
        };
        let submitted_view = submitter.clone();
        run_submit_loop(
            state.clone(),
            submitter.clone(),
            test_ledger(&state, Arc::new(SubmitTracker::default())),
            Arc::new(PauseState::default()),
            vec![pj("a", 0), pj("b", 0), pj("c", 0), pj("d", 0)],
            move || {
                submitted_view
                    .submitted
                    .lock()
                    .unwrap()
                    .iter()
                    .flat_map(|(_, b, _)| b.jobs.clone())
                    .collect()
            },
            || false,
            "ssh-ng://test".into(),
            knobs,
            Arc::new(AtomicU64::new(1)),
            Some(topup as Arc<dyn PreSubmitSupply>),
        )
        .await
        .unwrap();
        let mut records: Vec<BatchRecord> = state.load_jsonl(StateFile::Batches).unwrap();
        records.sort_by_key(|record| record.batch_id);
        let delivered: Vec<bool> = records.iter().map(|r| r.topup_delivered).collect();
        assert_eq!(
            delivered,
            vec![false, true, false, false],
            "Err / complete / partial / nothing-delivered must collapse to \
             false / true / false / false: {records:?}"
        );
        // All four batches were still submitted: the proof bit gates the
        // resume refusal, never the submission itself.
        assert_eq!(records.len(), 4);
        assert!(records.iter().all(|r| r.build_id.is_some()), "{records:?}");
    }

    /// The permit wait is a staleness window for the terminal set, not just
    /// pause/deadline: a batch whose member reached a terminal record after
    /// wave selection must be dropped at the post-acquire re-check instead
    /// of being submitted as a duplicate (which would waste a concurrency
    /// slot and let its eventual records displace the real verdict under
    /// latest-record-per-job semantics). The terminal view here flips
    /// between selection (empty) and the post-acquire re-check (job
    /// terminal), exactly the interleaving a slow classification leaves.
    #[tokio::test]
    async fn submit_loop_drops_batches_whose_jobs_settled_during_the_permit_wait() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateDir::new(dir.path()).unwrap());
        let submitter = Arc::new(FakeSubmitter::default());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_view = calls.clone();
        run_submit_loop(
            state.clone(),
            submitter.clone(),
            test_ledger(&state, Arc::new(SubmitTracker::default())),
            Arc::new(PauseState::default()),
            vec![pj("a", 0)],
            move || {
                // First call: the wave snapshot — job not yet terminal, so
                // the batch is selected. Every later call (the post-acquire
                // re-check, the next wave) sees it terminal.
                if calls_view.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    HashSet::new()
                } else {
                    ["a".to_string()].into()
                }
            },
            || false,
            "ssh-ng://test".into(),
            Knobs::default(),
            Arc::new(AtomicU64::new(1)),
            None,
        )
        .await
        .unwrap();
        assert!(
            submitter.submitted.lock().unwrap().is_empty(),
            "the settled-while-waiting batch must be dropped, not submitted"
        );
        let records: Vec<BatchRecord> = state.load_jsonl(StateFile::Batches).unwrap();
        assert!(records.is_empty(), "{records:?}");
        assert!(
            calls.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "the loop re-checked the terminal set after the permit"
        );
    }

    /// A deadline that is already reached exits cleanly before starting any
    /// batch: no submissions, no batch records, and the loop returns Ok.
    #[tokio::test]
    async fn submit_loop_exits_cleanly_at_the_deadline_without_submitting() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateDir::new(dir.path()).unwrap());
        let submitter = Arc::new(FakeSubmitter::default());
        run_submit_loop(
            state.clone(),
            submitter.clone(),
            test_ledger(&state, Arc::new(SubmitTracker::default())),
            Arc::new(PauseState::default()),
            vec![pj("a", 0)],
            HashSet::new,
            || true,
            "ssh-ng://test".into(),
            Knobs::default(),
            Arc::new(AtomicU64::new(1)),
            None,
        )
        .await
        .unwrap();
        assert!(submitter.submitted.lock().unwrap().is_empty());
        let records: Vec<BatchRecord> = state.load_jsonl(StateFile::Batches).unwrap();
        assert!(records.is_empty());
    }

    /// [`SubmitTracker::release_after_settle`] swaps the in-flight
    /// reservation for a cool-down entry, and [`SubmitTracker::cooling_jobs`]
    /// reports the job only until the cool-down expires (pruning it after).
    #[tokio::test(start_paused = true)]
    async fn tracker_cooldown_blocks_then_expires() {
        let tracker = SubmitTracker::default();
        tracker.in_flight.lock().await.insert("a".into(), 3);
        tracker
            .release_after_settle(3, &["a".to_string()], Duration::from_secs(60))
            .await;
        assert!(!tracker.in_flight.lock().await.contains_key("a"));
        assert_eq!(tracker.cooling_jobs().await, ["a".to_string()].into());
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(tracker.cooling_jobs().await.is_empty());
        assert!(
            tracker.cooldown_until.lock().await.is_empty(),
            "expired entries are pruned"
        );
    }

    /// The post-settlement cool-down (re-offer damper): a job whose batch
    /// settled without a terminal record is not offered again until one
    /// collect poll interval has elapsed, so a fast-failing batch cannot be
    /// resubmitted every wave before the collect pass has had a chance to
    /// run. Virtual time (paused clock) makes the wait deterministic.
    #[tokio::test(start_paused = true)]
    async fn submit_loop_waits_out_the_cooldown_before_reoffering() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateDir::new(dir.path()).unwrap());
        let tracker = Arc::new(SubmitTracker::default());
        let pause = Arc::new(PauseState::default());
        let submitter = Arc::new(FakeSubmitter::default());
        // Two scripted engine-side failures: each settles in microseconds and
        // leaves the job non-terminal, so the loop wants to re-offer it.
        for _ in 0..2 {
            submitter
                .outcomes
                .lock()
                .unwrap()
                .push(Err(anyhow::anyhow!("ssh handshake failed")));
        }
        let knobs = Knobs {
            collect_poll_secs: 30,
            ..Knobs::default()
        };
        let started = tokio::time::Instant::now();
        let handle = tokio::spawn(run_submit_loop(
            state.clone(),
            submitter.clone(),
            test_ledger(&state, tracker),
            pause,
            vec![pj("a", 0)],
            HashSet::new,
            || false,
            "ssh-ng://test".into(),
            knobs,
            Arc::new(AtomicU64::new(1)),
            None,
        ));
        // Wait (in virtual time) until the same job has been submitted twice.
        for _ in 0..600 {
            if submitter.submitted.lock().unwrap().len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let elapsed = started.elapsed();
        assert_eq!(
            submitter.submitted.lock().unwrap().len(),
            2,
            "job re-offered after the cool-down"
        );
        assert!(
            elapsed >= Duration::from_secs(30),
            "second submission must wait out the 30s cool-down, got {elapsed:?}"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn submit_loop_does_not_start_batches_while_paused() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateDir::new(dir.path()).unwrap());
        let tracker = Arc::new(SubmitTracker::default());
        let pause = Arc::new(PauseState::default());
        pause.set_manual(true);
        let submitter = Arc::new(FakeSubmitter::default());
        let attemptable = vec![pj("a", 0)];
        let handle = tokio::spawn(run_submit_loop(
            state.clone(),
            submitter.clone(),
            test_ledger(&state, tracker),
            pause.clone(),
            attemptable,
            HashSet::new,
            || false,
            "ssh-ng://test".into(),
            Knobs::default(),
            Arc::new(AtomicU64::new(1)),
            None,
        ));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            submitter.submitted.lock().unwrap().is_empty(),
            "paused loop must not submit"
        );
        // Unpause and let the single batch run.
        pause.set_manual(false);
        submitter
            .outcomes
            .lock()
            .unwrap()
            .push(Ok(BatchOutcome::default()));
        // Once submitted, the loop still sees the job as non-terminal (the
        // terminal set above is constantly empty), so it would resubmit
        // forever; abort the task once the first submission is observed.
        for _ in 0..100 {
            if !submitter.submitted.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            !submitter.submitted.lock().unwrap().is_empty(),
            "unpaused loop submits"
        );
        handle.abort();
    }

    /// A backpressure-paused loop with a granted probe token releases
    /// exactly ONE single-job probe batch — never the wave — records the
    /// probe intent on the batch record, reports the released batch through
    /// the pause channel, and goes back to submitting nothing until the
    /// next grant. The operator's manual pause still vetoes probing.
    #[tokio::test]
    async fn paused_loop_releases_exactly_one_probe_batch_per_token() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateDir::new(dir.path()).unwrap());
        let pause = Arc::new(PauseState::default());
        pause.set_backpressure(true);
        let submitter = Arc::new(FakeSubmitter::default());
        submitter
            .outcomes
            .lock()
            .unwrap()
            .push(Err(anyhow::anyhow!("gateway unreachable")));
        // Three offerable jobs: a wave would batch them together.
        let attemptable = vec![pj("a", 0), pj("b", 0), pj("c", 0)];
        let handle = tokio::spawn(run_submit_loop(
            state.clone(),
            submitter.clone(),
            test_ledger(&state, Arc::new(SubmitTracker::default())),
            pause.clone(),
            attemptable,
            HashSet::new,
            || false,
            "ssh-ng://test".into(),
            Knobs::default(),
            Arc::new(AtomicU64::new(1)),
            None,
        ));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            submitter.submitted.lock().unwrap().is_empty(),
            "no token, no submission"
        );

        pause.grant_probe();
        for _ in 0..100 {
            if !submitter.submitted.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        {
            let submitted = submitter.submitted.lock().unwrap();
            assert_eq!(submitted.len(), 1, "one token releases one batch");
            assert_eq!(
                submitted[0].1.jobs.len(),
                1,
                "the probe is a single job, not the wave"
            );
        }
        // The released probe is reported for poller-side scoring, and the
        // batch record carries the probe intent.
        let (batch_id, probe_jobs) = pause.probe_batch().expect("released probe is reported");
        assert_eq!(probe_jobs.len(), 1);
        let on_disk: Vec<BatchRecord> = state.load_jsonl(StateFile::Batches).unwrap();
        assert_eq!(on_disk.len(), 1);
        assert!(on_disk[0].probe, "the batch record carries the probe bit");
        assert_eq!(on_disk[0].batch_id, batch_id);
        assert_eq!(on_disk[0].jobs, probe_jobs);

        // No further batches without a fresh grant.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            submitter.submitted.lock().unwrap().len(),
            1,
            "a redeemed token never releases a second batch"
        );

        // The operator's manual pause vetoes probing: a token granted under
        // a manual pause is aborted, not redeemed into a submission.
        pause.clear_probe();
        pause.set_manual(true);
        pause.grant_probe();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            submitter.submitted.lock().unwrap().len(),
            1,
            "the manual pause vetoes probe submissions"
        );
        handle.abort();
    }
}
