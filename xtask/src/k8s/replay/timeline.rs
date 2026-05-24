//! Timeline engine for `xtask k8s replay` — request pacing at the recorded
//! offsets and per-request execution against the target.
//!
//! [`build_schedule`] turns the archive's recorded requests into a paced
//! [`ScheduledRequest`] list (offset-sorted, speedup-scaled, optionally
//! limited). [`run_timeline`] dispatches each entry at its due time under an
//! admission semaphore and runs it through the per-request pipeline: closure
//! walk → target validity probe → supply-gap upload (deduplicated across
//! concurrent requests via [`UploadClaims`]) → `BuildPathsWithResults` →
//! output-hash collection. Recorded client disconnects are replayed by
//! dropping the channel mid-build, and recorded-success builds that fail are
//! re-attempted a bounded number of times before the failure stands.
//!
//! [`InFlightTracker`] exposes live per-request stage data for the heartbeat
//! line; every request ends as a [`RequestOutcome`] for the comparison and
//! report phases.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use rio_nix::narinfo::NarInfo;
use rio_nix::protocol::build::{BuildResult, BuildStatus};
use rio_nix::protocol::client::{KeyedBuildResult, NarPayload, StoreEntry};
use tokio::sync::{RwLock, Semaphore};
use tokio::task::JoinSet;

use super::archive::{BuildRecord, ReplayArchive, ReplayRequest, prod_status};
use super::client::{DaemonChannel, GatewayPool, ReplayClientError};
use super::prewarm::SupplyContext;
use super::substituter::Substituter;
use super::supply::{
    ClaimOutcome, Closure, ClosureNode, PathSource, UploadClaims, UploadItem, UploadPayload,
    UploadPlan, plan_uploads, resolve_source, walk_closure,
};

/// Paths per `QueryValidPaths` chunk in the per-request validity probe.
const PROBE_CHUNK: usize = 2000;

/// Entry budget for one per-request `AddMultipleToStore` batch.
const BATCH_MAX_ENTRIES: usize = 500;

/// Byte budget (sum of NAR sizes) for one per-request batch upload.
const BATCH_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Deadline for individually streamed (large) gap uploads — multi-GB relayed
/// NARs legitimately need longer than the generic op deadline. Mirrors the
/// prewarm phase's large-upload budget.
const LARGE_UPLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Pause before the single retry of a failed daemon-channel open.
const CHANNEL_RETRY_DELAY: Duration = Duration::from_secs(5);

/// Emit a timeline progress line every this many completed requests.
const PROGRESS_EVERY_REQUESTS: usize = 25;

/// Disconnect delay assumed when a recorded disconnect carries no
/// `stop_offset_s` (scaled by the speedup like a recorded one).
const DEFAULT_DISCONNECT_DELAY_S: f64 = 60.0;

/// Lower bound on the disconnect delay still remaining when the build is
/// submitted, so the build is always actually submitted before the channel is
/// dropped (a high speedup or a slow supply phase cannot turn the replay into
/// a no-op). Also floors the scheduled delay itself in [`build_schedule`].
const DISCONNECT_FLOOR: Duration = Duration::from_secs(1);

/// One recorded request, scheduled for replay.
#[derive(Debug, Clone)]
pub struct ScheduledRequest {
    /// Unique per-run id (index into the schedule) — the tracker key.
    pub index: usize,
    /// The recorded request being replayed.
    pub request: ReplayRequest,
    /// When to dispatch, relative to the run start: `offset_s / speedup`.
    pub due: Duration,
    /// When the recorded outcome was a client disconnect and disconnect
    /// replay is enabled: how long after dispatch to drop the channel
    /// (= `(stop_offset_s - offset_s).max(0) / speedup`, floor 1s); `None`
    /// otherwise.
    pub disconnect_after: Option<Duration>,
}

/// Sort the recorded requests by offset, apply `--limit`, and compute each
/// request's due time and (optionally) disconnect timer.
pub fn build_schedule(
    requests: &[ReplayRequest],
    builds: &HashMap<(i64, String), BuildRecord>,
    speedup: f64,
    limit: Option<usize>,
    disconnect_replay: bool,
) -> Vec<ScheduledRequest> {
    let mut sorted: Vec<ReplayRequest> = requests.to_vec();
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
            let disconnect_after = if disconnect_replay {
                disconnect_after_for(&request, builds, speedup)
            } else {
                None
            };
            ScheduledRequest {
                index,
                request,
                due,
                disconnect_after,
            }
        })
        .collect()
}

/// Disconnect timer for one request: present only when one of its build
/// records is a recorded client disconnect. The delay is the recorded
/// dispatch-to-stop gap scaled by the speedup ([`DEFAULT_DISCONNECT_DELAY_S`]
/// when the record carries no stop offset), never below
/// [`DISCONNECT_FLOOR`].
fn disconnect_after_for(
    request: &ReplayRequest,
    builds: &HashMap<(i64, String), BuildRecord>,
    speedup: f64,
) -> Option<Duration> {
    let offset = request.offset_s.max(0.0);
    let record = request.paths.iter().find_map(|(drv_path, _outputs)| {
        builds
            .get(&(request.ssh_session_id, drv_path.clone()))
            .filter(|record| record.status == prod_status::CLIENT_DISCONNECT)
    })?;
    let scaled = match record.stop_offset_s {
        Some(stop) => (stop - offset).max(0.0) / speedup,
        None => DEFAULT_DISCONNECT_DELAY_S / speedup,
    };
    Some(Duration::from_secs_f64(scaled).max(DISCONNECT_FLOOR))
}

/// Pipeline stage a replayed request is currently in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestStage {
    /// Due (its recorded offset has passed) and waiting for an admission
    /// slot. Requests still sleeping toward their offset are not tracked.
    Waiting,
    /// Walking the request's archive-side closure.
    Closure,
    /// Probing which closure paths the target already has.
    Probe,
    /// Uploading the request's supply gaps.
    Upload,
    /// Waiting on `BuildPathsWithResults`.
    Build,
    /// Collecting replay-side output hashes.
    Collect,
    /// Finished; set momentarily before the entry is removed.
    Done,
}

/// Live progress data for the heartbeat: stage and stage-entry time per
/// in-flight request.
#[derive(Debug, Default)]
pub struct InFlightTracker {
    /// Request index → (stage, when that stage was entered, session id).
    entries: Mutex<HashMap<usize, (RequestStage, Instant, i64)>>,
}

impl InFlightTracker {
    /// New, empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that request `index` (recorded session `session`) entered
    /// `stage` now.
    pub fn set(&self, index: usize, session: i64, stage: RequestStage) {
        self.lock().insert(index, (stage, Instant::now(), session));
    }

    /// Drop request `index` from the tracker (it finished or was abandoned).
    pub fn remove(&self, index: usize) {
        self.lock().remove(&index);
    }

    /// In-flight count plus the entry that has been in its current stage the
    /// longest: `(index, session, stage, time in stage)` — the heartbeat
    /// line's "oldest" column. Entries that are actually executing are
    /// preferred over [`RequestStage::Waiting`] ones, so the column points at
    /// genuinely stuck work; a Waiting entry is reported only when nothing is
    /// past admission.
    pub fn snapshot(&self) -> (usize, Option<(usize, i64, RequestStage, Duration)>) {
        let entries = self.lock();
        let oldest = entries
            .iter()
            .min_by_key(|(_, (stage, since, _))| (*stage == RequestStage::Waiting, *since))
            .map(|(&index, &(stage, since, session))| (index, session, stage, since.elapsed()));
        (entries.len(), oldest)
    }

    /// Lock the entry map. Mutations are single inserts/removes, so a panic
    /// mid-update cannot corrupt it; recover from poisoning to keep the
    /// heartbeat alive.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<usize, (RequestStage, Instant, i64)>> {
        self.entries.lock().unwrap_or_else(|err| err.into_inner())
    }
}

