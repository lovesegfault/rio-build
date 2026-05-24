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

/// Pause before the single retry of a failed daemon-channel open.
const CHANNEL_RETRY_DELAY: Duration = Duration::from_secs(5);

/// Emit a timeline progress line every this many completed requests.
const PROGRESS_EVERY_REQUESTS: usize = 25;

/// Disconnect delay assumed when a recorded disconnect carries no
/// `stop_offset_s` (scaled by the speedup like a recorded one).
const DEFAULT_DISCONNECT_DELAY_S: f64 = 60.0;

/// Lower bound on the disconnect timer so a high speedup cannot turn it into
/// an instant drop before the build is even submitted.
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
    /// Scheduled; waiting for its due time and an admission slot.
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
    /// Finished (kept for consumers that want a terminal stage label).
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
    /// line's "oldest" column.
    pub fn snapshot(&self) -> (usize, Option<(usize, i64, RequestStage, Duration)>) {
        let entries = self.lock();
        let oldest = entries
            .iter()
            .min_by_key(|(_, (_, since, _))| *since)
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

/// Everything the replay learned about one scheduled request.
#[derive(Debug, Clone)]
pub struct RequestOutcome {
    /// Schedule index (matches [`ScheduledRequest::index`]).
    pub index: usize,
    /// The recorded request that was replayed.
    pub request: ReplayRequest,
    /// One entry per requested derived path, in request order.
    pub results: Vec<DerivedOutcome>,
    /// Build attempts made (1 = no confirmation retries needed).
    pub attempts: u32,
    /// True when the request was replayed as a recorded client disconnect.
    pub disconnected: bool,
    /// Transport/infra error that prevented the request from completing
    /// normally (not a build failure).
    pub error: Option<String>,
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
            env.tracker.set(index, session, RequestStage::Waiting);

            let due_at = start + scheduled.due;
            tokio::time::sleep_until(due_at).await;
            // FIFO admission: requests that came due earlier get a session
            // slot first; lateness measures schedule slip from backpressure.
            let permit = match Arc::clone(&admission).acquire_owned().await {
                Ok(permit) => permit,
                Err(_closed) => {
                    // The semaphore is never closed; degrade to an errored
                    // outcome rather than panicking the task.
                    env.tracker.remove(index);
                    return RequestOutcome {
                        index,
                        request: scheduled.request.clone(),
                        results: skeleton_results(&scheduled.request),
                        attempts: 0,
                        disconnected: false,
                        error: Some("admission semaphore closed unexpectedly".to_string()),
                        dispatch_lateness: Duration::ZERO,
                    };
                }
            };
            let dispatch_lateness = tokio::time::Instant::now().saturating_duration_since(due_at);

            let outcome = execute_request(&env, &scheduled, dispatch_lateness).await;
            drop(permit);
            env.tracker.remove(index);
            outcome
        });
        spawned.insert(handle.id(), meta);
    }

    let mut outcomes: Vec<RequestOutcome> = Vec::with_capacity(total);
    while let Some(joined) = tasks.join_next_with_id().await {
        match joined {
            Ok((_id, outcome)) => outcomes.push(outcome),
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
                        error: Some(format!("request task panicked: {join_error}")),
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
async fn execute_request(
    env: &TimelineEnv,
    scheduled: &ScheduledRequest,
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
                outcome.error = Some(format!("closure walk failed: {err:#}"));
                return outcome;
            }
            Err(err) => {
                outcome.error = Some(format!(
                    "closure walk task panicked or was cancelled: {err}"
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
            Err(message) => {
                outcome.error = Some(message);
                return outcome;
            }
        };
        match channel.query_valid_paths(chunk, env.cfg.op_timeout).await {
            Ok(found) => probed_valid.extend(found),
            Err(err) => {
                outcome.error = Some(format!("target validity probe failed: {err}"));
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
            outcome.error = Some(format!("upload planning failed: {err:#}"));
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
        GapUploadOutcome::Error(message) => {
            outcome.error = Some(message);
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
    let build_timeout = build_timeout_for(env, request);

    let mut attempts: u32 = 1;
    match run_build_attempt(
        &mut slot,
        &derived,
        build_timeout,
        scheduled.disconnect_after,
    )
    .await
    {
        BuildAttempt::Disconnected => {
            outcome.disconnected = true;
            outcome.attempts = attempts;
            tracing::debug!(session, "request replayed as a recorded client disconnect");
            return outcome;
        }
        BuildAttempt::Error(message) => {
            outcome.error = Some(message);
            outcome.attempts = attempts;
            return outcome;
        }
        BuildAttempt::Results(results) => {
            if results.len() != derived.len() {
                outcome.error = Some(format!(
                    "daemon returned {} results for {} derived paths",
                    results.len(),
                    derived.len()
                ));
                outcome.attempts = attempts;
                return outcome;
            }
            for (derived_outcome, keyed) in outcome.results.iter_mut().zip(results) {
                derived_outcome.result = Some(keyed.result);
            }
        }
    }

    // Confirmation: a recorded-success build that failed on replay gets up to
    // `confirm_regressions` total attempts (fresh channel each time) before
    // the failure is allowed to stand as a regression.
    let max_attempts = env.cfg.confirm_regressions.max(1);
    loop {
        let targets = regression_positions(&env.archive, request, &outcome.results);
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
        match run_build_attempt(&mut slot, &derived, build_timeout, None).await {
            // No disconnect timer is passed for confirmation attempts, so a
            // Disconnected outcome cannot occur; treat it like one anyway
            // rather than panicking if that ever changes.
            BuildAttempt::Disconnected => {
                outcome.disconnected = true;
                break;
            }
            BuildAttempt::Error(message) => {
                outcome.error = Some(message);
                outcome.attempts = attempts;
                return outcome;
            }
            BuildAttempt::Results(results) => {
                if results.len() != derived.len() {
                    outcome.error = Some(format!(
                        "daemon returned {} results for {} derived paths",
                        results.len(),
                        derived.len()
                    ));
                    outcome.attempts = attempts;
                    return outcome;
                }
                // Only the regression candidates take the re-run's result;
                // everything else keeps its first outcome (a path already
                // built once would only come back as already valid now).
                for position in targets {
                    outcome.results[position].result = Some(results[position].result.clone());
                }
            }
        }
    }
    outcome.attempts = attempts;

    // ---- Collect output hashes ----------------------------------------------
    env.tracker.set(index, session, RequestStage::Collect);
    if let Err(message) =
        collect_output_hashes(env, &mut slot, &closure, &mut outcome.results).await
    {
        outcome.error = Some(message);
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
    /// infra errors: one retry after [`CHANNEL_RETRY_DELAY`], then an error
    /// string for the request outcome.
    async fn get(&mut self) -> std::result::Result<&mut DaemonChannel, String> {
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
                        format!("could not open a daemon channel (after one retry): {err:#}")
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
    Error(String),
}

/// Result of one wire upload (one streamed large item or one batch).
enum SendOutcome {
    /// The upload landed; claims completed and the context updated.
    Sent,
    /// Refused twice (original + one retry on a fresh channel).
    Rejected(String),
    /// Transport/infra failure.
    Error(String),
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
            SendOutcome::Error(message) => return GapUploadOutcome::Error(message),
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
                SendOutcome::Error(message) => return GapUploadOutcome::Error(message),
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
            SendOutcome::Error(message) => return GapUploadOutcome::Error(message),
        }
    }
    GapUploadOutcome::Done
}

/// Send one upload (a single streamed large item, or one batch). A refusal
/// releases the affected claims, swaps in a fresh channel, and retries
/// exactly once — the refusal may be genuine or a transport error racing a
/// refusal, and one retry distinguishes a flake from a real rejection. A
/// successful send completes the claims and marks the paths valid in the
/// shared supply context.
async fn send_upload(
    env: &TimelineEnv,
    slot: &mut ChannelSlot,
    claim_guard: &mut ClaimGuard,
    items: &[&UploadItem],
    mut nars: Vec<Vec<u8>>,
    large: bool,
) -> SendOutcome {
    debug_assert_eq!(items.len(), nars.len());
    debug_assert!(!large || items.len() == 1);
    let paths: Vec<String> = items.iter().map(|item| item.store_path.clone()).collect();
    // Claims released because of a first refusal; completed after all if the
    // retry lands the upload (the path is on the target then).
    let mut released_for_retry: Vec<String> = Vec::new();
    let mut last_refusal: Option<String> = None;

    for attempt in 0..2u8 {
        let channel = match slot.get().await {
            Ok(channel) => channel,
            Err(message) => return SendOutcome::Error(message),
        };
        // Per-request gap payloads are small (the bulk went through prewarm),
        // so the first attempt clones the NAR bytes to keep a refusal retry
        // possible without re-materializing; the second attempt moves them.
        let mut entries: Vec<StoreEntry> = Vec::with_capacity(items.len());
        for (position, item) in items.iter().enumerate() {
            let nar = if attempt == 0 {
                nars[position].clone()
            } else {
                std::mem::take(&mut nars[position])
            };
            entries.push(StoreEntry {
                store_path: item.store_path.clone(),
                info: item.info.clone(),
                nar: NarPayload::Bytes(nar),
            });
        }
        let op = if large {
            format!("AddToStoreNar {}", paths[0])
        } else {
            format!("AddMultipleToStore ({} entries)", entries.len())
        };
        let sent = if large {
            let entry = entries
                .pop()
                .expect("a large upload always carries exactly one entry");
            channel.add_to_store_nar(entry, env.cfg.op_timeout).await
        } else {
            channel
                .add_multiple_to_store(entries, env.cfg.op_timeout)
                .await
        };
        match sent {
            Ok(()) => {
                for path in &paths {
                    claim_guard.complete(path);
                }
                for path in &released_for_retry {
                    env.claims.complete(path);
                }
                {
                    // Brief write-lock; the network work is already done.
                    let mut ctx = env.ctx.write().await;
                    for path in &paths {
                        ctx.target_valid.insert(path.clone());
                    }
                }
                tracing::debug!(paths = paths.len(), "uploaded request supply gap");
                return SendOutcome::Sent;
            }
            Err(ReplayClientError::Refused(message)) => {
                // The wire position is unknown after a refusal, and the
                // claims must not keep other requests waiting while this one
                // retries (or gives up).
                slot.discard();
                for path in &paths {
                    if claim_guard.release(path) {
                        released_for_retry.push(path.clone());
                    }
                }
                if attempt == 0 {
                    tracing::debug!(%op, %message, "upload refused; retrying once on a fresh channel");
                }
                last_refusal = Some(message);
            }
            Err(err) => {
                slot.discard();
                return SendOutcome::Error(format!("{op} failed: {err}"));
            }
        }
    }
    SendOutcome::Rejected(
        last_refusal.unwrap_or_else(|| "upload refused by the daemon".to_string()),
    )
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
fn build_timeout_for(env: &TimelineEnv, request: &ReplayRequest) -> Duration {
    let slowest = request
        .paths
        .iter()
        .filter_map(|(drv_path, _outputs)| {
            env.archive
                .builds()
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
            let capped = (2.0 * duration).min(env.cfg.build_timeout_cap.as_secs_f64());
            Duration::from_secs_f64(capped).max(env.cfg.build_timeout_floor)
        }
        _ => env.cfg.build_timeout_floor,
    }
}

/// One `BuildPathsWithResults` submission, optionally raced against the
/// recorded disconnect timer.
enum BuildAttempt {
    /// The daemon answered with per-path results (count not yet verified).
    Results(Vec<KeyedBuildResult>),
    /// The disconnect timer fired first; the channel was dropped abruptly.
    Disconnected,
    /// The build could not be driven to completion.
    Error(String),
}

/// Submit the request's derived paths and wait for results, replaying a
/// recorded client disconnect when `disconnect_after` is set: if the timer
/// fires before the daemon answers, the channel is dropped abruptly (the
/// gateway then cancels the session's builds, like the recorded client going
/// away did).
async fn run_build_attempt(
    slot: &mut ChannelSlot,
    derived: &[String],
    build_timeout: Duration,
    disconnect_after: Option<Duration>,
) -> BuildAttempt {
    let op_timeout = build_timeout;
    let channel = match slot.get().await {
        Ok(channel) => channel,
        Err(message) => return BuildAttempt::Error(message),
    };
    let outcome = match disconnect_after {
        None => Some(channel.build_paths_with_results(derived, op_timeout).await),
        Some(after) => {
            // Scope the build future so its borrow of the channel ends before
            // the disconnect path takes the channel out of the slot.
            let build = channel.build_paths_with_results(derived, op_timeout);
            tokio::pin!(build);
            tokio::select! {
                result = &mut build => Some(result),
                () = tokio::time::sleep(after) => None,
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
            BuildAttempt::Error(format!("build timed out after {}s", deadline.as_secs()))
        }
        Some(Err(err)) => {
            slot.discard();
            BuildAttempt::Error(format!("BuildPathsWithResults failed: {err}"))
        }
    }
}

/// Positions whose recorded outcome was a successful build but whose replay
/// result is currently a failure — the candidates the confirmation loop
/// re-runs.
fn regression_positions(
    archive: &ReplayArchive,
    request: &ReplayRequest,
    results: &[DerivedOutcome],
) -> Vec<usize> {
    results
        .iter()
        .enumerate()
        .filter(|(_, derived)| {
            let recorded_success = archive
                .builds()
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
) -> std::result::Result<(), String> {
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
                Err(message) => return Err(message),
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
                Err(err) => return Err(format!("QueryPathInfo {output_path} failed: {err}")),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    const DEP_DRV: &str = "/nix/store/a1111111111111111111111111111111-dep.drv";

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replay/basic")
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
            ssh_session_id: session,
            drv_path: drv.to_string(),
            status: prod_status::CLIENT_DISCONNECT,
            status_msg: None,
            duration_s: None,
            stop_offset_s,
            outputs: BTreeMap::new(),
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
    }
}
