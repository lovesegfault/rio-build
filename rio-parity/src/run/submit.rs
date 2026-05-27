//! Submit stage: turns the attemptable, not-yet-terminal job set into
//! batched `nix build` submissions. Runs concurrently with collect; the
//! two stages communicate only through results.jsonl/batches.jsonl and
//! the in-memory [`SubmitTracker`]. Per-job requeue decisions belong to
//! collect — this loop simply re-offers any job whose latest record is
//! non-terminal and that is not currently in flight.

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
    /// Job → number of engine-initiated resubmissions so far.
    pub resubmissions: Mutex<HashMap<String, u32>>,
}

impl SubmitTracker {
    pub async fn resubmission_count(&self, job: &str) -> u32 {
        *self.resubmissions.lock().await.get(job).unwrap_or(&0)
    }
}

/// Build the batch list for one submission wave: attemptable jobs whose
/// latest record is non-terminal and that are not currently in flight.
/// Jobs already resubmitted at least `failfast_singleton_after` times are
/// emitted one-per-batch so a repeatedly fail-fast-cancelled batch member
/// can no longer mask its batch-mates' progress. Returns the assembled
/// batches plus the number of pending (offerable) jobs.
pub fn pending_wave(
    attemptable: &[PendingJob],
    terminal: &HashSet<String>,
    in_flight: &HashSet<String>,
    resubmissions: &HashMap<String, u32>,
    knobs: &Knobs,
) -> (Vec<Batch>, usize) {
    let mut normal = Vec::new();
    let mut singletons = Vec::new();
    for job in attemptable {
        if terminal.contains(&job.job) || in_flight.contains(&job.job) {
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

/// One submission worker: submits the batch, appends the [`BatchRecord`],
/// and releases the in-flight reservation. Returns the recorded batch.
///
/// A submitter `Err` (ssh/spawn/import failure) is evidence, not a fatal
/// error: it is recorded on the batch record with no build id and the jobs
/// are re-offered on a later wave; only state-dir I/O failures propagate.
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
        reasons: BTreeMap::new(),
        stderr_tail: None,
        engine_cancelled: false,
    };
    match outcome {
        Ok(o) => {
            record.build_id = o.build_id;
            record.exit_code = o.exit_code;
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
    state.append_jsonl(StateFile::Batches, &record)?;
    let mut in_flight = tracker.in_flight.lock().await;
    for job in &batch.jobs {
        in_flight.remove(job);
    }
    Ok(record)
}

/// The submit loop. Exits when every attemptable job is terminal (per
/// `terminal_jobs`) or `deadline_reached` reports true. While
/// `pause.paused()` is true no new batch is started; running children are
/// never killed by a pause — pausing only stops new submissions.
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
    let timeout = Duration::from_secs((knobs.batch_timeout_hours * 3600.0) as u64);
    let mut join_set: tokio::task::JoinSet<Result<BatchRecord>> = tokio::task::JoinSet::new();

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
        let (batches, pending) = pending_wave(
            &attemptable,
            &terminal,
            &in_flight_snapshot,
            &resubs,
            &knobs,
        );
        if pending == 0 && in_flight_snapshot.is_empty() {
            tracing::info!("submit loop drained: every attemptable job has a terminal record");
            break;
        }
        if pause.paused() {
            tokio::time::sleep(PAUSE_POLL).await;
            continue;
        }
        let Some(batch) = batches.into_iter().next() else {
            // Everything pending is in flight; wait for workers.
            tokio::time::sleep(IN_FLIGHT_POLL).await;
            continue;
        };
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore not closed");
        {
            let mut in_flight = tracker.in_flight.lock().await;
            for job in &batch.jobs {
                in_flight.insert(job.clone());
            }
        }
        let batch_id = batch_seq.fetch_add(1, Ordering::SeqCst);
        tracing::debug!(
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
            )
            .await
        });
    }
    // Wait for in-flight batches to settle before returning.
    while let Some(res) = join_set.join_next().await {
        res.map_err(|e| anyhow::anyhow!("submit worker panicked: {e}"))??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::submitter::BatchOutcome;
    use crate::run::submitter::test_support::FakeSubmitter;

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
        submitter.outcomes.lock().unwrap().push(Ok(BatchOutcome {
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".into()),
            exit_code: Some(1),
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
        )
        .await
        .unwrap();
        assert_eq!(rec.batch_id, 7);
        assert_eq!(
            rec.build_id.as_deref(),
            Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a")
        );
        assert_eq!(rec.exit_code, Some(1));
        assert!(!tracker.in_flight.lock().await.contains("x.x86_64-linux"));
        let on_disk: Vec<BatchRecord> = state.load_jsonl(StateFile::Batches).unwrap();
        assert_eq!(on_disk.len(), 1);
        assert_eq!(on_disk[0].reasons.len(), 1);
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
        // Dual cap respected: no batch carries more than 2 jobs.
        assert!(records.iter().all(|r| r.jobs.len() <= 2));
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