/// Per derived path (one drv + requested outputs) replay-side result.
#[derive(Debug, Clone)]
pub struct DerivedOutcome {
    /// Derivation store path as requested.
    pub drv_path: String,
    /// Output names as requested (normalized: empty or `["*"]` stays as-is).
    pub outputs: Vec<String>,
    /// The daemon's `BuildResult` for this derived path, if a build ran.
    pub result: Option<BuildResult>,
    /// Output name → lowercase hex SHA-256 of the output's NAR, collected via
    /// `QueryPathInfo` after a successful build.
    pub replay_nar_hashes: BTreeMap<String, String>,
    /// Daemon refusal message if the upload for this request was rejected
    /// (after one retry) — the build was then not attempted.
    pub upload_rejected: Option<String>,
}

/// Coarse classification of a request-level failure — what stage of the
/// replay broke, separating infrastructure problems from build outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestErrorKind {
    /// No daemon channel could be opened (after the one retry).
    ChannelOpen,
    /// The target validity probe failed.
    Probe,
    /// The supply closure or upload plan could not be computed.
    UploadPlan,
    /// A gap upload failed at the transport level (not a daemon refusal).
    UploadTransport,
    /// The build exceeded its deadline.
    BuildTimeout,
    /// The daemon refused the build operation itself.
    BuildRefused,
    /// The build failed at the transport level.
    BuildTransport,
    /// The daemon answered with a different result count than submitted.
    ResultCountMismatch,
    /// Output-hash collection failed.
    Collect,
    /// The request task panicked.
    Panic,
}

/// A request-level failure: which stage broke plus the human-readable detail.
#[derive(Debug, Clone)]
pub struct RequestError {
    /// Coarse classification for the report.
    pub kind: RequestErrorKind,
    /// Human-readable detail (op name, daemon message, deadline, …).
    pub message: String,
}

