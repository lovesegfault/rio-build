//! Timed-replay scheduling: schedule construction and the timed dispatcher.
//!
//! [`build_schedule`] turns recorded client requests into a paced
//! [`ScheduledRequest`] list — offset-sorted, speedup-scaled, optionally
//! truncated — and arms a per-request disconnect timer for requests whose
//! recorded outcome was an interruption (a cancellation or a client
//! disconnect). [`build_timeout_for`] derives the per-request build deadline
//! from recorded durations, [`lateness_summary`] condenses per-request
//! dispatch lateness into max/p50/p95, and [`re_anchor_pending`] shifts a
//! partially completed schedule at resume time so pending requests fire
//! immediately while keeping their recorded relative spacing.
//!
//! [`run_timed_dispatch`] executes a built schedule: one task per request
//! sleeps until its due time, takes a FIFO admission permit capped at
//! `max_sessions`, optionally tops up supply for the request's roots, and
//! submits the recorded request as one timed batch through the shared
//! [`Submitter`] seam. Requests with an armed interruption are submitted
//! under the recorded disconnect deadline so the channel is abandoned at
//! the recorded relative time; units expected to build whose replayed
//! result is a failure are re-confirmed on fresh single-position batches up
//! to the confirmation budget. Every request settles into one
//! dispatch.jsonl line and the run aggregates into [`TimedRunStats`].
//!
//! Schedule construction is pure over engine-owned input types
//! ([`RecordedRequest`], [`RecordedTiming`]); loading those inputs from a
//! replay archive and choosing the scheduling mode belong to the run
//! orchestration, not here.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::batch::Batch;
use super::model::{
    BATCH_KIND_TIMED, BatchRecord, DispatchEntry, build_status_from_name, now_rfc3339,
};
use super::spec::Knobs;
use super::state::{StateDir, StateFile};
use super::submit::{SubmitTracker, submit_one_batch};
use super::submitter::Submitter;
use super::supply::exec::PreSubmitSupply;

/// Disconnect delay assumed when a recorded interruption carries no stop
/// offset (scaled by the speedup like a recorded one).
const DEFAULT_DISCONNECT_DELAY_S: f64 = 60.0;

/// Lower bound on the scheduled disconnect delay, so the interrupted build
/// is always actually submitted before the channel is dropped (a high
/// speedup or a tiny recorded gap cannot turn the replay into a no-op).
const DISCONNECT_FLOOR: Duration = Duration::from_secs(1);

/// One recorded client submission, as loaded from the archive at the wiring
/// point.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedRequest {
    /// Opaque grouping key for the recorded client connection.
    pub session: i64,
    /// Seconds after the recording started at which the request was made.
    pub offset_s: f64,
    /// The derivations (and outputs) the request asked for.
    pub targets: Vec<RecordedTarget>,
}

/// One requested derivation within a [`RecordedRequest`].
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedTarget {
    /// Store path of the requested derivation.
    pub drv: String,
    /// Requested output names; `[]` and `["*"]` both mean every output.
    pub outputs: Vec<String>,
}

/// Per-unit timing/interruption truth needed for scheduling (subset of the
/// archive's expected-outcome record).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecordedTiming {
    /// Wall-clock duration of the source attempt, in seconds.
    pub duration_s: Option<f64>,
    /// Seconds after the recording started at which the source attempt
    /// stopped.
    pub stop_offset_s: Option<f64>,
    /// The recorded outcome was an interruption (a cancellation or a client
    /// disconnect) eligible for replay.
    pub interrupted: bool,
    /// The recorded expected outcome was a successful build, making the
    /// unit eligible for a confirmation retry when its replayed result is a
    /// failure.
    pub expected_built: bool,
}

/// One recorded request, scheduled for replay.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduledRequest {
    /// Unique per-run id (index into the schedule).
    pub index: usize,
    /// The recorded request being replayed.
    pub request: RecordedRequest,
    /// When to dispatch, relative to the run start: `offset_s / speedup`.
    pub due: Duration,
    /// When interruption replay is armed for this request: how long after
    /// dispatch to abandon the channel
    /// (`(stop_offset_s − offset_s).max(0) / speedup`, or 60 s divided by
    /// the speedup when no stop offset was recorded, never below 1 s);
    /// `None` otherwise.
    pub disconnect_after: Option<Duration>,
    /// The targets whose recorded timing was interrupted — the units the
    /// armed disconnect timer stands in for. Empty whenever interruption
    /// replay is disabled or nothing in the request was interrupted, so a
    /// timer is armed exactly when this list is non-empty.
    pub interruption_drvs: Vec<String>,
}

/// Sort the recorded requests by offset, apply the optional limit, and
/// compute each request's due time and (when interruption replay is
/// enabled) its disconnect timer and interrupted targets.
///
/// `timing` answers per-`(session, drv)` lookups; it is a closure so callers
/// can serve it from whatever expected-outcome index they hold.
pub fn build_schedule(
    requests: &[RecordedRequest],
    timing: &dyn Fn(i64, &str) -> Option<RecordedTiming>,
    speedup: f64,
    limit: Option<usize>,
    replay_interruptions: bool,
) -> Vec<ScheduledRequest> {
    let mut sorted: Vec<RecordedRequest> = requests.to_vec();
    // Stable sort: requests recorded at the same offset keep their input
    // order.
    sorted.sort_by(|a, b| a.offset_s.total_cmp(&b.offset_s));
    if let Some(limit) = limit {
        sorted.truncate(limit);
    }
    sorted
        .into_iter()
        .enumerate()
        .map(|(index, request)| {
            let offset = request.offset_s.max(0.0);
            let due = Duration::from_secs_f64(offset / speedup);
            let (disconnect_after, interruption_drvs) = if replay_interruptions {
                (
                    disconnect_after_for(&request, timing, speedup),
                    interrupted_targets(&request, timing),
                )
            } else {
                (None, Vec::new())
            };
            ScheduledRequest {
                index,
                request,
                due,
                disconnect_after,
                interruption_drvs,
            }
        })
        .collect()
}

/// Disconnect timer for one request: present only when at least one of its
/// targets has an interrupted recorded timing. The delay is the recorded
/// dispatch-to-stop gap scaled by the speedup ([`DEFAULT_DISCONNECT_DELAY_S`]
/// when the record carries no stop offset), never below
/// [`DISCONNECT_FLOOR`].
fn disconnect_after_for(
    request: &RecordedRequest,
    timing: &dyn Fn(i64, &str) -> Option<RecordedTiming>,
    speedup: f64,
) -> Option<Duration> {
    let offset = request.offset_s.max(0.0);
    let interrupted = request
        .targets
        .iter()
        .find_map(|target| timing(request.session, &target.drv).filter(|t| t.interrupted))?;
    let scaled = match interrupted.stop_offset_s {
        Some(stop) => (stop - offset).max(0.0) / speedup,
        None => DEFAULT_DISCONNECT_DELAY_S / speedup,
    };
    Some(Duration::from_secs_f64(scaled).max(DISCONNECT_FLOOR))
}

/// The request's targets whose recorded timing was an interruption — the
/// units a replayed disconnect stands in for.
fn interrupted_targets(
    request: &RecordedRequest,
    timing: &dyn Fn(i64, &str) -> Option<RecordedTiming>,
) -> Vec<String> {
    request
        .targets
        .iter()
        .filter(|target| {
            timing(request.session, &target.drv).is_some_and(|record| record.interrupted)
        })
        .map(|target| target.drv.clone())
        .collect()
}

/// Build deadline for one scheduled request: twice the slowest recorded
/// duration among its targets, clamped to `[floor, cap]`; the floor alone
/// when no target carries a recorded duration.
pub fn build_timeout_for(
    scheduled: &ScheduledRequest,
    timing: &dyn Fn(i64, &str) -> Option<RecordedTiming>,
    floor: Duration,
    cap: Duration,
) -> Duration {
    let session = scheduled.request.session;
    let slowest = scheduled
        .request
        .targets
        .iter()
        .filter_map(|target| timing(session, &target.drv).and_then(|record| record.duration_s))
        .fold(None::<f64>, |acc, duration| {
            Some(acc.map_or(duration, |current| current.max(duration)))
        });
    match slowest {
        Some(duration) if duration.is_finite() && duration > 0.0 => {
            // Cap before converting so an absurd recorded duration cannot
            // overflow the conversion; then apply the floor.
            let capped = (2.0 * duration).min(cap.as_secs_f64());
            Duration::from_secs_f64(capped).max(floor)
        }
        _ => floor,
    }
}

