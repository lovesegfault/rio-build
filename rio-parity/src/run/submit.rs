//! Submit stage: turns the attemptable, not-yet-terminal job set into
//! batched `nix build` submissions. Runs concurrently with collect; the
//! two stages communicate only through results.jsonl/batches.jsonl and
//! the in-memory [`SubmitTracker`]. Per-job requeue decisions belong to
//! collect — this loop simply re-offers any job whose latest record is
//! non-terminal, that is not currently in flight, and whose
//! post-settlement cool-down has expired (the cool-down gives the collect
//! pass a chance to classify a settled batch before its jobs can be
//! offered again).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Mutex;

use super::batch::{Batch, PendingJob, assemble_batches};
use super::model::{BATCH_KIND_SUBMIT, BatchRecord, PauseState, now_rfc3339};
use super::spec::Knobs;
use super::state::{StateDir, StateFile};
use super::submitter::Submitter;

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
    /// Jobs reserved by a batch that has been started and has not settled.
    pub in_flight: Mutex<HashSet<String>>,
    /// Job → number of engine-initiated resubmissions so far. Incremented
    /// by the collect pass when it re-queues a job from a settled batch
    /// (and by the stall watchdog when it requeues a stalled job); the
    /// submit loop only reads it.
    pub resubmissions: Mutex<HashMap<String, u32>>,
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
    /// post-settlement cool-down for each of its jobs.
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
    pub async fn release_after_settle(&self, jobs: &[String], cooldown: Duration) {
        let until = tokio::time::Instant::now() + cooldown;
        {
            let mut cooling = self.cooldown_until.lock().await;
            for job in jobs {
                cooling.insert(job.clone(), until);
            }
        }
        let mut in_flight = self.in_flight.lock().await;
        for job in jobs {
            in_flight.remove(job);
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
/// A submitter `Err` (ssh/spawn/import failure) is evidence, not a fatal
/// error: it is recorded on the batch record with no build id and the jobs
/// are re-offered on a later wave; only state-dir I/O failures propagate.
/// The reservations are released before the append so an append failure
/// can never leak in-flight markers.
#[allow(clippy::too_many_arguments)]
pub async fn submit_one_batch(
    state: &StateDir,
    submitter: &dyn Submitter,
    tracker: &SubmitTracker,
    store_url: &str,
    kind: &str,
    batch_id: u64,
    batch: Batch,
    timeout: Duration,
    cooldown: Duration,
) -> Result<BatchRecord> {
    let started_at = now_rfc3339();
    let outcome = submitter.submit_batch(store_url, &batch, timeout).await;
    let mut record = BatchRecord {
        batch_id,
        kind: kind.to_string(),
        jobs: batch.jobs.clone(),
        root_drvs: batch.root_drvs.clone(),
        est_nodes: batch.est_nodes,
        build_id: None,
        started_at,
        finished_at: Some(now_rfc3339()),
        exit_code: None,
        results: Vec::new(),
        reasons: BTreeMap::new(),
        stderr_tail: None,
        engine_cancelled: false,
    };
    match outcome {
        Ok(o) => {
            record.build_id = o.build_id;
            record.exit_code = o.exit_code;
            record.results = o.results;
            record.reasons = o.reasons;
            record.stderr_tail = Some(o.stderr_tail);
            record.engine_cancelled = o.engine_cancelled;
            tracing::info!(
                batch_id,
                kind,
                jobs = record.jobs.len(),
                build_id = record.build_id.as_deref().unwrap_or(""),
                exit_code = record.exit_code,
                engine_cancelled = record.engine_cancelled,
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
    tracker.release_after_settle(&batch.jobs, cooldown).await;
    state.append_jsonl(StateFile::Batches, &record)?;
    Ok(record)
}

/// The submit loop. Exits when every attemptable job is terminal (per
/// `terminal_jobs`) or `deadline_reached` reports true. While
/// `pause.paused()` is true no new batch is started; running children are
/// never killed by a pause — pausing only stops new submissions. Either
/// exit still drains the batches already in flight before returning, which
/// can take up to the batch timeout.
#[allow(clippy::too_many_arguments)]
pub async fn run_submit_loop(
    state: Arc<StateDir>,
    submitter: Arc<dyn Submitter>,
    tracker: Arc<SubmitTracker>,
    pause: Arc<PauseState>,
    attemptable: Vec<PendingJob>,
    terminal_jobs: impl Fn() -> HashSet<String> + Send + Sync + 'static,
    deadline_reached: impl Fn() -> bool + Send + Sync + 'static,
    store_url: String,
    knobs: Knobs,
    batch_seq: Arc<AtomicU64>,
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
                tracker.in_flight.lock().await.clone(),
                tracker.resubmissions.lock().await.clone(),
            )
        };
        // Jobs still inside the post-settlement cool-down are withheld from
        // this wave; ones that already reached a terminal record are dropped
        // from the cooling view so they cannot keep the loop alive.
        let cooling: HashSet<String> = tracker
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
        if paused {
            tokio::time::sleep(PAUSE_POLL).await;
            continue;
        }
        let Some(batch) = batches.into_iter().next() else {
            // Everything pending is in flight or cooling down; wait for
            // workers to settle or cool-downs to expire.
            tokio::time::sleep(IN_FLIGHT_POLL).await;
            continue;
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
        // the wave snapshot.
        if pause.paused() || deadline_reached() {
            drop(permit);
            continue;
        }
        {
            let mut in_flight = tracker.in_flight.lock().await;
            for job in &batch.jobs {
                in_flight.insert(job.clone());
            }
        }
        let batch_id = batch_seq.fetch_add(1, Ordering::SeqCst);
        tracing::info!(
            batch_id,
            jobs = batch.jobs.len(),
            est_nodes = batch.est_nodes,
            pending,
            "starting batch"
        );
        let state = state.clone();
        let submitter = submitter.clone();
        let tracker = tracker.clone();
        let store_url = store_url.clone();
        join_set.spawn(async move {
            let _permit = permit;
            submit_one_batch(
                &state,
                submitter.as_ref(),
                &tracker,
                &store_url,
                BATCH_KIND_SUBMIT,
                batch_id,
                batch,
                timeout,
                cooldown,
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
    use rio_nix::protocol::build::BuildStatus;

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
            exit_code: Some(1),
            results: vec![scripted_result.clone()],
            reasons: BTreeMap::from([(
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv".to_string(),
                "failed on every eligible worker".to_string(),
            )]),
            stderr_tail: "tail".into(),
            engine_cancelled: false,
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
            .insert("x.x86_64-linux".into());
        let rec = submit_one_batch(
            &state,
            &submitter,
            &tracker,
            "ssh-ng://x",
            BATCH_KIND_SUBMIT,
            7,
            batch,
            Duration::from_secs(60),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert_eq!(rec.batch_id, 7);
        assert_eq!(
            rec.build_id.as_deref(),
            Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a")
        );
        assert_eq!(rec.exit_code, Some(1));
        // The submitter's in-band per-root results ride the batch record.
        assert_eq!(rec.results, vec![scripted_result.clone()]);
        assert!(!tracker.in_flight.lock().await.contains("x.x86_64-linux"));
        // The settled job enters the post-settlement cool-down.
        assert!(tracker.cooling_jobs().await.contains("x.x86_64-linux"));
        let on_disk: Vec<BatchRecord> = state.load_jsonl(StateFile::Batches).unwrap();
        assert_eq!(on_disk.len(), 1);
        assert_eq!(on_disk[0].reasons.len(), 1);
        assert_eq!(on_disk[0].results, vec![scripted_result]);
    }

    /// A submitter `Err` (engine-side submission failure) is evidence, not a
    /// loop-fatal error: the batch record carries it, no build id is set, the
    /// in-flight reservation is released, and the batch timeout the loop
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
            .insert("x.x86_64-linux".into());
        let timeout = Duration::from_secs(60);
        let rec = submit_one_batch(
            &state,
            &submitter,
            &tracker,
            "ssh-ng://x",
            BATCH_KIND_SUBMIT,
            1,
            batch,
            timeout,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert_eq!(rec.build_id, None);
        assert_eq!(rec.exit_code, None);
        assert!(
            rec.stderr_tail
                .as_deref()
                .unwrap_or_default()
                .contains("ssh handshake failed"),
            "{rec:?}"
        );
        assert!(!tracker.in_flight.lock().await.contains("x.x86_64-linux"));
        let on_disk: Vec<BatchRecord> = state.load_jsonl(StateFile::Batches).unwrap();
        assert_eq!(on_disk.len(), 1);
        assert_eq!(submitter.submitted.lock().unwrap()[0].2, timeout);
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
                exit_code: Some(0),
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
            tracker,
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
            Arc::new(SubmitTracker::default()),
            Arc::new(PauseState::default()),
            vec![pj("a", 0)],
            HashSet::new,
            || true,
            "ssh-ng://test".into(),
            Knobs::default(),
            Arc::new(AtomicU64::new(1)),
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
        tracker.in_flight.lock().await.insert("a".into());
        tracker
            .release_after_settle(&["a".to_string()], Duration::from_secs(60))
            .await;
        assert!(!tracker.in_flight.lock().await.contains("a"));
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
            tracker,
            pause,
            vec![pj("a", 0)],
            HashSet::new,
            || false,
            "ssh-ng://test".into(),
            knobs,
            Arc::new(AtomicU64::new(1)),
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
            tracker,
            pause.clone(),
            attemptable,
            HashSet::new,
            || false,
            "ssh-ng://test".into(),
            Knobs::default(),
            Arc::new(AtomicU64::new(1)),
        ));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            submitter.submitted.lock().unwrap().is_empty(),
            "paused loop must not submit"
        );
        // Unpause and let the single batch run.
        pause.set_manual(false);
        submitter.outcomes.lock().unwrap().push(Ok(BatchOutcome {
            exit_code: Some(0),
            ..BatchOutcome::default()
        }));
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
}