impl RequestError {
    /// New error of `kind` carrying `message`.
    fn new(kind: RequestErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// Everything the replay learned about one scheduled request.
#[derive(Debug, Clone)]
pub struct RequestOutcome {
    /// Schedule index (matches [`ScheduledRequest::index`]).
    pub index: usize,
    /// The recorded request that was replayed.
    pub request: ReplayRequest,
    /// One entry per requested derived path, in request order.
    pub results: Vec<DerivedOutcome>,
    /// Build attempts made (1 = no confirmation retries needed; 0 = the build
    /// was never attempted — upload rejected, pre-build infra error, or
    /// panic).
    pub attempts: u32,
    /// True when the request was replayed as a recorded client disconnect.
    pub disconnected: bool,
    /// Transport/infra error that prevented the request from completing
    /// normally (not a build failure).
    pub error: Option<RequestError>,
    /// How late dispatch was vs the schedule (admission/backpressure), for
    /// the report.
    pub dispatch_lateness: Duration,
}

/// Tunables for the timeline engine.
#[derive(Debug, Clone)]
pub struct TimelineConfig {
    /// Maximum concurrently executing requests (admission semaphore permits).
    pub max_sessions: usize,
    /// Total build attempts for a recorded-success build that fails on
    /// replay before the failure is reported as a regression.
    pub confirm_regressions: u32,
    /// Deadline for probes, uploads, and path-info queries.
    pub op_timeout: Duration,
    /// Lower bound on the per-request build deadline.
    pub build_timeout_floor: Duration,
    /// Upper bound on the per-request build deadline.
    pub build_timeout_cap: Duration,
    /// How long to wait for another request's in-flight upload claim.
    pub claim_wait: Duration,
}

impl Default for TimelineConfig {
    fn default() -> Self {
        Self {
            max_sessions: 32,
            confirm_regressions: 3,
            op_timeout: Duration::from_secs(120),
            build_timeout_floor: Duration::from_secs(30 * 60),
            build_timeout_cap: Duration::from_secs(2 * 60 * 60),
            claim_wait: Duration::from_secs(10 * 60),
        }
    }
}

/// Run-scoped handles shared by every request task.
struct TimelineEnv {
    /// The opened replay archive (closure walks, recorded outcomes, payloads).
    archive: Arc<ReplayArchive>,
    /// SSH connection pool to the gateway.
    pool: Arc<GatewayPool>,
    /// Shared supply context (target validity, coverage, relay narinfos).
    ctx: Arc<RwLock<SupplyContext>>,
    /// Cross-request upload claims.
    claims: Arc<UploadClaims>,
    /// Recording substituters, for relay payloads of per-request gap uploads.
    src_substituters: Arc<Vec<Substituter>>,
    /// Live stage data for the heartbeat.
    tracker: Arc<InFlightTracker>,
    /// Tunables.
    cfg: TimelineConfig,
}

/// Replay every scheduled request at its due time and collect the outcomes.
///
/// Each request runs as its own task: it sleeps until `start + due`, takes an
/// admission permit (FIFO, `max_sessions` wide), and executes the
/// per-request pipeline. A failed or panicked request never aborts the run —
/// it becomes a [`RequestOutcome`] with `error` set. Outcomes are returned
/// sorted by schedule index.
// Exactly the run-scoped handles the replay orchestration owns; bundling
// them into an ad-hoc struct would only move this list into a constructor.
#[allow(clippy::too_many_arguments)]
pub async fn run_timeline(
    archive: Arc<ReplayArchive>,
    pool: Arc<GatewayPool>,
    ctx: Arc<RwLock<SupplyContext>>,
    claims: Arc<UploadClaims>,
    src_substituters: Arc<Vec<Substituter>>,
    schedule: Vec<ScheduledRequest>,
    tracker: Arc<InFlightTracker>,
    cfg: TimelineConfig,
) -> Result<Vec<RequestOutcome>> {
    let total = schedule.len();
    if total == 0 {
        return Ok(Vec::new());
    }

    let env = Arc::new(TimelineEnv {
        archive,
        pool,
        ctx,
        claims,
        src_substituters,
        tracker: Arc::clone(&tracker),
        cfg,
    });
    let admission = Arc::new(Semaphore::new(env.cfg.max_sessions.max(1)));
    let start = tokio::time::Instant::now();

    let mut tasks: JoinSet<RequestOutcome> = JoinSet::new();
    // Task id → (schedule index, request), so a panicked task can still be
    // accounted for in the outcomes.
    let mut spawned: HashMap<tokio::task::Id, (usize, ReplayRequest)> = HashMap::new();
    for scheduled in schedule {
        let env = Arc::clone(&env);
        let admission = Arc::clone(&admission);
        let meta = (scheduled.index, scheduled.request.clone());
        let handle = tasks.spawn(async move {
            let index = scheduled.index;
            let session = scheduled.request.ssh_session_id;
            let due_at = start + scheduled.due;
            tokio::time::sleep_until(due_at).await;
            // Tracked only once due: "Waiting" means due-but-not-admitted, so
            // the heartbeat's in-flight count reflects real backlog rather
            // than the whole remaining schedule.
            env.tracker.set(index, session, RequestStage::Waiting);
            // FIFO admission: requests that came due earlier get a session
            // slot first; lateness measures schedule slip from backpressure.
            let permit = Arc::clone(&admission)
                .acquire_owned()
                .await
                .expect("the admission semaphore is never closed");
            let dispatch_at = tokio::time::Instant::now();
            let dispatch_lateness = dispatch_at.saturating_duration_since(due_at);

            let outcome = execute_request(&env, &scheduled, dispatch_at, dispatch_lateness).await;
            drop(permit);
            env.tracker.set(index, session, RequestStage::Done);
            env.tracker.remove(index);
            outcome
        });
        spawned.insert(handle.id(), meta);
    }

    let mut outcomes: Vec<RequestOutcome> = Vec::with_capacity(total);
    while let Some(joined) = tasks.join_next_with_id().await {
        match joined {
            Ok((id, outcome)) => {
                spawned.remove(&id);
                outcomes.push(outcome);
            }
            Err(join_error) => {
                // A panicked request must not take down the run; account for
                // it as an errored outcome so the report still sees it.
                if let Some((index, request)) = spawned.remove(&join_error.id()) {
                    tracker.remove(index);
                    tracing::warn!(index, error = %join_error, "replay request task panicked");
                    let results = skeleton_results(&request);
                    outcomes.push(RequestOutcome {
                        index,
                        request,
                        results,
                        attempts: 0,
                        disconnected: false,
                        error: Some(RequestError::new(
                            RequestErrorKind::Panic,
                            format!("request task panicked: {join_error}"),
                        )),
                        dispatch_lateness: Duration::ZERO,
                    });
                } else {
                    tracing::warn!(error = %join_error, "replay request task panicked (unknown task id)");
                }
            }
        }
        let completed = outcomes.len();
        if completed.is_multiple_of(PROGRESS_EVERY_REQUESTS) || completed == total {
            tracing::info!(completed, total, "replay timeline progress");
        }
    }

    outcomes.sort_by_key(|outcome| outcome.index);
    Ok(outcomes)
}

/// One [`DerivedOutcome`] per requested `(drv, outputs)` pair, with nothing
/// filled in yet.
fn skeleton_results(request: &ReplayRequest) -> Vec<DerivedOutcome> {
    request
        .paths
        .iter()
        .map(|(drv_path, outputs)| DerivedOutcome {
            drv_path: drv_path.clone(),
            outputs: outputs.clone(),
            result: None,
            replay_nar_hashes: BTreeMap::new(),
            upload_rejected: None,
        })
        .collect()
}

/// Replay one scheduled request end to end: closure walk, validity probe,
/// supply-gap upload, build (with disconnect replay and confirmation
/// retries), and output-hash collection. Never returns an error — every
/// failure mode is folded into the [`RequestOutcome`].
///
/// `dispatch_at` is when the request was admitted; the disconnect-replay
/// deadline is anchored there.
async fn execute_request(
    env: &TimelineEnv,
    scheduled: &ScheduledRequest,
    dispatch_at: tokio::time::Instant,
    dispatch_lateness: Duration,
) -> RequestOutcome {
    let request = &scheduled.request;
    let index = scheduled.index;
    let session = request.ssh_session_id;
    let mut outcome = RequestOutcome {
        index,
        request: request.clone(),
        results: skeleton_results(request),
        attempts: 0,
        disconnected: false,
        error: None,
        dispatch_lateness,
    };

    // ---- Closure ----------------------------------------------------------
    env.tracker.set(index, session, RequestStage::Closure);
    let mut roots: Vec<String> = Vec::new();
    for (drv_path, _outputs) in &request.paths {
        if !roots.contains(drv_path) {
            roots.push(drv_path.clone());
        }
    }
    let closure = {
        let archive = Arc::clone(&env.archive);
        let roots = roots.clone();
        match tokio::task::spawn_blocking(move || walk_closure(&archive, &roots)).await {
            Ok(Ok(closure)) => closure,
            Ok(Err(err)) => {
                outcome.error = Some(RequestError::new(
                    RequestErrorKind::UploadPlan,
                    format!("closure walk failed: {err:#}"),
                ));
                return outcome;
            }
            Err(err) => {
                outcome.error = Some(RequestError::new(
                    RequestErrorKind::Panic,
                    format!("closure walk task panicked or was cancelled: {err}"),
                ));
                return outcome;
            }
        }
    };

    // ---- Probe ------------------------------------------------------------
    env.tracker.set(index, session, RequestStage::Probe);
    let snapshot = snapshot_supply_context(env, &closure).await;
    let mut valid = snapshot.valid.clone();

    let mut slot = ChannelSlot::new(Arc::clone(&env.pool));
    let to_probe: Vec<String> = closure
        .all_paths
        .iter()
        .filter(|path| !valid.contains(*path))
        .cloned()
        .collect();
    let mut probed_valid: BTreeSet<String> = BTreeSet::new();
    for chunk in to_probe.chunks(PROBE_CHUNK) {
        let channel = match slot.get().await {
            Ok(channel) => channel,
            Err(error) => {
                outcome.error = Some(error);
                return outcome;
            }
        };
        match channel.query_valid_paths(chunk, env.cfg.op_timeout).await {
            Ok(found) => probed_valid.extend(found),
            Err(err) => {
                outcome.error = Some(RequestError::new(
                    RequestErrorKind::Probe,
                    format!("target validity probe failed: {err}"),
                ));
                return outcome;
            }
        }
    }
    if !probed_valid.is_empty() {
        valid.extend(probed_valid.iter().cloned());
        // Brief write-lock; the network work above is already done.
        let mut ctx = env.ctx.write().await;
        ctx.target_valid.extend(probed_valid);
    }

    // ---- Resolve sources and claims ---------------------------------------
    let mut claim_guard = ClaimGuard::new(Arc::clone(&env.claims));
    let closure_drvs: BTreeSet<&str> = closure
        .topo
        .iter()
        .map(|node| node.drv_path.as_str())
        .collect();
    let mut sources: HashMap<String, PathSource> = HashMap::new();
    for path in &closure.all_paths {
        if closure_drvs.contains(path.as_str()) || valid.contains(path) {
            continue;
        }
        let source = resolve_source(
            path,
            &snapshot.workload_outputs,
            &snapshot.coverage,
            &env.archive,
            &snapshot.relay,
        );
        let source = match source {
            PathSource::Archive | PathSource::Relay { .. } => {
                match resolve_claim(&env.claims, path, env.cfg.claim_wait).await {
                    ClaimResolution::Ours => {
                        claim_guard.add(path);
                        source
                    }
                    ClaimResolution::Landed => {
                        valid.insert(path.clone());
                        continue;
                    }
                    ClaimResolution::Unavailable => {
                        // Another request holds the claim but it has not
                        // landed within the wait budget. Replaying without
                        // the path is the honest outcome — the build may then
                        // fail, and that failure is real.
                        PathSource::NotSupplied { workload: false }
                    }
                }
            }
            other => other,
        };
        sources.insert(path.clone(), source);
    }

    // ---- Plan and upload the gaps ------------------------------------------
    env.tracker.set(index, session, RequestStage::Upload);
    let plan = match plan_uploads(&closure, &sources, &valid, &env.archive) {
        Ok(plan) => plan,
        Err(err) => {
            outcome.error = Some(RequestError::new(
                RequestErrorKind::UploadPlan,
                format!("upload planning failed: {err:#}"),
            ));
            return outcome;
        }
    };
    if !plan.skipped.is_empty() {
        tracing::debug!(
            session,
            skipped = plan.skipped.len(),
            first = ?plan.skipped.first(),
            "per-request upload plan left paths unsupplied"
        );
    }
    // A claim won for a path the planner could not place would make other
    // requests wait on an upload that will never happen — release those now.
    claim_guard.release_except(
        plan.large
            .iter()
            .chain(plan.batch.iter())
            .map(|item| item.store_path.as_str()),
    );

    match upload_request_gaps(env, &mut slot, &plan, &mut claim_guard).await {
        GapUploadOutcome::Done => {}
        GapUploadOutcome::Rejected(message) => {
            tracing::debug!(session, %message, "upload rejected; skipping the build");
            for derived in &mut outcome.results {
                derived.upload_rejected = Some(message.clone());
            }
            return outcome;
        }
        GapUploadOutcome::Error(error) => {
            outcome.error = Some(error);
            return outcome;
        }
    }

    // ---- Build (disconnect replay + confirmation retries) -------------------
    env.tracker.set(index, session, RequestStage::Build);
    let derived: Vec<String> = request
        .paths
        .iter()
        .map(|(drv_path, outputs)| format_derived(drv_path, outputs))
        .collect();
    let build_timeout = build_timeout_for(env.archive.builds(), request, &env.cfg);
    // Disconnect replay is anchored at dispatch: the recorded gap between
    // request start and client disconnect elapses on the dispatch clock,
    // however long the supply work above took.
    let disconnect_deadline = scheduled.disconnect_after.map(|after| dispatch_at + after);

    let mut attempts: u32 = 1;
    match run_build_attempt(&mut slot, &derived, build_timeout, disconnect_deadline).await {
        BuildAttempt::Disconnected => {
            outcome.disconnected = true;
            outcome.attempts = attempts;
            tracing::debug!(session, "request replayed as a recorded client disconnect");
            return outcome;
        }
        BuildAttempt::Error(error) => {
            outcome.error = Some(error);
            outcome.attempts = attempts;
            return outcome;
        }
        BuildAttempt::Results(results) => {
            let all_positions: Vec<usize> = (0..derived.len()).collect();
            if let Err(error) = apply_build_results(&mut outcome.results, &all_positions, results) {
                outcome.error = Some(error);
                outcome.attempts = attempts;
                return outcome;
            }
        }
    }

    // Confirmation: a recorded-success build that failed on replay gets up to
    // `confirm_regressions` total attempts (fresh channel each time) before
    // the failure is allowed to stand as a regression.
    let max_attempts = env.cfg.confirm_regressions.max(1);
    loop {
        let targets = regression_positions(env.archive.builds(), request, &outcome.results);
        if targets.is_empty() || attempts >= max_attempts {
            break;
        }
        attempts += 1;
        slot.discard();
        env.tracker.set(index, session, RequestStage::Build);
        tracing::debug!(
            session,
            attempt = attempts,
            candidates = targets.len(),
            "re-running the build to confirm recorded-success regressions"
        );
        // Re-submit ONLY the regression candidates: the gateway applies one
        // DAG-level result to every derived path of a request, so re-sending
        // the full list would keep innocent recorded-success siblings
        // correlated with a still-failing drv; alone, the candidates can
        // build cleanly and be reclassified correctly.
        let retry_derived: Vec<String> = targets
            .iter()
            .map(|&position| derived[position].clone())
            .collect();
        match run_build_attempt(&mut slot, &retry_derived, build_timeout, None).await {
            // No disconnect deadline is passed for confirmation attempts, so
            // a Disconnected outcome cannot occur; treat it like one anyway
            // rather than panicking if that ever changes.
            BuildAttempt::Disconnected => {
                outcome.disconnected = true;
                break;
            }
            BuildAttempt::Error(error) => {
                outcome.error = Some(error);
                outcome.attempts = attempts;
                return outcome;
            }
            BuildAttempt::Results(results) => {
                // Only the resubmitted positions take the re-run's result;
                // everything else keeps its first outcome.
                if let Err(error) = apply_build_results(&mut outcome.results, &targets, results) {
                    outcome.error = Some(error);
                    outcome.attempts = attempts;
                    return outcome;
                }
            }
        }
    }
    outcome.attempts = attempts;

    // ---- Collect output hashes ----------------------------------------------
    env.tracker.set(index, session, RequestStage::Collect);
    if let Err(error) = collect_output_hashes(env, &mut slot, &closure, &mut outcome.results).await
    {
        outcome.error = Some(error);
        return outcome;
    }

    tracing::debug!(
        session,
        attempts = outcome.attempts,
        derived = outcome.results.len(),
        "request replay complete"
    );
    outcome
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

/// The slice of the shared [`SupplyContext`] relevant to one closure,
/// captured under a single brief read-lock so no lock is held across the
/// network work that follows.
struct SupplySnapshot {
    /// Closure paths already known valid on the target.
    valid: BTreeSet<String>,
    /// Closure paths covered by a target substituter.
    coverage: BTreeSet<String>,
    /// Closure paths relay-able from a recording substituter.
    relay: HashMap<String, (String, NarInfo)>,
    /// Closure paths that are workload outputs (never supplied).
    workload_outputs: BTreeSet<String>,
}

/// Snapshot the supply context's membership for this closure's paths.
async fn snapshot_supply_context(env: &TimelineEnv, closure: &Closure) -> SupplySnapshot {
    let ctx = env.ctx.read().await;
    let mut snapshot = SupplySnapshot {
        valid: BTreeSet::new(),
        coverage: BTreeSet::new(),
        relay: HashMap::new(),
        workload_outputs: BTreeSet::new(),
    };
    for path in &closure.all_paths {
        if ctx.target_valid.contains(path) {
            snapshot.valid.insert(path.clone());
        }
        if ctx.target_coverage.contains(path) {
            snapshot.coverage.insert(path.clone());
        }
        if let Some(entry) = ctx.relay_narinfos.get(path) {
            snapshot.relay.insert(path.clone(), entry.clone());
        }
        if ctx.workload_outputs.contains(path) {
            snapshot.workload_outputs.insert(path.clone());
        }
    }
    snapshot
}

/// What the cross-request claim table decided for one Archive/Relay path.
enum ClaimResolution {
    /// This request won the claim and uploads the path.
    Ours,
    /// The path landed (or had landed already); treat it as valid.
    Landed,
    /// Another request still holds the claim but it has not landed within
    /// the wait budget; proceed without the path.
    Unavailable,
}

/// Claim `path` for upload, waiting (bounded) for another request's claim and
/// re-claiming once if that claim is released without landing. A `wait()`
/// that returns `false` means "not landed" — never "present".
async fn resolve_claim(claims: &UploadClaims, path: &str, claim_wait: Duration) -> ClaimResolution {
    match claims.claim(path) {
        ClaimOutcome::Won => ClaimResolution::Ours,
        ClaimOutcome::AlreadyDone => ClaimResolution::Landed,
        ClaimOutcome::MustWait => {
            if claims.wait(path, claim_wait).await {
                return ClaimResolution::Landed;
            }
            match claims.claim(path) {
                ClaimOutcome::Won => ClaimResolution::Ours,
                ClaimOutcome::AlreadyDone => ClaimResolution::Landed,
                ClaimOutcome::MustWait => ClaimResolution::Unavailable,
            }
        }
    }
}

/// Upload claims this request has won and not yet completed. Dropping the
/// guard releases whatever is left, so an early return (or panic) cannot
/// leave other requests waiting on a path nobody is uploading anymore.
struct ClaimGuard {
    claims: Arc<UploadClaims>,
    pending: BTreeSet<String>,
}

impl ClaimGuard {
    /// Empty guard over the shared claim table.
    fn new(claims: Arc<UploadClaims>) -> Self {
        Self {
            claims,
            pending: BTreeSet::new(),
        }
    }

    /// Track a claim this request just won.
    fn add(&mut self, path: &str) {
        self.pending.insert(path.to_string());
    }

    /// Mark a held claim as landed.
    fn complete(&mut self, path: &str) {
        if self.pending.remove(path) {
            self.claims.complete(path);
        }
    }

    /// Release a held claim so another request can upload the path; returns
    /// whether this request actually held it.
    fn release(&mut self, path: &str) -> bool {
        if self.pending.remove(path) {
            self.claims.release(path);
            true
        } else {
            false
        }
    }

    /// Release every held claim whose path is not in `keep`.
    fn release_except<'a>(&mut self, keep: impl IntoIterator<Item = &'a str>) {
        let keep: BTreeSet<&str> = keep.into_iter().collect();
        let to_release: Vec<String> = self
            .pending
            .iter()
            .filter(|path| !keep.contains(path.as_str()))
            .cloned()
            .collect();
        for path in to_release {
            self.release(&path);
        }
    }
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        for path in &self.pending {
            self.claims.release(path);
        }
    }
}

/// Lazily opened daemon channel for one request, replaceable mid-request
/// (a refusal or transport error leaves the wire position unknown).
struct ChannelSlot {
    pool: Arc<GatewayPool>,
    current: Option<DaemonChannel>,
}

impl ChannelSlot {
    /// Empty slot over the shared pool; the first [`Self::get`] dials.
    fn new(pool: Arc<GatewayPool>) -> Self {
        Self {
            pool,
            current: None,
        }
    }