/// `"<drv>!out1,out2"` / `"<drv>!*"` formatting for `BuildPathsWithResults`:
/// `[]` and `["*"]` both mean every output.
pub fn format_derived(drv: &str, outputs: &[String]) -> String {
    if outputs.is_empty() || (outputs.len() == 1 && outputs[0] == "*") {
        format!("{drv}!*")
    } else {
        format!("{drv}!{}", outputs.join(","))
    }
}

/// Max / p50 / p95 dispatch lateness over one run, in milliseconds.
///
/// Percentiles are nearest-rank over the sorted samples (the smallest sample
/// with at least the requested share of samples at or below it); an empty
/// slice summarizes to all zeros.
pub fn lateness_summary(lateness_ms: &[u64]) -> (u64, u64, u64) {
    if lateness_ms.is_empty() {
        return (0, 0, 0);
    }
    let mut sorted = lateness_ms.to_vec();
    sorted.sort_unstable();
    let max = *sorted.last().expect("slice checked non-empty above");
    let nearest_rank = |percentile: f64| -> u64 {
        let rank = ((percentile / 100.0) * sorted.len() as f64).ceil() as usize;
        sorted[rank.clamp(1, sorted.len()) - 1]
    };
    (max, nearest_rank(50.0), nearest_rank(95.0))
}

/// Re-anchor a partially completed schedule at resume time.
///
/// Requests whose index is in `already_terminal` keep their original slot;
/// the earliest pending request becomes due at `now_offset` and every other
/// pending request shifts by the same amount, so the recorded relative
/// spacing between pending requests is preserved and no pending request is
/// ever scheduled earlier than `now_offset`.
pub fn re_anchor_pending(
    scheduled: &mut [ScheduledRequest],
    already_terminal: &HashSet<usize>,
    now_offset: Duration,
) {
    let Some(earliest_pending) = scheduled
        .iter()
        .filter(|entry| !already_terminal.contains(&entry.index))
        .map(|entry| entry.due)
        .min()
    else {
        return;
    };
    for entry in scheduled
        .iter_mut()
        .filter(|entry| !already_terminal.contains(&entry.index))
    {
        // Every pending due is >= the earliest pending due by construction;
        // saturate anyway so a malformed slice degrades to "due now" instead
        // of panicking.
        entry.due = now_offset + entry.due.saturating_sub(earliest_pending);
    }
}

/// Timed-dispatcher tuning derived from the campaign knobs once at the
/// wiring point, so the per-request tasks never re-read [`Knobs`] for the
/// values the dispatcher itself owns.
#[derive(Debug, Clone)]
pub struct TimelineConfig {
    /// Concurrently admitted requests (FIFO admission permits = concurrently
    /// held build channels).
    pub max_sessions: usize,
    /// Total submission attempts for a unit whose expected outcome is built
    /// but whose replayed result is a failure (initial submission included).
    pub confirm_attempts: u32,
    /// Recorded-offset divisor the schedule was built with.
    pub speedup: f64,
    /// Whether recorded interruptions are replayed via channel abandon.
    pub replay_interruptions: bool,
    /// Floor on a request's build deadline.
    pub build_timeout_floor: Duration,
    /// Cap on a request's build deadline.
    pub build_timeout_cap: Duration,
    /// Per-op deadline for non-build client ops issued on behalf of a
    /// request (probes, top-up uploads).
    pub op_timeout: Duration,
    /// How long a request waits on another request's upload claim for a
    /// shared path before re-claiming it.
    pub claim_wait: Duration,
}

impl TimelineConfig {
    /// Field-by-field translation from the spec knobs. Zero values that
    /// would deadlock or zero out deadlines are clamped to their smallest
    /// useful value (spec validation already rejects them; the clamp keeps
    /// hand-built [`Knobs`] in tests safe).
    pub fn from_knobs(k: &Knobs) -> Self {
        Self {
            max_sessions: k.max_sessions.max(1),
            confirm_attempts: k.confirm_attempts.max(1),
            speedup: k.speedup,
            replay_interruptions: k.replay_interruptions,
            build_timeout_floor: Duration::from_secs(k.build_timeout_floor_mins.saturating_mul(60)),
            build_timeout_cap: Duration::from_secs_f64(
                (k.build_timeout_cap_hours * 3600.0).max(1.0),
            ),
            op_timeout: Duration::from_secs(k.op_timeout_secs.max(1)),
            claim_wait: Duration::from_secs(k.claim_wait_mins.saturating_mul(60)),
        }
    }
}

/// Shareable per-`(session, drv)` recorded-timing lookup, served from
/// whatever expected-outcome index the caller holds (the dispatcher's
/// `'static` counterpart of the borrowed closure [`build_schedule`] takes).
pub type SharedTimingLookup = Arc<dyn Fn(i64, &str) -> Option<RecordedTiming> + Send + Sync>;

/// Aggregate outcome of one timed dispatch run, reported under the `timed`
/// progress/report block.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TimedRunStats {
    /// Scheduled requests this run knew about (including ones skipped as
    /// already terminal on resume).
    pub requests_total: usize,
    /// Requests actually submitted by this run.
    pub dispatched: usize,
    /// Largest dispatch lateness observed, in milliseconds.
    pub max_dispatch_lateness_ms: u64,
    /// Median dispatch lateness, in milliseconds.
    pub lateness_p50_ms: u64,
    /// 95th-percentile dispatch lateness, in milliseconds.
    pub lateness_p95_ms: u64,
    /// Armed units whose recorded interruption was reproduced (the channel
    /// was abandoned at the recorded relative time).
    pub interruptions_replayed: usize,
    /// Armed units that completed successfully before the recorded
    /// interruption offset.
    pub interruptions_not_reproduced: usize,
    /// Timed submissions that settled with neither in-band results nor an
    /// observed build id and were not engine cancellations — engine-side
    /// submission failures. Their members get no terminal record from
    /// collect and the dispatcher does not re-offer them, so they are
    /// accounted here and fall to the end-of-run not-attempted backfill.
    #[serde(default)]
    pub submission_failures: usize,
    /// Requests skipped because every target was already terminal from a
    /// prior run that had dispatched them.
    pub resume_count: usize,
    /// True when this run resumed a previous timed run, so the recorded
    /// cadence was re-anchored and the timing-fidelity numbers are
    /// low-confidence.
    pub timing_degraded: bool,
}

/// Everything the per-request dispatch tasks share; cloned once per task
/// behind an [`Arc`].
struct DispatchShared {
    state: Arc<StateDir>,
    submitter: Arc<dyn Submitter>,
    tracker: Arc<SubmitTracker>,
    job_of_drv: Arc<BTreeMap<String, String>>,
    timing: SharedTimingLookup,
    deadline_reached: Arc<dyn Fn() -> bool + Send + Sync>,
    topup: Option<Arc<dyn PreSubmitSupply>>,
    semaphore: Arc<Semaphore>,
    store_url: String,
    config: TimelineConfig,
    batch_seq: Arc<AtomicU64>,
    /// Post-settlement cool-down handed to [`submit_one_batch`]; only
    /// meaningful to the timeless re-offer machinery, passed through for
    /// tracker consistency.
    cooldown: Duration,
    /// The run anchor every due time is relative to.
    start: tokio::time::Instant,
}

/// What one per-request task contributed to the run statistics.
struct RequestOutcome {
    /// The request was submitted (not skipped at the campaign deadline).
    dispatched: bool,
    /// Dispatch lateness when the request was admitted; `None` when it was
    /// never admitted.
    lateness_ms: Option<u64>,
    /// Armed units whose interruption was reproduced.
    replayed: usize,
    /// Armed units that completed before the interruption offset.
    not_reproduced: usize,
    /// Submissions of this request (initial or confirmation retry) that
    /// failed engine-side — see [`engine_side_submission_failure`].
    submission_failures: usize,
}

/// True when a settled timed submission produced neither in-band results
/// nor an observed build id and was not an engine cancellation: the shape
/// of an engine-side submission failure (channel open, drv import, the
/// build op erroring before any result arrived). Collect writes no terminal
/// record for such a batch's members and the timed dispatcher never
/// re-offers them, so the run statistics account for them explicitly.
fn engine_side_submission_failure(record: &BatchRecord) -> bool {
    record.results.is_empty() && record.build_id.is_none() && !record.engine_cancelled
}

/// Dispatch a built timed schedule: fire each pending request at its due
/// time under FIFO admission, replay recorded interruptions via the
/// disconnect deadline, re-confirm unexpected failures, and append one
/// [`DispatchEntry`] per request.
///
/// Resume semantics: requests whose every target is already terminal (per
/// `terminal_jobs`, keyed by job) are skipped — counted into `resume_count`
/// when a prior dispatch.jsonl entry shows they were dispatched before —
/// and, when any prior dispatch entry exists, the pending schedule is
/// re-anchored so the first pending request fires immediately and
/// `timing_degraded` is set.
///
/// The campaign deadline stops new dispatches only: a request whose sleep
/// completes after `deadline_reached()` is recorded with `attempts: 0` and
/// not submitted. The dispatcher never consults the pause state (pause is
/// advisory in timed mode) and never registers queued jobs with the
/// watchdog; in-flight requests surface to the watchdog as Active through
/// the shared `tracker`, which is why the caller passes the same
/// [`SubmitTracker`] the poller reads.
#[allow(clippy::too_many_arguments)]
pub async fn run_timed_dispatch(
    state: Arc<StateDir>,
    submitter: Arc<dyn Submitter>,
    tracker: Arc<SubmitTracker>,
    mut schedule: Vec<ScheduledRequest>,
    job_of_drv: Arc<BTreeMap<String, String>>,
    timing: SharedTimingLookup,
    store_url: String,
    config: TimelineConfig,
    knobs: Knobs,
    batch_seq: Arc<AtomicU64>,
    deadline_reached: impl Fn() -> bool + Send + Sync + Clone + 'static,
    topup: Option<Arc<dyn PreSubmitSupply>>,
    terminal_jobs: Arc<tokio::sync::Mutex<HashSet<String>>>,
) -> Result<TimedRunStats> {
    let requests_total = schedule.len();

    // ── Resume ──────────────────────────────────────────────────────────
    // Prior dispatch entries prove a previous dispatcher run happened;
    // terminal jobs decide which requests are already settled and must not
    // be re-submitted.
    let prior_entries: Vec<DispatchEntry> = state.load_jsonl(StateFile::Dispatch)?;
    let prior_indexes: HashSet<usize> = prior_entries
        .iter()
        .map(|entry| entry.request_index)
        .collect();
    let terminal_snapshot = terminal_jobs.lock().await.clone();
    let mut already_terminal: HashSet<usize> = HashSet::new();
    let mut resume_count = 0usize;
    for entry in &schedule {
        let all_terminal = !entry.request.targets.is_empty()
            && entry.request.targets.iter().all(|target| {
                job_of_drv
                    .get(&target.drv)
                    .is_some_and(|job| terminal_snapshot.contains(job))
            });
        if all_terminal {
            already_terminal.insert(entry.index);
            if prior_indexes.contains(&entry.index) {
                resume_count += 1;
            }
        }
    }
    let resumed = !prior_entries.is_empty();
    if resumed {
        // Pending requests fire immediately, preserving recorded relative
        // spacing; a fresh run keeps the recorded absolute offsets instead.
        re_anchor_pending(&mut schedule, &already_terminal, Duration::ZERO);
    }
    let pending = schedule
        .iter()
        .filter(|entry| !already_terminal.contains(&entry.index))
        .count();
    tracing::info!(
        requests_total,
        pending,
        skipped_terminal = already_terminal.len(),
        resume_count,
        resumed,
        max_sessions = config.max_sessions,
        replay_interruptions = config.replay_interruptions,
        "timed dispatch starting"
    );

    // ── Dispatch ────────────────────────────────────────────────────────
    let shared = Arc::new(DispatchShared {
        state,
        submitter,
        tracker,
        job_of_drv,
        timing,
        deadline_reached: Arc::new(deadline_reached),
        topup,
        semaphore: Arc::new(Semaphore::new(config.max_sessions.max(1))),
        store_url,
        config,
        batch_seq,
        // One collect poll interval, mirroring the timeless loop, so the
        // shared tracker behaves identically whichever mode filled it.
        cooldown: Duration::from_secs(knobs.collect_poll_secs.max(1)),
        start: tokio::time::Instant::now(),
    });
    let mut join_set: JoinSet<Result<RequestOutcome>> = JoinSet::new();
    for scheduled in schedule {
        if already_terminal.contains(&scheduled.index) {
            continue;
        }
        let shared = shared.clone();
        join_set.spawn(async move { dispatch_one_request(shared, scheduled).await });
    }

    // ── Aggregate ───────────────────────────────────────────────────────
    let mut lateness_samples: Vec<u64> = Vec::new();
    let mut dispatched = 0usize;
    let mut interruptions_replayed = 0usize;
    let mut interruptions_not_reproduced = 0usize;
    let mut submission_failures = 0usize;
    while let Some(joined) = join_set.join_next().await {
        let outcome =
            joined.map_err(|e| anyhow::anyhow!("timed dispatch task panicked: {e}"))??;
        if outcome.dispatched {
            dispatched += 1;
        }
        if let Some(ms) = outcome.lateness_ms {
            lateness_samples.push(ms);
        }
        interruptions_replayed += outcome.replayed;
        interruptions_not_reproduced += outcome.not_reproduced;
        submission_failures += outcome.submission_failures;
    }
    let (max_dispatch_lateness_ms, lateness_p50_ms, lateness_p95_ms) =
        lateness_summary(&lateness_samples);
    tracing::info!(
        dispatched,
        max_dispatch_lateness_ms,
        interruptions_replayed,
        interruptions_not_reproduced,
        submission_failures,
        "timed dispatch drained"
    );
    Ok(TimedRunStats {
        requests_total,
        dispatched,
        max_dispatch_lateness_ms,
        lateness_p50_ms,
        lateness_p95_ms,
        interruptions_replayed,
        interruptions_not_reproduced,
        submission_failures,
        resume_count,
        timing_degraded: resumed,
    })
}