    /// The held channel, dialing one if needed. Channel-open failures are
    /// infra errors: one retry after [`CHANNEL_RETRY_DELAY`], then a
    /// [`RequestErrorKind::ChannelOpen`] error for the request outcome.
    async fn get(&mut self) -> std::result::Result<&mut DaemonChannel, RequestError> {
        if self.current.is_none() {
            let channel = match self.pool.open_channel().await {
                Ok(channel) => channel,
                Err(first) => {
                    tracing::debug!(
                        error = %format!("{first:#}"),
                        "daemon channel open failed; retrying once"
                    );
                    tokio::time::sleep(CHANNEL_RETRY_DELAY).await;
                    self.pool.open_channel().await.map_err(|err| {
                        RequestError::new(
                            RequestErrorKind::ChannelOpen,
                            format!("could not open a daemon channel (after one retry): {err:#}"),
                        )
                    })?
                }
            };
            self.current = Some(channel);
        }
        Ok(self
            .current
            .as_mut()
            .expect("channel was just ensured above"))
    }

    /// Drop the held channel (its wire position is unknown); the next
    /// [`Self::get`] dials a fresh one.
    fn discard(&mut self) {
        self.current = None;
    }

    /// Abruptly drop the held channel — disconnect replay. The gateway treats
    /// the channel close as the client going away and cancels its builds.
    fn abandon(&mut self) {
        if let Some(channel) = self.current.take() {
            channel.abandon();
        }
    }
}

/// Outcome of the per-request gap-upload phase.
enum GapUploadOutcome {
    /// Everything supplyable was uploaded (possibly nothing needed it).
    Done,
    /// The daemon refused an upload even after one retry on a fresh channel.
    Rejected(String),
    /// Transport/infra failure — the request cannot proceed normally.
    Error(RequestError),
}

/// Result of one upload (one streamed large item or one batch) after the
/// refusal-retry policy.
enum SendOutcome {
    /// The upload landed; claims completed and the context updated.
    Sent,
    /// Refused twice (original + one retry on a fresh channel).
    Rejected(String),
    /// Transport/infra failure.
    Error(RequestError),
}

/// Result of a single wire attempt of an upload.
enum SendAttempt {
    /// The daemon accepted the entries.
    Sent,
    /// The daemon refused (or a transport error raced a refusal).
    Refused(String),
    /// Transport/infra failure (channel open or non-refusal op error).
    Error(RequestError),
}

/// Upload this request's supply gaps: large relayed items individually
/// first, then the rest as `AddMultipleToStore` batches in the plan's
/// reference-safe order (split only when the entry/byte caps require it).
async fn upload_request_gaps(
    env: &TimelineEnv,
    slot: &mut ChannelSlot,
    plan: &UploadPlan,
    claim_guard: &mut ClaimGuard,
) -> GapUploadOutcome {
    for item in &plan.large {
        let nar = match materialize_item(env, item).await {
            Ok(nar) => nar,
            Err(reason) => {
                tracing::debug!(
                    path = %item.store_path,
                    %reason,
                    "gap upload skipped: payload could not be materialized"
                );
                claim_guard.release(&item.store_path);
                continue;
            }
        };
        match send_upload(env, slot, claim_guard, &[item], vec![nar], true).await {
            SendOutcome::Sent => {}
            SendOutcome::Rejected(message) => return GapUploadOutcome::Rejected(message),
            SendOutcome::Error(error) => return GapUploadOutcome::Error(error),
        }
    }

    let mut pending_items: Vec<&UploadItem> = Vec::new();
    let mut pending_nars: Vec<Vec<u8>> = Vec::new();
    let mut pending_bytes: u64 = 0;
    for item in &plan.batch {
        let nar = match materialize_item(env, item).await {
            Ok(nar) => nar,
            Err(reason) => {
                tracing::debug!(
                    path = %item.store_path,
                    %reason,
                    "gap upload skipped: payload could not be materialized"
                );
                claim_guard.release(&item.store_path);
                continue;
            }
        };
        let nar_len = nar.len() as u64;
        // Inline batch splitting on the same entry/byte caps as
        // `prewarm::split_batches` (kept inline because payloads are
        // materialized as the batch is walked, not planned up front).
        if !pending_items.is_empty()
            && (pending_items.len() >= BATCH_MAX_ENTRIES
                || pending_bytes + nar_len > BATCH_MAX_BYTES)
        {
            match send_upload(
                env,
                slot,
                claim_guard,
                &pending_items,
                std::mem::take(&mut pending_nars),
                false,
            )
            .await
            {
                SendOutcome::Sent => {}
                SendOutcome::Rejected(message) => return GapUploadOutcome::Rejected(message),
                SendOutcome::Error(error) => return GapUploadOutcome::Error(error),
            }
            pending_items.clear();
            pending_bytes = 0;
        }
        pending_items.push(item);
        pending_nars.push(nar);
        pending_bytes += nar_len;
    }
    if !pending_items.is_empty() {
        match send_upload(env, slot, claim_guard, &pending_items, pending_nars, false).await {
            SendOutcome::Sent => {}
            SendOutcome::Rejected(message) => return GapUploadOutcome::Rejected(message),
            SendOutcome::Error(error) => return GapUploadOutcome::Error(error),
        }
    }
    GapUploadOutcome::Done
}

/// Send one upload (a single streamed large item, or one batch). A refusal
/// releases the affected claims, swaps in a fresh channel, and retries
/// exactly once with freshly materialized payloads — the refusal may be
/// genuine or a transport error racing a refusal, and one retry
/// distinguishes a flake from a real rejection. A successful send completes
/// the claims and marks the paths valid in the shared supply context.
async fn send_upload(
    env: &TimelineEnv,
    slot: &mut ChannelSlot,
    claim_guard: &mut ClaimGuard,
    items: &[&UploadItem],
    nars: Vec<Vec<u8>>,
    large: bool,
) -> SendOutcome {
    debug_assert_eq!(items.len(), nars.len());
    debug_assert!(!large || items.len() == 1);
    // Large relayed NARs legitimately take longer than the generic op
    // deadline; mirror the prewarm phase's streaming budget for them.
    let timeout = if large {
        LARGE_UPLOAD_TIMEOUT
    } else {
        env.cfg.op_timeout
    };

    let first_paths: Vec<String> = items.iter().map(|item| item.store_path.clone()).collect();
    let refusal = match send_entries(slot, items, nars, large, timeout).await {
        SendAttempt::Sent => {
            finish_successful_upload(env, claim_guard, &first_paths, &[]).await;
            return SendOutcome::Sent;
        }
        SendAttempt::Refused(message) => message,
        SendAttempt::Error(error) => {
            slot.discard();
            return SendOutcome::Error(error);
        }
    };

    // The wire position is unknown after a refusal, and the claims must not
    // keep other requests waiting while this one retries (or gives up). The
    // retry re-materializes its payloads instead of keeping a second copy of
    // every NAR around for a case this rare.
    slot.discard();
    let mut released: Vec<String> = Vec::new();
    for path in &first_paths {
        if claim_guard.release(path) {
            released.push(path.clone());
        }
    }
    tracing::debug!(
        %refusal,
        paths = first_paths.len(),
        "upload refused; retrying once on a fresh channel"
    );
    let mut retry_items: Vec<&UploadItem> = Vec::new();
    let mut retry_nars: Vec<Vec<u8>> = Vec::new();
    for &item in items {
        match materialize_item(env, item).await {
            Ok(nar) => {
                retry_items.push(item);
                retry_nars.push(nar);
            }
            Err(reason) => {
                tracing::debug!(
                    path = %item.store_path,
                    %reason,
                    "gap upload retry skipped: payload could not be re-materialized"
                );
            }
        }
    }
    if retry_items.is_empty() {
        return SendOutcome::Rejected(refusal);
    }

    let retry_paths: Vec<String> = retry_items
        .iter()
        .map(|item| item.store_path.clone())
        .collect();
    match send_entries(slot, &retry_items, retry_nars, large, timeout).await {
        SendAttempt::Sent => {
            finish_successful_upload(env, claim_guard, &retry_paths, &released).await;
            SendOutcome::Sent
        }
        SendAttempt::Refused(message) => {
            slot.discard();
            SendOutcome::Rejected(message)
        }
        SendAttempt::Error(error) => {
            slot.discard();
            SendOutcome::Error(error)
        }
    }
}

/// One wire attempt of an upload: build the entries from the given payloads
/// and run the matching daemon op.
async fn send_entries(
    slot: &mut ChannelSlot,
    items: &[&UploadItem],
    nars: Vec<Vec<u8>>,
    large: bool,
    timeout: Duration,
) -> SendAttempt {
    let channel = match slot.get().await {
        Ok(channel) => channel,
        Err(error) => return SendAttempt::Error(error),
    };
    let mut entries: Vec<StoreEntry> = Vec::with_capacity(items.len());
    for (item, nar) in items.iter().zip(nars) {
        entries.push(StoreEntry {
            store_path: item.store_path.clone(),
            info: item.info.clone(),
            nar: NarPayload::Bytes(nar),
        });
    }
    let op = if large {
        format!("AddToStoreNar {}", items[0].store_path)
    } else {
        format!("AddMultipleToStore ({} entries)", entries.len())
    };
    let sent = if large {
        let entry = entries
            .pop()
            .expect("a large upload always carries exactly one entry");
        channel.add_to_store_nar(entry, timeout).await
    } else {
        channel.add_multiple_to_store(entries, timeout).await
    };
    match sent {
        Ok(()) => SendAttempt::Sent,
        Err(ReplayClientError::Refused(message)) => SendAttempt::Refused(message),
        Err(err) => SendAttempt::Error(RequestError::new(
            RequestErrorKind::UploadTransport,
            format!("{op} failed: {err}"),
        )),
    }
}

/// Bookkeeping after an upload landed: complete the claims this request
/// still holds for the sent paths, mark claims it had to release for the
/// refusal retry as done when their path was re-sent, and record every sent
/// path as valid on the target.
async fn finish_successful_upload(
    env: &TimelineEnv,
    claim_guard: &mut ClaimGuard,
    sent_paths: &[String],
    released_paths: &[String],
) {
    for path in sent_paths {
        claim_guard.complete(path);
    }
    for path in released_paths {
        if sent_paths.contains(path) {
            env.claims.complete(path);
        }
    }
    {
        // Brief write-lock; the network work is already done.
        let mut ctx = env.ctx.write().await;
        for path in sent_paths {
            ctx.target_valid.insert(path.clone());
        }
    }
    tracing::debug!(paths = sent_paths.len(), "uploaded request supply gap");
}

/// Produce the NAR bytes for one planned upload item, verifying the length
/// against the path-info it will be sent with (a mismatch would desync the
/// upload stream and read as a daemon refusal).
async fn materialize_item(
    env: &TimelineEnv,
    item: &UploadItem,
) -> std::result::Result<Vec<u8>, String> {
    let nar = match &item.payload {
        UploadPayload::DrvText(nar) => nar.clone(),
        UploadPayload::ArchivePath => {
            let archive = Arc::clone(&env.archive);
            let store_path = item.store_path.clone();
            match tokio::task::spawn_blocking(move || archive.dump_nar(&store_path)).await {
                Ok(Ok(nar)) => nar,
                Ok(Err(err)) => {
                    return Err(format!(
                        "failed to NAR-serialize the embedded path: {err:#}"
                    ));
                }
                Err(err) => {
                    return Err(format!(
                        "the archive NAR dump task panicked or was cancelled: {err}"
                    ));
                }
            }
        }
        UploadPayload::Relay {
            substituter_url,
            narinfo,
        } => {
            let Some(substituter) = env
                .src_substituters
                .iter()
                .find(|substituter| substituter.url() == *substituter_url)
            else {
                return Err(format!(
                    "no configured recording substituter matches {substituter_url}"
                ));
            };
            match tokio::time::timeout(env.cfg.op_timeout, substituter.fetch_nar(narinfo)).await {
                Ok(Ok(nar)) => nar,
                Ok(Err(err)) => return Err(format!("relay fetch failed: {err:#}")),
                Err(_elapsed) => {
                    return Err(format!(
                        "relay fetch timed out after {}s",
                        env.cfg.op_timeout.as_secs()
                    ));
                }
            }
        }
    };
    if nar.len() as u64 != item.info.nar_size {
        return Err(format!(
            "materialized NAR is {} bytes but the path-info declares {}",
            nar.len(),
            item.info.nar_size
        ));
    }
    Ok(nar)
}

/// Build deadline for one request: twice the slowest recorded duration among
/// its build records, clamped to `[build_timeout_floor, build_timeout_cap]`;
/// the floor alone when no matching record carries a duration.
fn build_timeout_for(
    builds: &HashMap<(i64, String), BuildRecord>,
    request: &ReplayRequest,
    cfg: &TimelineConfig,
) -> Duration {
    let slowest = request
        .paths
        .iter()
        .filter_map(|(drv_path, _outputs)| {
            builds
                .get(&(request.ssh_session_id, drv_path.clone()))
                .and_then(|record| record.duration_s)
        })
        .fold(None::<f64>, |acc, duration| {
            Some(acc.map_or(duration, |current| current.max(duration)))
        });
    match slowest {
        Some(duration) if duration.is_finite() && duration > 0.0 => {
            // Cap before converting so an absurd recorded duration cannot
            // overflow the conversion; then apply the floor.
            let capped = (2.0 * duration).min(cfg.build_timeout_cap.as_secs_f64());
            Duration::from_secs_f64(capped).max(cfg.build_timeout_floor)
        }
        _ => cfg.build_timeout_floor,
    }
}

/// One `BuildPathsWithResults` submission, optionally raced against the
/// recorded disconnect deadline.
enum BuildAttempt {
    /// The daemon answered with per-path results (count not yet verified).
    Results(Vec<KeyedBuildResult>),
    /// The disconnect deadline passed first; the channel was dropped abruptly.
    Disconnected,
    /// The build could not be driven to completion.
    Error(RequestError),
}

/// Submit the request's derived paths and wait for results, replaying a
/// recorded client disconnect when `disconnect_deadline` is set: if the
/// deadline (anchored at dispatch) passes before the daemon answers, the
/// channel is dropped abruptly (the gateway then cancels the session's
/// builds, like the recorded client going away did).
async fn run_build_attempt(
    slot: &mut ChannelSlot,
    derived: &[String],
    build_timeout: Duration,
    disconnect_deadline: Option<tokio::time::Instant>,
) -> BuildAttempt {
    let channel = match slot.get().await {
        Ok(channel) => channel,
        Err(error) => return BuildAttempt::Error(error),
    };
    let outcome = match disconnect_deadline {
        None => Some(
            channel
                .build_paths_with_results(derived, build_timeout)
                .await,
        ),
        Some(deadline) => {
            // The supply phases may already have eaten into the recorded
            // disconnect gap; the build is still always submitted, with the
            // floor applied to whatever delay remains.
            let remaining = deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .max(DISCONNECT_FLOOR);
            // Scope the build future so its borrow of the channel ends before
            // the disconnect path takes the channel out of the slot.
            let build = channel.build_paths_with_results(derived, build_timeout);
            tokio::pin!(build);
            tokio::select! {
                result = &mut build => Some(result),
                () = tokio::time::sleep(remaining) => None,
            }
        }
    };
    match outcome {
        None => {
            slot.abandon();
            BuildAttempt::Disconnected
        }
        Some(Ok(results)) => BuildAttempt::Results(results),
        Some(Err(ReplayClientError::Timeout(deadline))) => {
            slot.discard();
            BuildAttempt::Error(RequestError::new(
                RequestErrorKind::BuildTimeout,
                format!("build timed out after {}s", deadline.as_secs()),
            ))
        }
        Some(Err(ReplayClientError::Refused(message))) => {
            slot.discard();
            BuildAttempt::Error(RequestError::new(
                RequestErrorKind::BuildRefused,
                format!("daemon refused BuildPathsWithResults: {message}"),
            ))
        }
        Some(Err(err)) => {
            slot.discard();
            BuildAttempt::Error(RequestError::new(
                RequestErrorKind::BuildTransport,
                format!("BuildPathsWithResults failed: {err}"),
            ))
        }
    }
}

/// Zip one build attempt's results onto the outcome positions they were
/// submitted for (submission order). The daemon answering with a different
/// count than submitted is a protocol-level fault, not a build failure.
fn apply_build_results(
    results: &mut [DerivedOutcome],
    positions: &[usize],
    keyed: Vec<KeyedBuildResult>,
) -> std::result::Result<(), RequestError> {
    if keyed.len() != positions.len() {
        return Err(RequestError::new(
            RequestErrorKind::ResultCountMismatch,
            format!(
                "daemon returned {} results for {} derived paths",
                keyed.len(),
                positions.len()
            ),
        ));
    }
    for (&position, keyed_result) in positions.iter().zip(keyed) {
        results[position].result = Some(keyed_result.result);
    }
    Ok(())
}

/// Positions whose recorded outcome was a successful build but whose replay
/// result is currently a failure — the candidates the confirmation loop
/// re-runs.
fn regression_positions(
    builds: &HashMap<(i64, String), BuildRecord>,
    request: &ReplayRequest,
    results: &[DerivedOutcome],
) -> Vec<usize> {
    results
        .iter()
        .enumerate()
        .filter(|(_, derived)| {
            let recorded_success = builds
                .get(&(request.ssh_session_id, derived.drv_path.clone()))
                .is_some_and(|record| record.status == prod_status::BUILT);
            let replay_failed = derived
                .result
                .as_ref()
                .is_some_and(|result| !result.status.is_success());
            recorded_success && replay_failed
        })
        .map(|(position, _)| position)
        .collect()
}

/// Collect replay-side NAR hashes for every actually-rebuilt result: each
/// declared output path of the drv is `QueryPathInfo`'d and its NAR hash
/// recorded as lowercase hex. Already-valid and substituted results are left
/// alone (the comparison treats them as skips, not rebuilds).
async fn collect_output_hashes(
    env: &TimelineEnv,
    slot: &mut ChannelSlot,
    closure: &Closure,
    results: &mut [DerivedOutcome],
) -> std::result::Result<(), RequestError> {
    let nodes: HashMap<&str, &ClosureNode> = closure
        .topo
        .iter()
        .map(|node| (node.drv_path.as_str(), node))
        .collect();
    for derived in results.iter_mut() {
        let Some(result) = &derived.result else {
            continue;
        };
        if result.status != BuildStatus::Built {
            continue;
        }
        let Some(node) = nodes.get(derived.drv_path.as_str()) else {
            continue;
        };
        for (output_name, output_path) in &node.outputs {
            if output_path.is_empty() {
                // Floating/CA outputs have no declared path to query.
                continue;
            }
            let channel = match slot.get().await {
                Ok(channel) => channel,
                Err(error) => return Err(error),
            };
            match channel
                .query_path_info(output_path, env.cfg.op_timeout)
                .await
            {
                Ok(Some(info)) => {
                    derived
                        .replay_nar_hashes
                        .insert(output_name.clone(), hex::encode(&info.nar_hash));
                }
                Ok(None) => {
                    tracing::debug!(
                        path = %output_path,
                        "built output has no path info on the target; no replay hash collected"
                    );
                }
                Err(err) => {
                    return Err(RequestError::new(
                        RequestErrorKind::Collect,
                        format!("QueryPathInfo {output_path} failed: {err}"),
                    ));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    const DEP_DRV: &str = "/nix/store/a1111111111111111111111111111111-dep.drv";

    fn fixture() -> PathBuf {
        // Runtime env var, not compile-time env!() — see `fixture()` in archive.rs's tests.
        PathBuf::from(
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo/nextest"),
        )
        .join("tests/fixtures/replay/basic")
    }

    fn open_fixture() -> ReplayArchive {
        ReplayArchive::open(&fixture()).unwrap()
    }

    /// Minimal request for fabricated-schedule cases.
    fn request(session: i64, offset_s: f64, drv: &str) -> ReplayRequest {
        ReplayRequest {
            ssh_session_id: session,
            offset_s,
            paths: vec![(drv.to_string(), vec!["out".to_string()])],
        }
    }

    /// Minimal recorded client-disconnect build record.
    fn disconnect_record(session: i64, drv: &str, stop_offset_s: Option<f64>) -> BuildRecord {
        BuildRecord {
            stop_offset_s,
            ..build_record(session, drv, prod_status::CLIENT_DISCONNECT, None)
        }
    }

    /// Recorded build record with an arbitrary status and optional duration.
    fn build_record(session: i64, drv: &str, status: i32, duration_s: Option<f64>) -> BuildRecord {
        BuildRecord {
            ssh_session_id: session,
            drv_path: drv.to_string(),
            status,
            status_msg: None,
            duration_s,
            stop_offset_s: None,
            outputs: BTreeMap::new(),
        }
    }

    /// Replay-side derived outcome with just a drv path and an optional
    /// build result.
    fn derived_outcome(drv: &str, result: Option<BuildResult>) -> DerivedOutcome {
        DerivedOutcome {
            drv_path: drv.to_string(),
            outputs: vec!["out".to_string()],
            result,
            replay_nar_hashes: BTreeMap::new(),
            upload_rejected: None,
        }
    }

    #[test]
    fn build_schedule_sorts_limits_and_scales() {
        let archive = open_fixture();

        // Fixture offsets 0.25/2.0/5.5/9.0 at speedup 2.0: due times halve,
        // order is by recorded offset, indices follow the schedule order.
        let schedule = build_schedule(archive.requests(), archive.builds(), 2.0, None, true);
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
        let sessions: Vec<i64> = schedule
            .iter()
            .map(|entry| entry.request.ssh_session_id)
            .collect();
        assert_eq!(sessions, vec![10, 13, 11, 12]);
        let indices: Vec<usize> = schedule.iter().map(|entry| entry.index).collect();
        assert_eq!(indices, vec![0, 1, 2, 3]);

        // --limit keeps the first N by offset.
        let limited = build_schedule(archive.requests(), archive.builds(), 2.0, Some(2), true);
        assert_eq!(limited.len(), 2);
        let limited_sessions: Vec<i64> = limited
            .iter()
            .map(|entry| entry.request.ssh_session_id)
            .collect();
        assert_eq!(limited_sessions, vec![10, 13]);

        // A fabricated negative offset clamps to a zero due time and sorts
        // ahead of everything else.
        let mut requests = archive.requests().to_vec();
        requests.push(request(99, -3.5, DEP_DRV));
        let schedule = build_schedule(&requests, archive.builds(), 2.0, None, false);
        assert_eq!(schedule[0].request.ssh_session_id, 99);
        assert_eq!(schedule[0].due, Duration::ZERO);
    }

    #[test]
    fn disconnect_after_uses_stop_offset() {
        let archive = open_fixture();

        // Session 12's record is a client disconnect at stop offset 11.0 for
        // a request at offset 9.0: (11 - 9) / 2.0 = 1s. Everything else has
        // no disconnect record and stays None.
        let schedule = build_schedule(archive.requests(), archive.builds(), 2.0, None, true);
        let timers: Vec<Option<Duration>> = schedule
            .iter()
            .map(|entry| entry.disconnect_after)
            .collect();
        assert_eq!(timers, vec![None, None, None, Some(Duration::from_secs(1))]);

        // Disconnect replay disabled: no timers at all.
        let schedule = build_schedule(archive.requests(), archive.builds(), 2.0, None, false);
        assert!(
            schedule
                .iter()
                .all(|entry| entry.disconnect_after.is_none())
        );

        // A disconnect record without stop_offset_s falls back to 60s scaled
        // by the speedup.
        let requests = vec![request(50, 4.0, DEP_DRV)];
        let mut builds = HashMap::new();
        builds.insert(
            (50_i64, DEP_DRV.to_string()),
            disconnect_record(50, DEP_DRV, None),
        );
        let schedule = build_schedule(&requests, &builds, 2.0, None, true);
        assert_eq!(schedule[0].disconnect_after, Some(Duration::from_secs(30)));

        // The 1s floor holds when the recorded gap is tiny and the speedup is
        // high.
        let requests = vec![request(51, 5.0, DEP_DRV)];
        let mut builds = HashMap::new();
        builds.insert(
            (51_i64, DEP_DRV.to_string()),
            disconnect_record(51, DEP_DRV, Some(5.001)),
        );
        let schedule = build_schedule(&requests, &builds, 100.0, None, true);
        assert_eq!(schedule[0].disconnect_after, Some(Duration::from_secs(1)));
    }

    #[test]
    fn format_derived_forms() {
        let drv = DEP_DRV;
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
    fn tracker_snapshot_reports_oldest() {
        let tracker = InFlightTracker::new();
        let (count, oldest) = tracker.snapshot();
        assert_eq!(count, 0);
        assert!(oldest.is_none());

        // Staggered entry times: the first entry is the oldest.
        tracker.set(7, 100, RequestStage::Probe);
        std::thread::sleep(Duration::from_millis(15));
        tracker.set(3, 101, RequestStage::Build);
        std::thread::sleep(Duration::from_millis(15));
        tracker.set(9, 102, RequestStage::Waiting);

        let (count, oldest) = tracker.snapshot();
        assert_eq!(count, 3);
        let (index, session, stage, age) = oldest.expect("three entries are in flight");
        assert_eq!(index, 7);
        assert_eq!(session, 100);
        assert_eq!(stage, RequestStage::Probe);
        assert!(
            age >= Duration::from_millis(25),
            "oldest age must reflect the first insertion, got {age:?}"
        );

        // Re-setting an entry's stage resets its clock, so the next-oldest
        // entry takes over; removing everything empties the tracker.
        tracker.set(7, 100, RequestStage::Collect);
        let (_, oldest) = tracker.snapshot();
        assert_eq!(oldest.expect("still three in flight").0, 3);
        tracker.remove(3);
        tracker.remove(7);
        tracker.remove(9);
        assert_eq!(tracker.snapshot().0, 0);

        // Waiting entries are reported as the oldest only when nothing is
        // actually executing — an older Waiting entry must not hide a
        // younger executing one.
        tracker.set(1, 200, RequestStage::Waiting);
        std::thread::sleep(Duration::from_millis(15));
        tracker.set(2, 201, RequestStage::Build);
        let (count, oldest) = tracker.snapshot();
        assert_eq!(count, 2);
        assert_eq!(oldest.expect("two entries in flight").0, 2);
        tracker.remove(2);
        let (_, oldest) = tracker.snapshot();
        assert_eq!(oldest.expect("only the waiting entry remains").0, 1);
        tracker.remove(1);
    }

    #[test]
    fn regression_positions_pick_recorded_success_with_replay_failure() {
        let session = 1_i64;
        // One drv per recorded/replayed combination the confirmation loop
        // must distinguish.
        let recorded_built_failed = "/nix/store/r1111111111111111111111111111111-a.drv";
        let recorded_failed_failed = "/nix/store/r2222222222222222222222222222222-b.drv";
        let recorded_disconnect_failed = "/nix/store/r3333333333333333333333333333333-c.drv";
        let recorded_built_succeeded = "/nix/store/r4444444444444444444444444444444-d.drv";
        let recorded_built_no_result = "/nix/store/r5555555555555555555555555555555-e.drv";
        let unrecorded_failed = "/nix/store/r6666666666666666666666666666666-f.drv";

        let mut builds = HashMap::new();
        for (drv, status) in [
            (recorded_built_failed, prod_status::BUILT),
            (recorded_failed_failed, 1),
            (recorded_disconnect_failed, prod_status::CLIENT_DISCONNECT),
            (recorded_built_succeeded, prod_status::BUILT),
            (recorded_built_no_result, prod_status::BUILT),
        ] {
            builds.insert(
                (session, drv.to_string()),
                build_record(session, drv, status, None),
            );
        }
        let request = ReplayRequest {
            ssh_session_id: session,
            offset_s: 0.0,
            paths: [
                recorded_built_failed,
                recorded_failed_failed,
                recorded_disconnect_failed,
                recorded_built_succeeded,
                recorded_built_no_result,
                unrecorded_failed,
            ]
            .iter()
            .map(|drv| (drv.to_string(), vec!["out".to_string()]))
            .collect(),
        };
        let failed = || Some(BuildResult::failure(BuildStatus::PermanentFailure, "boom"));
        let results = vec![
            derived_outcome(recorded_built_failed, failed()),
            derived_outcome(recorded_failed_failed, failed()),
            derived_outcome(recorded_disconnect_failed, failed()),
            derived_outcome(recorded_built_succeeded, Some(BuildResult::success())),
            derived_outcome(recorded_built_no_result, None),
            derived_outcome(unrecorded_failed, failed()),
        ];

        // Only the recorded-success drv whose replay actually failed is a
        // confirmation candidate.
        assert_eq!(regression_positions(&builds, &request, &results), vec![0]);

        // Nothing to confirm when every replay result matches or is absent.
        let all_clear = vec![
            derived_outcome(recorded_built_failed, Some(BuildResult::success())),
            derived_outcome(recorded_failed_failed, failed()),
        ];
        let request_clear = ReplayRequest {
            ssh_session_id: session,
            offset_s: 0.0,
            paths: vec![
                (recorded_built_failed.to_string(), vec!["out".to_string()]),
                (recorded_failed_failed.to_string(), vec!["out".to_string()]),
            ],
        };
        assert!(regression_positions(&builds, &request_clear, &all_clear).is_empty());
    }

    #[test]
    fn build_timeout_scales_clamps_and_falls_back() {
        let cfg = TimelineConfig::default();
        let session = 7_i64;
        let drv_a = "/nix/store/t1111111111111111111111111111111-a.drv";
        let drv_b = "/nix/store/t2222222222222222222222222222222-b.drv";

        let single = |duration: Option<f64>| {
            let mut builds = HashMap::new();
            builds.insert(
                (session, drv_a.to_string()),
                build_record(session, drv_a, prod_status::BUILT, duration),
            );
            build_timeout_for(&builds, &request(session, 0.0, drv_a), &cfg)
        };

        // Twice the recorded duration when that lands between the bounds.
        assert_eq!(single(Some(1800.0)), Duration::from_secs(3600));
        // Short recorded builds clamp up to the floor…
        assert_eq!(single(Some(4.2)), cfg.build_timeout_floor);
        // …massive ones clamp down to the cap…
        assert_eq!(single(Some(100_000.0)), cfg.build_timeout_cap);
        // …and a record without a duration falls back to the floor,
        assert_eq!(single(None), cfg.build_timeout_floor);
        // as does a request with no matching record at all.
        assert_eq!(
            build_timeout_for(&HashMap::new(), &request(session, 0.0, drv_a), &cfg),
            cfg.build_timeout_floor
        );

        // Several derived paths: the slowest recorded duration wins.
        let mut builds = HashMap::new();
        builds.insert(
            (session, drv_a.to_string()),
            build_record(session, drv_a, prod_status::BUILT, Some(900.0)),
        );
        builds.insert(
            (session, drv_b.to_string()),
            build_record(session, drv_b, prod_status::BUILT, Some(2000.0)),
        );
        let request = ReplayRequest {
            ssh_session_id: session,
            offset_s: 0.0,
            paths: vec![
                (drv_a.to_string(), vec!["out".to_string()]),
                (drv_b.to_string(), vec!["out".to_string()]),
            ],
        };
        assert_eq!(
            build_timeout_for(&builds, &request, &cfg),
            Duration::from_secs(4000)
        );
    }
}