/// Drive one scheduled request end to end: sleep to its due time, take an
/// admission permit, top up supply, submit it as one timed batch, run any
/// confirmation retries, and append its [`DispatchEntry`].
async fn dispatch_one_request(
    shared: Arc<DispatchShared>,
    scheduled: ScheduledRequest,
) -> Result<RequestOutcome> {
    let due_at = shared.start + scheduled.due;
    tokio::time::sleep_until(due_at).await;
    if (shared.deadline_reached)() {
        return record_deadline_skip(&shared, &scheduled);
    }

    // FIFO admission: permits are granted in acquire order, i.e. due-time
    // order, and held until the request fully settles (confirmation retries
    // included) so `max_sessions` bounds concurrently held build channels.
    let permit = shared
        .semaphore
        .clone()
        .acquire_owned()
        .await
        .expect("admission semaphore is never closed");
    if (shared.deadline_reached)() {
        // The permit wait can outlive the deadline; skip rather than start a
        // submission the campaign no longer wants.
        drop(permit);
        return record_deadline_skip(&shared, &scheduled);
    }
    let _admission_permit = permit;
    let admitted_at = tokio::time::Instant::now();
    let lateness = admitted_at.saturating_duration_since(due_at);
    let lateness_ms = u64::try_from(lateness.as_millis()).unwrap_or(u64::MAX);
    let dispatched_at = now_rfc3339();

    let drvs: Vec<String> = scheduled
        .request
        .targets
        .iter()
        .map(|target| target.drv.clone())
        .collect();
    let jobs: Vec<String> = drvs
        .iter()
        .map(|drv| {
            // Targets without a workload-unit mapping keep their drv path as
            // the job key: collect skips them (no job context) but the batch
            // bookkeeping and watchdog visibility stay complete.
            shared
                .job_of_drv
                .get(drv)
                .cloned()
                .unwrap_or_else(|| drv.clone())
        })
        .collect();

    // Inline top-up fallback (prewarm-miss / inline delivery): a failure
    // degrades to a log line, never aborts the request.
    if let Some(topup) = &shared.topup
        && let Err(e) = topup.topup(&drvs).await
    {
        tracing::warn!(
            request = scheduled.index,
            error = %format!("{e:#}"),
            "pre-submission supply top-up failed; dispatching the request anyway"
        );
    }

    // Build deadline from recorded durations; the interruption deadline is
    // anchored at the admission instant so supply/top-up time never shifts
    // the recorded disconnect offset.
    let timing_fn = |session: i64, drv: &str| (shared.timing)(session, drv);
    let build_deadline = build_timeout_for(
        &scheduled,
        &timing_fn,
        shared.config.build_timeout_floor,
        shared.config.build_timeout_cap,
    );
    let timeout = match scheduled.disconnect_after {
        Some(after) => {
            let remaining = after
                .saturating_sub(admitted_at.elapsed())
                .max(DISCONNECT_FLOOR);
            build_deadline.min(remaining)
        }
        None => build_deadline,
    };
    let armed = !scheduled.interruption_drvs.is_empty();

    // Reserve the jobs as in flight so the shared tracker (and through it
    // the watchdog poller) sees this request as Active while it runs;
    // submit_one_batch releases the reservation when the batch settles.
    {
        let mut in_flight = shared.tracker.in_flight.lock().await;
        for job in &jobs {
            in_flight.insert(job.clone());
        }
    }
    let batch_id = shared.batch_seq.fetch_add(1, Ordering::SeqCst);
    tracing::info!(
        request = scheduled.index,
        session = scheduled.request.session,
        batch_id,
        targets = drvs.len(),
        lateness_ms,
        timeout_secs = timeout.as_secs(),
        interruption_armed = armed,
        "dispatching timed request"
    );
    let record = submit_one_batch(
        &shared.state,
        shared.submitter.as_ref(),
        &shared.tracker,
        &shared.store_url,
        BATCH_KIND_TIMED,
        batch_id,
        Batch {
            jobs: jobs.clone(),
            root_drvs: drvs.clone(),
            est_nodes: drvs.len(),
        },
        timeout,
        shared.cooldown,
        scheduled.interruption_drvs.clone(),
    )
    .await?;
    let mut batch_ids = vec![batch_id];
    let mut attempts = 1u32;
    let mut submission_failures = usize::from(engine_side_submission_failure(&record));

    // Interruption accounting per armed unit, mirroring how collect buckets
    // them: an engine cancellation reproduces the recorded interruption; a
    // unit that settled with any in-band success status out-raced it.
    let fired = armed && record.engine_cancelled;
    let (replayed, not_reproduced) = if armed {
        if record.engine_cancelled {
            (scheduled.interruption_drvs.len(), 0)
        } else {
            let succeeded = scheduled
                .interruption_drvs
                .iter()
                .filter(|drv| in_band_success(&record, drv))
                .count();
            (0, succeeded)
        }
    } else {
        (0, 0)
    };

    // Confirmation retries: positions whose recorded expectation is built
    // but whose replayed result is a failure are re-submitted alone (only
    // the failing positions) up to the confirmation budget. Interruption-
    // armed requests never confirmation-retry — the recorded outcome being
    // reproduced is the point — and the campaign deadline stops further
    // retries like any other new submission.
    if !armed {
        let mut last_status: BTreeMap<String, String> = record
            .results
            .iter()
            .map(|result| (result.drv_path.clone(), result.status.clone()))
            .collect();
        loop {
            let failing: Vec<String> = scheduled
                .request
                .targets
                .iter()
                .filter(|target| {
                    let expected_built = (shared.timing)(scheduled.request.session, &target.drv)
                        .is_some_and(|timing| timing.expected_built);
                    expected_built
                        && last_status.get(&target.drv).is_some_and(|status| {
                            !build_status_from_name(status).is_some_and(|s| s.is_success())
                        })
                })
                .map(|target| target.drv.clone())
                .collect();
            if failing.is_empty()
                || attempts >= shared.config.confirm_attempts
                || (shared.deadline_reached)()
            {
                break;
            }
            let retry_jobs: Vec<String> = failing
                .iter()
                .map(|drv| {
                    shared
                        .job_of_drv
                        .get(drv)
                        .cloned()
                        .unwrap_or_else(|| drv.clone())
                })
                .collect();
            {
                let mut in_flight = shared.tracker.in_flight.lock().await;
                for job in &retry_jobs {
                    in_flight.insert(job.clone());
                }
            }
            let retry_id = shared.batch_seq.fetch_add(1, Ordering::SeqCst);
            tracing::info!(
                request = scheduled.index,
                batch_id = retry_id,
                attempt = attempts + 1,
                positions = failing.len(),
                "re-confirming unexpected failures on a fresh timed batch"
            );
            let retry_record = submit_one_batch(
                &shared.state,
                shared.submitter.as_ref(),
                &shared.tracker,
                &shared.store_url,
                BATCH_KIND_TIMED,
                retry_id,
                Batch {
                    jobs: retry_jobs,
                    root_drvs: failing.clone(),
                    est_nodes: failing.len(),
                },
                build_deadline,
                shared.cooldown,
                Vec::new(),
            )
            .await?;
            attempts += 1;
            batch_ids.push(retry_id);
            submission_failures += usize::from(engine_side_submission_failure(&retry_record));
            for result in &retry_record.results {
                last_status.insert(result.drv_path.clone(), result.status.clone());
            }
        }
    }

    shared.state.append_jsonl(
        StateFile::Dispatch,
        &DispatchEntry {
            request_index: scheduled.index,
            session: scheduled.request.session,
            due_offset_s: scheduled.due.as_secs_f64(),
            dispatched_at,
            dispatch_lateness_ms: lateness_ms,
            deadline_secs: timeout.as_secs(),
            interruption_armed: armed,
            interruption_fired: fired,
            attempts,
            batch_ids,
            drvs,
        },
    )?;
    Ok(RequestOutcome {
        dispatched: true,
        lateness_ms: Some(lateness_ms),
        replayed,
        not_reproduced,
        submission_failures,
    })
}

/// True when the batch settled an in-band result for `drv` with any
/// success status (built, substituted, already valid).
fn in_band_success(record: &BatchRecord, drv: &str) -> bool {
    record.results.iter().any(|result| {
        result.drv_path == drv
            && build_status_from_name(&result.status).is_some_and(|status| status.is_success())
    })
}

/// Record a request the campaign deadline prevented from dispatching:
/// one dispatch.jsonl entry with zero attempts, nothing submitted.
fn record_deadline_skip(
    shared: &DispatchShared,
    scheduled: &ScheduledRequest,
) -> Result<RequestOutcome> {
    let timing_fn = |session: i64, drv: &str| (shared.timing)(session, drv);
    let build_deadline = build_timeout_for(
        scheduled,
        &timing_fn,
        shared.config.build_timeout_floor,
        shared.config.build_timeout_cap,
    );
    tracing::info!(
        request = scheduled.index,
        session = scheduled.request.session,
        "campaign deadline reached before this request was admitted; not dispatching it"
    );
    shared.state.append_jsonl(
        StateFile::Dispatch,
        &DispatchEntry {
            request_index: scheduled.index,
            session: scheduled.request.session,
            due_offset_s: scheduled.due.as_secs_f64(),
            dispatched_at: now_rfc3339(),
            dispatch_lateness_ms: 0,
            deadline_secs: build_deadline.as_secs(),
            interruption_armed: !scheduled.interruption_drvs.is_empty(),
            interruption_fired: false,
            attempts: 0,
            batch_ids: Vec::new(),
            drvs: scheduled
                .request
                .targets
                .iter()
                .map(|target| target.drv.clone())
                .collect(),
        },
    )?;
    Ok(RequestOutcome {
        dispatched: false,
        lateness_ms: None,
        replayed: 0,
        not_reproduced: 0,
        submission_failures: 0,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use rio_nix::protocol::build::BuildStatus;

    use crate::run::collect::{BatchView, JobContext, process_settled_batch};
    use crate::run::grpc::test_support::FakeStoreApi;
    use crate::run::grpc::{AdminApi, GraphSnapshot, PoisonedView};
    use crate::run::model::{
        Bucket, HydraOutcome, HydraSide, JobRecord, PathOutcome, RioSide, build_status_name,
    };
    use crate::run::state::latest_per_job;
    use crate::run::submitter::BatchOutcome;
    use crate::run::submitter::test_support::FakeSubmitter;

    use super::*;

    const DRV_A: &str = "/nix/store/a1111111111111111111111111111111-a.drv";
    const DRV_B: &str = "/nix/store/b2222222222222222222222222222222-b.drv";
    const DRV_C: &str = "/nix/store/c3333333333333333333333333333333-c.drv";
    const DRV_D: &str = "/nix/store/d4444444444444444444444444444444-d.drv";

    /// Single-target request for fabricated-schedule cases.
    fn request(session: i64, offset_s: f64, drv: &str) -> RecordedRequest {
        RecordedRequest {
            session,
            offset_s,
            targets: vec![RecordedTarget {
                drv: drv.to_string(),
                outputs: vec!["out".to_string()],
            }],
        }
    }

    /// Timing lookup that knows nothing — no durations, no interruptions.
    fn no_timing(_: i64, _: &str) -> Option<RecordedTiming> {
        None
    }

    /// Timing lookup backed by an explicit `(session, drv)` map.
    fn timing_in(
        map: &HashMap<(i64, String), RecordedTiming>,
    ) -> impl Fn(i64, &str) -> Option<RecordedTiming> + '_ {
        move |session, drv| map.get(&(session, drv.to_string())).cloned()
    }

    /// Recorded timing for an interrupted attempt with an optional stop
    /// offset.
    fn interrupted(stop_offset_s: Option<f64>) -> RecordedTiming {
        RecordedTiming {
            duration_s: None,
            stop_offset_s,
            interrupted: true,
            expected_built: false,
        }
    }

    /// Schedule entry with just an index and a due time, for re-anchoring
    /// cases.
    fn scheduled(index: usize, due_secs: u64) -> ScheduledRequest {
        ScheduledRequest {
            index,
            request: request(index as i64, due_secs as f64, DRV_A),
            due: Duration::from_secs(due_secs),
            disconnect_after: None,
            interruption_drvs: Vec::new(),
        }
    }

    #[test]
    fn build_schedule_sorts_limits_and_scales() {
        // Recorded out of order; offsets 0.25/2.0/5.5/9.0 at speedup 2.0:
        // due times halve, order is by recorded offset, indices follow the
        // schedule order.
        let requests = vec![
            request(11, 5.5, DRV_B),
            request(10, 0.25, DRV_A),
            request(12, 9.0, DRV_C),
            request(13, 2.0, DRV_D),
        ];
        let schedule = build_schedule(&requests, &no_timing, 2.0, None, true);
        assert_eq!(schedule.len(), 4);
        let due: Vec<Duration> = schedule.iter().map(|entry| entry.due).collect();
        assert_eq!(
            due,
            vec![
                Duration::from_millis(125),
                Duration::from_millis(1000),
                Duration::from_millis(2750),
                Duration::from_millis(4500),
            ]
        );
        let sessions: Vec<i64> = schedule.iter().map(|entry| entry.request.session).collect();
        assert_eq!(sessions, vec![10, 13, 11, 12]);
        let indices: Vec<usize> = schedule.iter().map(|entry| entry.index).collect();
        assert_eq!(indices, vec![0, 1, 2, 3]);

        // The limit keeps the first N by offset.
        let limited = build_schedule(&requests, &no_timing, 2.0, Some(2), true);
        assert_eq!(limited.len(), 2);
        let limited_sessions: Vec<i64> =
            limited.iter().map(|entry| entry.request.session).collect();
        assert_eq!(limited_sessions, vec![10, 13]);

        // A fabricated negative offset clamps to a zero due time and sorts
        // ahead of everything else.
        let mut requests = requests.clone();
        requests.push(request(99, -3.5, DRV_A));
        let schedule = build_schedule(&requests, &no_timing, 2.0, None, false);
        assert_eq!(schedule[0].request.session, 99);
        assert_eq!(schedule[0].due, Duration::ZERO);
    }

    #[test]
    fn disconnect_after_uses_stop_offset() {
        // Only session 12's target has an interrupted recorded timing,
        // stopping at offset 11.0 for a request at offset 9.0:
        // (11 - 9) / 2.0 = 1s. Everything else stays unarmed; a
        // non-interrupted timing record never arms a timer.
        let requests = vec![
            request(10, 0.25, DRV_A),
            request(13, 2.0, DRV_D),
            request(11, 5.5, DRV_B),
            request(12, 9.0, DRV_C),
        ];
        let mut map = HashMap::new();
        map.insert((12_i64, DRV_C.to_string()), interrupted(Some(11.0)));
        map.insert(
            (10_i64, DRV_A.to_string()),
            RecordedTiming {
                duration_s: Some(3.0),
                stop_offset_s: None,
                interrupted: false,
                expected_built: false,
            },
        );
        let timing = timing_in(&map);
        let schedule = build_schedule(&requests, &timing, 2.0, None, true);
        let timers: Vec<Option<Duration>> = schedule
            .iter()
            .map(|entry| entry.disconnect_after)
            .collect();
        assert_eq!(timers, vec![None, None, None, Some(Duration::from_secs(1))]);
        // The armed request also names which targets the timer stands in
        // for; unarmed requests name none.
        assert_eq!(schedule[3].interruption_drvs, vec![DRV_C.to_string()]);
        assert!(
            schedule[..3]
                .iter()
                .all(|entry| entry.interruption_drvs.is_empty())
        );

        // Interruption replay disabled: no timers and nothing armed at all.
        let schedule = build_schedule(&requests, &timing, 2.0, None, false);
        assert!(
            schedule
                .iter()
                .all(|entry| entry.disconnect_after.is_none()
                    && entry.interruption_drvs.is_empty())
        );

        // An interruption without a stop offset falls back to 60s scaled by
        // the speedup.
        let requests = vec![request(50, 4.0, DRV_A)];
        let mut map = HashMap::new();
        map.insert((50_i64, DRV_A.to_string()), interrupted(None));
        let timing = timing_in(&map);
        let schedule = build_schedule(&requests, &timing, 2.0, None, true);
        assert_eq!(schedule[0].disconnect_after, Some(Duration::from_secs(30)));

        // The 1s floor holds when the recorded gap is tiny and the speedup
        // is high.
        let requests = vec![request(51, 5.0, DRV_A)];
        let mut map = HashMap::new();
        map.insert((51_i64, DRV_A.to_string()), interrupted(Some(5.001)));
        let timing = timing_in(&map);
        let schedule = build_schedule(&requests, &timing, 100.0, None, true);
        assert_eq!(schedule[0].disconnect_after, Some(Duration::from_secs(1)));
    }

    #[test]
    fn format_derived_forms() {
        let drv = DRV_A;
        assert_eq!(
            format_derived(drv, &["out".to_string()]),
            format!("{drv}!out")
        );
        assert_eq!(format_derived(drv, &["*".to_string()]), format!("{drv}!*"));
        assert_eq!(format_derived(drv, &[]), format!("{drv}!*"));
        assert_eq!(
            format_derived(drv, &["out".to_string(), "dev".to_string()]),
            format!("{drv}!out,dev")
        );
    }

    #[test]
    fn build_timeout_scales_clamps_and_falls_back() {
        let floor = Duration::from_secs(30 * 60);
        let cap = Duration::from_secs(2 * 60 * 60);
        let session = 7_i64;

        let single = |duration: Option<f64>| {
            let schedule = build_schedule(
                &[request(session, 0.0, DRV_A)],
                &no_timing,
                1.0,
                None,
                false,
            );
            let mut map = HashMap::new();
            map.insert(
                (session, DRV_A.to_string()),
                RecordedTiming {
                    duration_s: duration,
                    stop_offset_s: None,
                    interrupted: false,
                    expected_built: false,
                },
            );
            let timing = timing_in(&map);
            build_timeout_for(&schedule[0], &timing, floor, cap)
        };

        // Twice the recorded duration when that lands between the bounds.
        assert_eq!(single(Some(1800.0)), Duration::from_secs(3600));
        // Short recorded builds clamp up to the floor…
        assert_eq!(single(Some(4.2)), floor);
        // …massive ones clamp down to the cap…
        assert_eq!(single(Some(100_000.0)), cap);
        // …and a record without a duration falls back to the floor,
        assert_eq!(single(None), floor);
        // as does a request with no matching record at all.
        let schedule = build_schedule(
            &[request(session, 0.0, DRV_A)],
            &no_timing,
            1.0,
            None,
            false,
        );
        assert_eq!(
            build_timeout_for(&schedule[0], &no_timing, floor, cap),
            floor
        );

        // Several targets: the slowest recorded duration wins.
        let two_targets = RecordedRequest {
            session,
            offset_s: 0.0,
            targets: vec![
                RecordedTarget {
                    drv: DRV_A.to_string(),
                    outputs: vec!["out".to_string()],
                },
                RecordedTarget {
                    drv: DRV_B.to_string(),
                    outputs: vec!["out".to_string()],
                },
            ],
        };
        let schedule = build_schedule(&[two_targets], &no_timing, 1.0, None, false);
        let mut map = HashMap::new();
        map.insert(
            (session, DRV_A.to_string()),
            RecordedTiming {
                duration_s: Some(900.0),
                stop_offset_s: None,
                interrupted: false,
                expected_built: false,
            },
        );
        map.insert(
            (session, DRV_B.to_string()),
            RecordedTiming {
                duration_s: Some(2000.0),
                stop_offset_s: None,
                interrupted: false,
                expected_built: false,
            },
        );
        let timing = timing_in(&map);
        assert_eq!(
            build_timeout_for(&schedule[0], &timing, floor, cap),
            Duration::from_secs(4000)
        );
    }

    #[test]
    fn lateness_summary_percentiles() {
        // Empty input summarizes to zeros; a single sample is its own max
        // and percentiles.
        assert_eq!(lateness_summary(&[]), (0, 0, 0));
        assert_eq!(lateness_summary(&[42]), (42, 42, 42));

        // Unsorted input is sorted before ranking: nearest-rank p50 of five
        // samples is the 3rd smallest, p95 the 5th.
        assert_eq!(lateness_summary(&[50, 10, 30, 20, 40]), (50, 30, 50));

        // A 100..2000 ladder of twenty samples: p50 is the 10th value, p95
        // the 19th.
        let ladder: Vec<u64> = (1..=20).map(|step| step * 100).collect();
        assert_eq!(lateness_summary(&ladder), (2000, 1000, 1900));
    }

    #[test]
    fn re_anchor_preserves_relative_spacing() {
        // Request 0 is already terminal: it keeps its recorded slot. The
        // earliest pending request fires at the resume offset and the later
        // one keeps its recorded 150s gap behind it.
        let mut schedule = vec![scheduled(0, 100), scheduled(1, 200), scheduled(2, 350)];
        let terminal: HashSet<usize> = [0].into_iter().collect();
        re_anchor_pending(&mut schedule, &terminal, Duration::from_secs(7));
        assert_eq!(schedule[0].due, Duration::from_secs(100));
        assert_eq!(schedule[1].due, Duration::from_secs(7));
        assert_eq!(schedule[2].due, Duration::from_secs(157));

        // A late resume (past the earliest pending due) shifts pending
        // requests later, never earlier than the resume offset.
        let mut schedule = vec![scheduled(0, 5), scheduled(1, 10)];
        re_anchor_pending(&mut schedule, &HashSet::new(), Duration::from_secs(7));
        assert_eq!(schedule[0].due, Duration::from_secs(7));
        assert_eq!(schedule[1].due, Duration::from_secs(12));

        // Nothing pending: nothing moves.
        let mut schedule = vec![scheduled(0, 100)];
        let terminal: HashSet<usize> = [0].into_iter().collect();
        re_anchor_pending(&mut schedule, &terminal, Duration::from_secs(7));
        assert_eq!(schedule[0].due, Duration::from_secs(100));
    }

    // ── Dispatcher tests ────────────────────────────────────────────────

    /// Submitter wrapper for dispatcher tests: scripted outcomes come from
    /// the wrapped [`FakeSubmitter`]; every call additionally records the
    /// paused-clock elapsed time it started at and is stretched by `delay`
    /// so admission ordering under `max_sessions` becomes observable.
    struct InstrumentedSubmitter {
        inner: FakeSubmitter,
        delay: Duration,
        started: tokio::time::Instant,
        /// `(elapsed at call, root drvs, timeout)` per submission, in call
        /// order.
        calls: std::sync::Mutex<Vec<(Duration, Vec<String>, Duration)>>,
    }

    impl InstrumentedSubmitter {
        fn new(inner: FakeSubmitter, delay: Duration) -> Self {
            Self {
                inner,
                delay,
                started: tokio::time::Instant::now(),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Submitter for InstrumentedSubmitter {
        async fn submit_batch(
            &self,
            store_url: &str,
            batch: &Batch,
            timeout: Duration,
        ) -> anyhow::Result<BatchOutcome> {
            self.calls.lock().unwrap().push((
                self.started.elapsed(),
                batch.root_drvs.clone(),
                timeout,
            ));
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.inner.submit_batch(store_url, batch, timeout).await
        }
    }

    /// AdminApi stub for the collect assertions: no poison evidence, no
    /// logs — the buckets under test are decided by the interruption flag
    /// and the in-band results alone.
    struct NoEvidenceAdmin;

    #[async_trait::async_trait]
    impl AdminApi for NoEvidenceAdmin {
        async fn get_build_graph(&self, _build_id: &str) -> anyhow::Result<GraphSnapshot> {
            Ok(GraphSnapshot::default())
        }
        async fn list_poisoned(&self) -> anyhow::Result<Vec<PoisonedView>> {
            Ok(Vec::new())
        }
        async fn log_tail(
            &self,
            _drv: &str,
            _exec: Option<&str>,
            _max: usize,
        ) -> anyhow::Result<Vec<u8>> {
            Ok(Vec::new())
        }
        async fn list_builds(
            &self,
            _tenant: &str,
            _limit: u32,
        ) -> anyhow::Result<Vec<(String, Option<String>)>> {
            Ok(Vec::new())
        }
    }

    /// Owned `(session, drv)` → timing lookup; the dispatcher needs a
    /// `'static`, shareable closure (unlike [`timing_in`], which borrows).
    fn timing_arc(map: HashMap<(i64, String), RecordedTiming>) -> SharedTimingLookup {
        Arc::new(move |session, drv| map.get(&(session, drv.to_string())).cloned())
    }

    /// In-band per-root result with the given status.
    fn po(drv: &str, status: BuildStatus, error: &str) -> PathOutcome {
        PathOutcome {
            drv_path: drv.to_string(),
            status: build_status_name(status).to_string(),
            error_msg: error.to_string(),
            start_time: 0,
            stop_time: 0,
        }
    }

    /// Job context for the collect assertions (output path derived from the
    /// drv name, no plan-time exclusions).
    fn job_ctx(job: &str, drv: &str) -> JobContext {
        JobContext {
            job: job.to_string(),
            system: "x86_64-linux".into(),
            drv_path: drv.to_string(),
            outputs: BTreeMap::from([(
                "out".to_string(),
                format!("{}-out", drv.trim_end_matches(".drv")),
            )]),
            dep_drvs: HashSet::new(),
            hydra_outcome: HydraOutcome::Built,
            hydra_outputs: BTreeMap::new(),
            hydra_buildstatus: None,
            plan_not_attemptable: false,
            plan_snapshot_valid: false,
        }
    }

    /// Minimal terminal results.jsonl record (match-built) for resume tests.
    fn terminal_job_record(job: &str, drv: &str) -> JobRecord {
        JobRecord {
            job: job.into(),
            system: "x86_64-linux".into(),
            drv_path: drv.into(),
            mode: "leaf".into(),
            attempts: 1,
            build_ids: Vec::new(),
            rio: RioSide::default(),
            hydra: HydraSide::default(),
            nar_compare: BTreeMap::new(),
            bucket: Bucket::MatchBuilt.as_str().into(),
            cascaded: false,
            signature: None,
            log_key: None,
            repro: String::new(),
            evidence: None,
            updated_at: now_rfc3339(),
        }
    }

    /// Dispatch entry as a prior run would have written it for `index`.
    fn prior_dispatch_entry(index: usize, session: i64, drv: &str) -> DispatchEntry {
        DispatchEntry {
            request_index: index,
            session,
            due_offset_s: 0.0,
            dispatched_at: now_rfc3339(),
            dispatch_lateness_ms: 0,
            deadline_secs: 1800,
            interruption_armed: false,
            interruption_fired: false,
            attempts: 1,
            batch_ids: vec![1],
            drvs: vec![drv.to_string()],
        }
    }

    /// Run the dispatcher with test defaults: no top-up, no deadline, a
    /// fresh tracker, and batch ids starting at 1.
    async fn drive_dispatch(
        state: &Arc<StateDir>,
        submitter: Arc<dyn Submitter>,
        schedule: Vec<ScheduledRequest>,
        job_of_drv: BTreeMap<String, String>,
        timing: SharedTimingLookup,
        config: TimelineConfig,
        terminal: HashSet<String>,
    ) -> TimedRunStats {
        run_timed_dispatch(
            state.clone(),
            submitter,
            Arc::new(SubmitTracker::default()),
            schedule,
            Arc::new(job_of_drv),
            timing,
            "ssh-ng://test".into(),
            config,
            Knobs::default(),
            Arc::new(AtomicU64::new(1)),
            || false,
            None,
            Arc::new(tokio::sync::Mutex::new(terminal)),
        )
        .await
        .unwrap()
    }

    /// Run the production collect classification over one settled timed
    /// batch record (the same path the campaign collect loop drives), so the
    /// tests assert the bucket a real campaign would record.
    async fn collect_settled_batch(
        state: &StateDir,
        record: &BatchRecord,
        contexts: &HashMap<String, JobContext>,
    ) {
        let view = BatchView {
            kind: record.kind.clone(),
            build_id: record.build_id.clone(),
            results: record.results.clone(),
            reasons: record.reasons.clone(),
            engine_cancelled: record.engine_cancelled,
            interruption_drvs: record.interruption_drvs.clone(),
            submitted_at: Some(record.started_at.clone()),
        };
        let requeue = process_settled_batch(
            state,
            &NoEvidenceAdmin,
            &FakeStoreApi::default(),
            None,
            contexts,
            &record.jobs,
            &view,
            &HashMap::new(),
            &Knobs::default(),
            "leaf",
            "ssh-ng://test",
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert!(
            requeue.is_empty(),
            "timed batch members are never re-offered"
        );
    }

    /// Requests fire at their recorded offsets divided by the speedup, FIFO
    /// admission under `max_sessions = 1` serializes them in schedule order,
    /// and every request settles into one dispatch.jsonl entry whose
    /// lateness reflects the wait for an admission permit.
    #[tokio::test(start_paused = true)]
    async fn dispatcher_fires_requests_at_offset_over_speedup_and_respects_max_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateDir::new(dir.path()).unwrap());
        // Recorded offsets 0/10/20s at speedup 2.0 → due at 0/5/10s.
        let requests = vec![
            request(1, 0.0, DRV_A),
            request(2, 10.0, DRV_B),
            request(3, 20.0, DRV_C),
        ];
        let schedule = build_schedule(&requests, &no_timing, 2.0, None, true);
        // Each scripted submission succeeds after 7s of paused-clock time,
        // so with one admission permit the second request (due 5s) is
        // admitted at 7s and the third (due 10s) at 14s.
        let fake = FakeSubmitter::default();
        for _ in 0..3 {
            fake.outcomes
                .lock()
                .unwrap()
                .push(Ok(BatchOutcome::default()));
        }
        let submitter = Arc::new(InstrumentedSubmitter::new(fake, Duration::from_secs(7)));
        let config = TimelineConfig {
            max_sessions: 1,
            speedup: 2.0,
            ..TimelineConfig::from_knobs(&Knobs::default())
        };
        let stats = drive_dispatch(
            &state,
            submitter.clone(),
            schedule,
            BTreeMap::from([
                (DRV_A.to_string(), "a".to_string()),
                (DRV_B.to_string(), "b".to_string()),
                (DRV_C.to_string(), "c".to_string()),
            ]),
            timing_arc(HashMap::new()),
            config,
            HashSet::new(),
        )
        .await;

        // Submissions happen in schedule order; the second never starts
        // before its 5s due time.
        let calls = submitter.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 3);
        let roots: Vec<&str> = calls.iter().map(|(_, drvs, _)| drvs[0].as_str()).collect();
        assert_eq!(roots, vec![DRV_A, DRV_B, DRV_C]);
        assert!(
            calls[1].0 >= Duration::from_secs(5),
            "second submission started at {:?}, before its due time",
            calls[1].0
        );

        // dispatch.jsonl: one entry per request, settle order = admission
        // order, dispatch timestamps monotonic, and the serialized
        // admissions surface as growing lateness (the third entry is late).
        let entries: Vec<DispatchEntry> = state.load_jsonl(StateFile::Dispatch).unwrap();
        assert_eq!(entries.len(), 3);
        let order: Vec<usize> = entries.iter().map(|e| e.request_index).collect();
        assert_eq!(order, vec![0, 1, 2]);
        let stamps: Vec<jiff::Timestamp> = entries
            .iter()
            .map(|e| e.dispatched_at.parse().unwrap())
            .collect();
        assert!(
            stamps.windows(2).all(|pair| pair[0] <= pair[1]),
            "{stamps:?}"
        );
        assert_eq!(entries[0].dispatch_lateness_ms, 0);
        assert!(entries[1].dispatch_lateness_ms >= 2_000, "{entries:?}");
        assert!(entries[2].dispatch_lateness_ms >= 4_000, "{entries:?}");
        assert!(entries[2].dispatch_lateness_ms > 0);
        assert!(
            entries
                .iter()
                .all(|e| e.attempts == 1 && e.batch_ids.len() == 1)
        );

        assert_eq!(stats.requests_total, 3);
        assert_eq!(stats.dispatched, 3);
        assert_eq!(
            stats.max_dispatch_lateness_ms,
            entries[2].dispatch_lateness_ms
        );
        assert_eq!(stats.resume_count, 0);
        assert!(!stats.timing_degraded);
    }

    /// A timed submission that fails engine-side (no in-band results, no
    /// build id, not an engine cancellation) is counted in the run
    /// statistics and is never confirmation-retried — there is no in-band
    /// failure to re-confirm, so its members are simply left to the
    /// end-of-run not-attempted backfill.
    #[tokio::test(start_paused = true)]
    async fn engine_side_submission_failure_is_counted_and_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateDir::new(dir.path()).unwrap());
        let requests = vec![request(1, 0.0, DRV_A)];
        // The recorded expectation is a successful build, so a reproduced
        // in-band failure WOULD trigger confirmation retries; an engine-side
        // submission failure must not.
        let mut map = HashMap::new();
        map.insert(
            (1i64, DRV_A.to_string()),
            RecordedTiming {
                duration_s: Some(10.0),
                stop_offset_s: None,
                interrupted: false,
                expected_built: true,
            },
        );
        let schedule = build_schedule(&requests, &timing_in(&map), 1.0, None, true);
        let fake = FakeSubmitter::default();
        fake.outcomes
            .lock()
            .unwrap()
            .push(Err(anyhow::anyhow!("channel open refused")));
        let submitter = Arc::new(InstrumentedSubmitter::new(fake, Duration::ZERO));
        let stats = drive_dispatch(
            &state,
            submitter.clone(),
            schedule,
            BTreeMap::from([(DRV_A.to_string(), "a".to_string())]),
            timing_arc(map),
            TimelineConfig::from_knobs(&Knobs::default()),
            HashSet::new(),
        )
        .await;

        assert_eq!(stats.dispatched, 1);
        assert_eq!(stats.submission_failures, 1);
        // Exactly one submission: the engine-side failure is not retried.
        assert_eq!(submitter.calls.lock().unwrap().len(), 1);
        let entries: Vec<DispatchEntry> = state.load_jsonl(StateFile::Dispatch).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].attempts, 1);
        assert_eq!(entries[0].batch_ids.len(), 1);
    }

    /// A request whose only target was recorded as interrupted is submitted
    /// under the recorded disconnect deadline (not the build deadline); the
    /// engine cancellation is recorded on a timed batch carrying the armed
    /// drv, and the production collect classification buckets the unit
    /// interruption-replayed.
    #[tokio::test(start_paused = true)]
    async fn interruption_replay_abandons_at_recorded_offset_and_records_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateDir::new(dir.path()).unwrap());
        // Recorded request at offset 0 whose target stopped (disconnected)
        // at offset 2.0 → a 2s disconnect deadline at speedup 1.0.
        let requests = vec![request(7, 0.0, DRV_A)];
        let mut map = HashMap::new();
        map.insert(
            (7_i64, DRV_A.to_string()),
            RecordedTiming {
                duration_s: None,
                stop_offset_s: Some(2.0),
                interrupted: true,
                expected_built: false,
            },
        );
        let schedule = build_schedule(&requests, &timing_in(&map), 1.0, None, true);
        assert_eq!(schedule[0].disconnect_after, Some(Duration::from_secs(2)));

        // The scripted outcome is an engine cancellation: the abandon
        // deadline won the race against the build.
        let fake = FakeSubmitter::default();
        fake.outcomes.lock().unwrap().push(Ok(BatchOutcome {
            engine_cancelled: true,
            ..BatchOutcome::default()
        }));
        let submitter = Arc::new(InstrumentedSubmitter::new(fake, Duration::ZERO));
        let stats = drive_dispatch(
            &state,
            submitter.clone(),
            schedule,
            BTreeMap::from([(DRV_A.to_string(), "a".to_string())]),
            timing_arc(map),
            TimelineConfig::from_knobs(&Knobs::default()),
            HashSet::new(),
        )
        .await;

        // The submitter saw the 2s disconnect deadline, not the 30-minute
        // build-deadline floor, and an armed request never
        // confirmation-retries.
        let calls = submitter.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].2, Duration::from_secs(2));

        // The settled batch is a timed batch carrying the armed drv.
        let batches: Vec<BatchRecord> = state.load_jsonl(StateFile::Batches).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].kind, BATCH_KIND_TIMED);
        assert_eq!(batches[0].interruption_drvs, vec![DRV_A.to_string()]);
        assert!(batches[0].engine_cancelled);

        let entries: Vec<DispatchEntry> = state.load_jsonl(StateFile::Dispatch).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].interruption_armed);
        assert!(entries[0].interruption_fired);
        assert_eq!(entries[0].attempts, 1);
        assert_eq!(stats.interruptions_replayed, 1);
        assert_eq!(stats.interruptions_not_reproduced, 0);

        // The production collect path over that batch yields the
        // interruption-replayed bucket for the unit.
        let contexts = HashMap::from([("a".to_string(), job_ctx("a", DRV_A))]);
        collect_settled_batch(&state, &batches[0], &contexts).await;
        let records = latest_per_job(state.load_jsonl(StateFile::Results).unwrap());
        assert_eq!(records["a"].bucket, Bucket::InterruptionReplayed.as_str());
    }

    /// The same armed request whose build settles in band before the
    /// disconnect deadline classifies interruption-not-reproduced. The
    /// scripted result uses a Substituted status to pin that ANY in-band
    /// success counts, not just Built.
    #[tokio::test(start_paused = true)]
    async fn interruption_not_reproduced_when_build_finishes_first() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateDir::new(dir.path()).unwrap());
        let requests = vec![request(7, 0.0, DRV_A)];
        let mut map = HashMap::new();
        map.insert(
            (7_i64, DRV_A.to_string()),
            RecordedTiming {
                duration_s: None,
                stop_offset_s: Some(2.0),
                interrupted: true,
                expected_built: false,
            },
        );
        let schedule = build_schedule(&requests, &timing_in(&map), 1.0, None, true);

        // The scripted outcome completes in band (no cancellation) with a
        // non-Built success status.
        let fake = FakeSubmitter::default();
        fake.outcomes.lock().unwrap().push(Ok(BatchOutcome {
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a".into()),
            results: vec![po(DRV_A, BuildStatus::Substituted, "")],
            ..BatchOutcome::default()
        }));
        let submitter = Arc::new(InstrumentedSubmitter::new(fake, Duration::ZERO));
        let stats = drive_dispatch(
            &state,
            submitter.clone(),
            schedule,
            BTreeMap::from([(DRV_A.to_string(), "a".to_string())]),
            timing_arc(map),
            TimelineConfig::from_knobs(&Knobs::default()),
            HashSet::new(),
        )
        .await;

        assert_eq!(submitter.calls.lock().unwrap().len(), 1);
        let batches: Vec<BatchRecord> = state.load_jsonl(StateFile::Batches).unwrap();
        assert_eq!(batches.len(), 1);
        assert!(!batches[0].engine_cancelled);
        let entries: Vec<DispatchEntry> = state.load_jsonl(StateFile::Dispatch).unwrap();
        assert!(entries[0].interruption_armed);
        assert!(!entries[0].interruption_fired);
        assert_eq!(stats.interruptions_replayed, 0);
        assert_eq!(stats.interruptions_not_reproduced, 1);

        let contexts = HashMap::from([("a".to_string(), job_ctx("a", DRV_A))]);
        collect_settled_batch(&state, &batches[0], &contexts).await;
        let records = latest_per_job(state.load_jsonl(StateFile::Results).unwrap());
        assert_eq!(
            records["a"].bucket,
            Bucket::InterruptionNotReproduced.as_str()
        );
    }

    /// A unit expected to build whose replayed result is a failure is
    /// re-confirmed alone: the retry batch carries only the failing
    /// position, and the request stops once it succeeds, well inside the
    /// confirmation budget.
    #[tokio::test(start_paused = true)]
    async fn confirmation_retries_resubmit_only_failing_positions() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateDir::new(dir.path()).unwrap());
        // One recorded request carrying two targets, both expected built.
        let two_targets = RecordedRequest {
            session: 5,
            offset_s: 0.0,
            targets: vec![
                RecordedTarget {
                    drv: DRV_A.to_string(),
                    outputs: vec!["out".to_string()],
                },
                RecordedTarget {
                    drv: DRV_B.to_string(),
                    outputs: vec!["out".to_string()],
                },
            ],
        };
        let mut map = HashMap::new();
        for drv in [DRV_A, DRV_B] {
            map.insert(
                (5_i64, drv.to_string()),
                RecordedTiming {
                    duration_s: Some(60.0),
                    stop_offset_s: None,
                    interrupted: false,
                    expected_built: true,
                },
            );
        }
        let schedule = build_schedule(&[two_targets], &timing_in(&map), 1.0, None, true);

        // Scripted from the back: the initial submission fails only B, the
        // confirmation retry then builds it.
        let fake = FakeSubmitter::default();
        fake.outcomes.lock().unwrap().push(Ok(BatchOutcome {
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-000000000002".into()),
            results: vec![po(DRV_B, BuildStatus::Built, "")],
            ..BatchOutcome::default()
        }));
        fake.outcomes.lock().unwrap().push(Ok(BatchOutcome {
            build_id: Some("0193e4a2-7c1b-7d20-9b3a-000000000001".into()),
            results: vec![
                po(DRV_A, BuildStatus::Built, ""),
                po(DRV_B, BuildStatus::PermanentFailure, "builder failed"),
            ],
            ..BatchOutcome::default()
        }));
        let submitter = Arc::new(InstrumentedSubmitter::new(fake, Duration::ZERO));
        let config = TimelineConfig {
            confirm_attempts: 3,
            ..TimelineConfig::from_knobs(&Knobs::default())
        };
        let stats = drive_dispatch(
            &state,
            submitter.clone(),
            schedule,
            BTreeMap::from([
                (DRV_A.to_string(), "a".to_string()),
                (DRV_B.to_string(), "b".to_string()),
            ]),
            timing_arc(map),
            config,
            HashSet::new(),
        )
        .await;

        // Exactly two submissions: the full request, then only the failing
        // position.
        let calls = submitter.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec![DRV_A.to_string(), DRV_B.to_string()]);
        assert_eq!(calls[1].1, vec![DRV_B.to_string()]);

        let batches: Vec<BatchRecord> = state.load_jsonl(StateFile::Batches).unwrap();
        assert_eq!(batches.len(), 2);
        assert!(batches.iter().all(|b| b.kind == BATCH_KIND_TIMED));
        assert!(batches[1].interruption_drvs.is_empty());
        assert_eq!(batches[1].root_drvs, vec![DRV_B.to_string()]);

        let entries: Vec<DispatchEntry> = state.load_jsonl(StateFile::Dispatch).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].attempts, 2);
        assert_eq!(entries[0].batch_ids.len(), 2);
        assert_eq!(stats.dispatched, 1);
        assert_eq!(stats.requests_total, 1);
    }

    /// Resuming a timed run skips requests that are already terminal and
    /// were dispatched before, re-anchors the rest so the first pending
    /// request fires immediately, and flags the run's timing as degraded.
    #[tokio::test(start_paused = true)]
    async fn timed_resume_re_anchors_and_flags_degraded() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateDir::new(dir.path()).unwrap());
        // Request 0 already settled in the prior run; request 1 was
        // recorded 300s later.
        let requests = vec![request(1, 0.0, DRV_A), request(2, 300.0, DRV_B)];
        let schedule = build_schedule(&requests, &no_timing, 1.0, None, true);
        state
            .append_jsonl(StateFile::Results, &terminal_job_record("a", DRV_A))
            .unwrap();
        state
            .append_jsonl(StateFile::Dispatch, &prior_dispatch_entry(0, 1, DRV_A))
            .unwrap();

        let fake = FakeSubmitter::default();
        fake.outcomes
            .lock()
            .unwrap()
            .push(Ok(BatchOutcome::default()));
        let submitter = Arc::new(InstrumentedSubmitter::new(fake, Duration::ZERO));
        let stats = drive_dispatch(
            &state,
            submitter.clone(),
            schedule,
            BTreeMap::from([
                (DRV_A.to_string(), "a".to_string()),
                (DRV_B.to_string(), "b".to_string()),
            ]),
            timing_arc(HashMap::new()),
            TimelineConfig::from_knobs(&Knobs::default()),
            ["a".to_string()].into_iter().collect(),
        )
        .await;

        // Only the pending request is submitted, and it fires immediately
        // instead of waiting out its recorded 300s offset.
        let calls = submitter.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, vec![DRV_B.to_string()]);
        assert!(
            calls[0].0 < Duration::from_secs(5),
            "re-anchored request waited {:?}",
            calls[0].0
        );

        assert_eq!(stats.requests_total, 2);
        assert_eq!(stats.dispatched, 1);
        assert_eq!(stats.resume_count, 1);
        assert!(stats.timing_degraded);

        // The resumed run appends a fresh dispatch entry for the request it
        // fired, with its (re-anchored) lateness recorded.
        let entries: Vec<DispatchEntry> = state.load_jsonl(StateFile::Dispatch).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].request_index, 1);
        assert_eq!(entries[1].attempts, 1);
    }
}
